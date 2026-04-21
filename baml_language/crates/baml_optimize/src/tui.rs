#![allow(clippy::print_stdout, clippy::print_stderr)]
//! TUI Visualization for GEPA Optimization
//!
//! Provides a real-time terminal user interface for visualizing the GEPA
//! optimization process, showing trials, candidates, and their metrics.
//!
//! The left panel shows candidates as cards arranged in rows by iteration.
//! Multiple candidates in the same iteration appear side-by-side.
//!
//! Ported from `engine/baml-runtime/src/optimize/tui.rs`. Data access goes
//! through [`Storage`] instead of the engine's `OptimizationStorage`. Where
//! the engine had per-check scores, this port surfaces the new crate's
//! per-testset pass rates (same shape, different naming).

use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Wrap,
    },
};

use crate::candidate::{Candidate, CandidateMethod, CandidateScores};
use crate::storage::{ObjectiveConfig, Storage, write_apply_request};

/// Colors for the TUI (optimized for dark/black terminal backgrounds)
const ACCENT_COLOR: Color = Color::Rgb(180, 100, 255); // Bright purple
const HEADER_COLOR: Color = Color::Cyan;
const SCORE_GOOD: Color = Color::Rgb(100, 255, 100); // Bright green
const SCORE_MED: Color = Color::Rgb(255, 220, 100); // Bright yellow
const SCORE_BAD: Color = Color::Rgb(255, 100, 100); // Bright red
const CARD_BORDER: Color = Color::Rgb(100, 100, 120);
const CARD_SELECTED_BORDER: Color = Color::Rgb(180, 100, 255);

/// A row of candidates (all from the same iteration).
#[derive(Clone, Debug)]
pub struct CandidateRow {
    pub iteration: usize,
    pub candidate_ids: Vec<usize>,
}

/// Optimization run status shown in the header.
#[derive(Clone, Debug, PartialEq)]
pub enum OptimizationStatus {
    Running {
        /// 1-indexed iteration currently in progress.
        display_iteration: usize,
        total_trials: usize,
    },
    Completed,
    Failed {
        message: String,
    },
    Unknown,
}

/// Main application state for the TUI.
pub struct App {
    candidates: Vec<Candidate>,
    rows: Vec<CandidateRow>,
    selected_row: usize,
    selected_col: usize,
    scroll_offset: usize,
    prompt_scroll: u16,
    should_quit: bool,
    storage_path: String,
    function_name: String,
    id_to_index: HashMap<usize, usize>,
    objectives: Vec<ObjectiveConfig>,
    pareto_frontier: Vec<usize>,
    live_mode: bool,
    /// Only set in live mode — the path we reload from each tick.
    storage_path_for_reload: Option<PathBuf>,
    status: OptimizationStatus,
    total_trials: usize,
    /// Set when the user presses Enter in live mode to apply a candidate.
    apply_candidate_id: Option<usize>,
}

impl App {
    /// Build an App by loading everything currently on disk for `storage`.
    pub fn from_storage(storage: &Storage) -> Result<Self> {
        let candidates = storage
            .load_candidates()
            .context("Failed to load candidates")?;

        let config = storage.load_config().ok();
        let function_name = config
            .as_ref()
            .map(|c| c.function_name.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let objectives = config
            .as_ref()
            .map(|c| c.objectives.clone())
            .unwrap_or_default();
        let total_trials = config
            .as_ref()
            .map(|c| c.max_iterations as usize)
            .unwrap_or(0);

        let pareto_frontier = storage
            .load_checkpoint()
            .ok()
            .flatten()
            .map(|s| s.pareto_frontier)
            .unwrap_or_default();

        let storage_path = storage.run_dir().display().to_string();

        let mut app = Self::from_candidates_with_config(
            candidates,
            function_name,
            storage_path,
            objectives,
            pareto_frontier,
        )?;
        app.total_trials = total_trials;
        Ok(app)
    }

    /// In-memory constructor used by tests and by `from_storage`.
    pub fn from_candidates_with_config(
        candidates: Vec<Candidate>,
        function_name: String,
        storage_path: String,
        objectives: Vec<ObjectiveConfig>,
        pareto_frontier: Vec<usize>,
    ) -> Result<Self> {
        let id_to_index: HashMap<usize, usize> = candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| (c.id, idx))
            .collect();

        let mut by_iter: HashMap<usize, Vec<usize>> = HashMap::new();
        for c in &candidates {
            by_iter.entry(c.iteration).or_default().push(c.id);
        }
        let mut iterations: Vec<usize> = by_iter.keys().copied().collect();
        iterations.sort();

        let rows: Vec<CandidateRow> = iterations
            .into_iter()
            .map(|iteration| {
                let mut candidate_ids = by_iter.remove(&iteration).unwrap();
                candidate_ids.sort();
                CandidateRow {
                    iteration,
                    candidate_ids,
                }
            })
            .collect();

        Ok(Self {
            candidates,
            rows,
            selected_row: 0,
            selected_col: 0,
            scroll_offset: 0,
            prompt_scroll: 0,
            should_quit: false,
            storage_path,
            function_name,
            id_to_index,
            objectives,
            pareto_frontier,
            live_mode: false,
            storage_path_for_reload: None,
            status: OptimizationStatus::Unknown,
            total_trials: 0,
            apply_candidate_id: None,
        })
    }

    /// Reload candidate/state/error data from disk. Called periodically in
    /// live mode.
    fn try_reload(&mut self) {
        let Some(ref storage_path) = self.storage_path_for_reload else {
            return;
        };
        let Ok(storage) = Storage::load(storage_path.clone()) else {
            return;
        };

        // Orchestrator wrote an error — switch to "Failed" and exit so the
        // terminal returns control to stderr.
        if let Some(error_message) = storage.load_error() {
            self.status = OptimizationStatus::Failed {
                message: error_message,
            };
            self.should_quit = true;
            return;
        }

        let Ok(new_candidates) = storage.load_candidates() else {
            return;
        };

        if new_candidates.len() == self.candidates.len() {
            // No new candidates; just refresh the frontier if it changed.
            let new_pareto = storage
                .load_checkpoint()
                .ok()
                .flatten()
                .map(|s| s.pareto_frontier)
                .unwrap_or_default();
            if new_pareto != self.pareto_frontier {
                self.pareto_frontier = new_pareto;
            }
            self.update_status(&storage);
            return;
        }

        let selected_id = self.selected_candidate_id();

        self.candidates = new_candidates;
        self.id_to_index = self
            .candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| (c.id, idx))
            .collect();

        let mut by_iter: HashMap<usize, Vec<usize>> = HashMap::new();
        for c in &self.candidates {
            by_iter.entry(c.iteration).or_default().push(c.id);
        }
        let mut iterations: Vec<usize> = by_iter.keys().copied().collect();
        iterations.sort();
        self.rows = iterations
            .into_iter()
            .map(|iteration| {
                let mut candidate_ids = by_iter.remove(&iteration).unwrap();
                candidate_ids.sort();
                CandidateRow {
                    iteration,
                    candidate_ids,
                }
            })
            .collect();

        if let Some(old_id) = selected_id {
            for (row_idx, row) in self.rows.iter().enumerate() {
                if let Some(col_idx) = row.candidate_ids.iter().position(|&id| id == old_id) {
                    self.selected_row = row_idx;
                    self.selected_col = col_idx;
                    self.update_scroll_offset();
                    break;
                }
            }
        } else if !self.rows.is_empty() {
            self.selected_row = self.rows.len() - 1;
            self.selected_col = 0;
            self.update_scroll_offset();
        }

        self.pareto_frontier = storage
            .load_checkpoint()
            .ok()
            .flatten()
            .map(|s| s.pareto_frontier)
            .unwrap_or_default();

        if let Ok(cfg) = storage.load_config() {
            self.objectives = cfg.objectives;
            self.function_name = cfg.function_name;
            self.total_trials = cfg.max_iterations as usize;
        }

        self.update_status(&storage);
    }

    fn update_status(&mut self, storage: &Storage) {
        if matches!(storage.load_final_results(), Ok(Some(_))) {
            self.status = OptimizationStatus::Completed;
            return;
        }
        if let Ok(Some(state)) = storage.load_checkpoint() {
            // current_iteration is the last completed step; show +1 for the
            // one in progress. At the very start it's 0 so we display 1.
            let display = state.current_iteration.saturating_add(1);
            self.status = OptimizationStatus::Running {
                display_iteration: display,
                total_trials: self.total_trials,
            };
            return;
        }
        if !self.rows.is_empty() {
            let max_iter = self.rows.last().map(|r| r.iteration).unwrap_or(0);
            self.status = OptimizationStatus::Running {
                display_iteration: max_iter + 1,
                total_trials: self.total_trials,
            };
        } else if self.live_mode && self.total_trials > 0 {
            self.status = OptimizationStatus::Running {
                display_iteration: 0,
                total_trials: self.total_trials,
            };
        } else {
            self.status = OptimizationStatus::Unknown;
        }
    }

    fn selected_candidate_id(&self) -> Option<usize> {
        let row = self.rows.get(self.selected_row)?;
        let col = self
            .selected_col
            .min(row.candidate_ids.len().saturating_sub(1));
        row.candidate_ids.get(col).copied()
    }

    fn selected_candidate(&self) -> Option<&Candidate> {
        let candidate_id = self.selected_candidate_id()?;
        self.id_to_index
            .get(&candidate_id)
            .and_then(|&idx| self.candidates.get(idx))
    }

    fn is_pareto(&self, candidate_id: usize) -> bool {
        self.pareto_frontier.contains(&candidate_id)
    }

    fn select_previous_row(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.selected_row > 0 {
            self.selected_row -= 1;
            let max_col = self.rows[self.selected_row]
                .candidate_ids
                .len()
                .saturating_sub(1);
            self.selected_col = self.selected_col.min(max_col);
            self.prompt_scroll = 0;
            self.update_scroll_offset();
        }
    }

    fn select_next_row(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.selected_row < self.rows.len() - 1 {
            self.selected_row += 1;
            let max_col = self.rows[self.selected_row]
                .candidate_ids
                .len()
                .saturating_sub(1);
            self.selected_col = self.selected_col.min(max_col);
            self.prompt_scroll = 0;
            self.update_scroll_offset();
        }
    }

    fn select_previous_col(&mut self) {
        if self.selected_col > 0 {
            self.selected_col -= 1;
            self.prompt_scroll = 0;
        }
    }

    fn select_next_col(&mut self) {
        if let Some(row) = self.rows.get(self.selected_row) {
            if self.selected_col < row.candidate_ids.len().saturating_sub(1) {
                self.selected_col += 1;
                self.prompt_scroll = 0;
            }
        }
    }

    fn update_scroll_offset(&mut self) {
        if self.selected_row < self.scroll_offset {
            self.scroll_offset = self.selected_row;
        }
    }

    fn scroll_prompt_up(&mut self) {
        self.prompt_scroll = self.prompt_scroll.saturating_sub(3);
    }

    fn scroll_prompt_down(&mut self) {
        self.prompt_scroll = self.prompt_scroll.saturating_add(3);
    }

    fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Enter => {
                if self.live_mode {
                    if let Some(candidate_id) = self.selected_candidate_id() {
                        self.apply_candidate_id = Some(candidate_id);
                        self.should_quit = true;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_row(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_row(),
            KeyCode::Left | KeyCode::Char('h') => self.select_previous_col(),
            KeyCode::Right | KeyCode::Char('l') => self.select_next_col(),
            KeyCode::PageUp => {
                for _ in 0..5 {
                    self.select_previous_row();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..5 {
                    self.select_next_row();
                }
            }
            KeyCode::Home => {
                self.selected_row = 0;
                self.selected_col = 0;
                self.scroll_offset = 0;
                self.prompt_scroll = 0;
            }
            KeyCode::End => {
                if !self.rows.is_empty() {
                    self.selected_row = self.rows.len() - 1;
                    self.selected_col = 0;
                    self.prompt_scroll = 0;
                    self.update_scroll_offset();
                }
            }
            KeyCode::Char('[') => self.scroll_prompt_up(),
            KeyCode::Char(']') => self.scroll_prompt_down(),
            _ => {}
        }
    }

    /// Resolve the value of a named objective against a candidate's scores.
    ///
    /// Per-testset objectives follow the orchestrator's `"accuracy:testset"`
    /// naming — the suffix is looked up in `per_testset_scores`.
    fn get_objective_value(objective: &ObjectiveConfig, scores: &CandidateScores) -> f64 {
        match objective.name.as_str() {
            "accuracy" => scores.test_pass_rate,
            "tokens" => scores.avg_prompt_tokens + scores.avg_completion_tokens,
            "prompt_tokens" => scores.avg_prompt_tokens,
            "completion_tokens" => scores.avg_completion_tokens,
            "latency" => scores.avg_latency_ms,
            other => {
                if let Some((_, testset)) = other.split_once(':') {
                    scores.per_testset_scores.get(testset).copied().unwrap_or(0.0)
                } else {
                    0.0
                }
            }
        }
    }
}

fn render_ui(frame: &mut Frame, app: &mut App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(frame.area());

    render_header(frame, app, main_chunks[0]);

    let content_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(main_chunks[1]);

    render_tree_panel(frame, app, content_chunks[0]);
    render_details_panel(frame, app, content_chunks[1]);
    render_footer(frame, app, main_chunks[2]);
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(" GEPA Optimization Viewer - {} ", app.function_name);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_COLOR));

    let objectives_str = if app.objectives.is_empty() {
        "default".to_string()
    } else {
        app.objectives
            .iter()
            .map(|o| format!("{}={:.0}%", o.name, o.weight * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let status_display = match &app.status {
        OptimizationStatus::Running {
            display_iteration,
            total_trials,
        } => {
            let spinner_idx = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() / 100)
                .unwrap_or(0) as usize)
                % SPINNER_FRAMES.len();
            let spinner = SPINNER_FRAMES[spinner_idx];
            format!("{} Running {}/{}", spinner, display_iteration, total_trials)
        }
        OptimizationStatus::Completed => "✓ Complete".to_string(),
        OptimizationStatus::Failed { message } => format!("✗ Error: {message}"),
        OptimizationStatus::Unknown => String::new(),
    };

    let is_error = matches!(app.status, OptimizationStatus::Failed { .. });
    let stats = format!(
        "Candidates: {} | Pareto: {} | Objectives: {} | {}  [{}]",
        app.candidates.len(),
        app.pareto_frontier.len(),
        objectives_str,
        status_display,
        app.storage_path,
    );
    let stats_color = if is_error { Color::Red } else { Color::Gray };
    let paragraph = Paragraph::new(stats)
        .style(Style::default().fg(stats_color))
        .block(block);
    frame.render_widget(paragraph, area);
}

const CARD_HEIGHT: u16 = 5;

fn render_tree_panel(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(" Candidates ")
        .title_style(
            Style::default()
                .fg(HEADER_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    if app.rows.is_empty() {
        let msg = Paragraph::new("No candidates yet").style(Style::default().fg(Color::Gray));
        frame.render_widget(msg, inner_area);
        return;
    }

    let visible_rows = (inner_area.height / CARD_HEIGHT) as usize;

    if app.selected_row >= app.scroll_offset + visible_rows {
        app.scroll_offset = app.selected_row - visible_rows + 1;
    }
    if app.selected_row < app.scroll_offset {
        app.scroll_offset = app.selected_row;
    }

    let mut y_offset = 0u16;
    for (row_idx, row) in app.rows.iter().enumerate().skip(app.scroll_offset) {
        if y_offset + CARD_HEIGHT > inner_area.height {
            break;
        }
        let row_area = Rect {
            x: inner_area.x,
            y: inner_area.y + y_offset,
            width: inner_area.width,
            height: CARD_HEIGHT,
        };
        render_candidate_row(frame, app, row, row_idx, row_area);
        y_offset += CARD_HEIGHT;
    }

    if app.rows.len() > visible_rows {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state = ScrollbarState::new(app.rows.len()).position(app.selected_row);
        frame.render_stateful_widget(scrollbar, inner_area, &mut scrollbar_state);
    }
}

fn render_candidate_row(
    frame: &mut Frame,
    app: &App,
    row: &CandidateRow,
    row_idx: usize,
    area: Rect,
) {
    let is_selected_row = row_idx == app.selected_row;
    let num_cards = row.candidate_ids.len();
    let card_width = if num_cards > 0 {
        (area.width / num_cards as u16).min(area.width).max(12)
    } else {
        area.width
    };

    let iter_label = format!("T{}", row.iteration);
    let iter_style = if is_selected_row {
        Style::default()
            .fg(ACCENT_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let label_width = 3u16;
    let label_area = Rect {
        x: area.x,
        y: area.y + CARD_HEIGHT / 2,
        width: label_width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(iter_label).style(iter_style), label_area);

    let cards_area = Rect {
        x: area.x + label_width,
        y: area.y,
        width: area.width.saturating_sub(label_width),
        height: area.height,
    };

    for (col_idx, &candidate_id) in row.candidate_ids.iter().enumerate() {
        let is_selected = is_selected_row && col_idx == app.selected_col;
        let card_x = cards_area.x + (col_idx as u16 * card_width);
        let card_area = Rect {
            x: card_x,
            y: cards_area.y,
            width: card_width.min(cards_area.width.saturating_sub(col_idx as u16 * card_width)),
            height: CARD_HEIGHT,
        };
        if card_area.width > 2 {
            render_candidate_card(frame, app, candidate_id, is_selected, card_area);
        }
    }
}

fn render_candidate_card(
    frame: &mut Frame,
    app: &App,
    candidate_id: usize,
    is_selected: bool,
    area: Rect,
) {
    let candidate = app
        .id_to_index
        .get(&candidate_id)
        .and_then(|&idx| app.candidates.get(idx));
    let is_pareto = app.is_pareto(candidate_id);

    let (method_icon, method_color) = match candidate.map(|c| &c.method) {
        Some(CandidateMethod::Initial) => ("◆", Color::Blue),
        Some(CandidateMethod::Reflection) => ("◇", Color::Yellow),
        Some(CandidateMethod::Merge) => ("◈", Color::Magenta),
        None => ("?", Color::Gray),
    };

    let border_color = if is_selected {
        CARD_SELECTED_BORDER
    } else if is_pareto {
        Color::Yellow
    } else {
        CARD_BORDER
    };

    let border_type = if is_selected {
        BorderType::Double
    } else {
        BorderType::Rounded
    };

    let pareto_star = if is_pareto { "★" } else { "" };
    let title = format!(" {}{} #{} ", pareto_star, method_icon, candidate_id);

    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(method_color)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(Style::default().fg(border_color));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let content = if let Some(scores) = candidate.and_then(|c| c.scores.as_ref()) {
        let mut lines = Vec::new();
        if app.objectives.is_empty() {
            let pass_text = format!("{:.0}%", scores.test_pass_rate * 100.0);
            lines.push(Line::from(vec![Span::styled(
                pass_text,
                Style::default().fg(score_color(scores.test_pass_rate)),
            )]));
        } else {
            for obj in &app.objectives {
                let value = App::get_objective_value(obj, scores);
                let (text, color) = format_compact_metric(obj, value);
                lines.push(Line::from(vec![Span::styled(
                    text,
                    Style::default().fg(color),
                )]));
            }
        }

        if let Some(c) = candidate {
            if !c.parent_ids.is_empty() {
                let parents: String = c
                    .parent_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(",");
                lines.push(Line::from(vec![Span::styled(
                    format!("←{parents}"),
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "...",
            Style::default().fg(Color::Gray),
        ))]
    };

    let paragraph = Paragraph::new(content);
    frame.render_widget(paragraph, inner_area);
}

fn render_details_panel(frame: &mut Frame, app: &App, area: Rect) {
    let num_objectives = app.objectives.len().max(1);
    let metadata_height = 6 + num_objectives as u16;

    let has_rationale = app
        .selected_candidate()
        .and_then(|c| c.rationale.as_ref())
        .map(|r| !r.is_empty())
        .unwrap_or(false);

    let constraints = if has_rationale {
        vec![
            Constraint::Length(metadata_height),
            Constraint::Length(10),
            Constraint::Min(10),
        ]
    } else {
        vec![Constraint::Length(metadata_height), Constraint::Min(10)]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_metadata_panel(frame, app, chunks[0]);
    if has_rationale {
        render_rationale_panel(frame, app, chunks[1]);
        render_prompt_panel(frame, app, chunks[2]);
    } else {
        render_prompt_panel(frame, app, chunks[1]);
    }
}

fn render_metadata_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Candidate Details ")
        .title_style(
            Style::default()
                .fg(HEADER_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let content = if let Some(candidate) = app.selected_candidate() {
        let method_str = match &candidate.method {
            CandidateMethod::Initial => "Initial (user's original)",
            CandidateMethod::Reflection => "Reflection (improved from failures)",
            CandidateMethod::Merge => "Merge (combined candidates)",
        };

        let parents_str = if candidate.parent_ids.is_empty() {
            "None (initial candidate)".to_string()
        } else {
            candidate
                .parent_ids
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let is_pareto = app.is_pareto(candidate.id);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("ID: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("#{}", candidate.id),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("Method: ", Style::default().fg(Color::Gray)),
                Span::styled(method_str, Style::default().fg(ACCENT_COLOR)),
                if is_pareto {
                    Span::styled("  ★ Pareto", Style::default().fg(Color::Yellow))
                } else {
                    Span::raw("")
                },
            ]),
            Line::from(vec![
                Span::styled("Parent(s): ", Style::default().fg(Color::Gray)),
                Span::styled(parents_str, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
        ];

        if let Some(scores) = &candidate.scores {
            lines.push(Line::from(Span::styled(
                "Optimization Metrics:",
                Style::default()
                    .fg(HEADER_COLOR)
                    .add_modifier(Modifier::BOLD),
            )));

            if app.objectives.is_empty() {
                lines.extend(format_default_scores(scores));
            } else {
                for obj in &app.objectives {
                    let value = App::get_objective_value(obj, scores);
                    let (formatted_value, color) = format_objective_value(obj, value);
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{} ", obj.name), Style::default().fg(Color::Gray)),
                        Span::styled(
                            format!("({}%): ", (obj.weight * 100.0) as i32),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(formatted_value, Style::default().fg(color)),
                    ]));
                }
            }

            if !scores.per_testset_scores.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("Testsets: ", Style::default().fg(Color::Gray)),
                ]));
                let mut entries: Vec<(&String, &f64)> = scores.per_testset_scores.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (name, rate) in entries {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(format!("{name}: "), Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{:.0}%", rate * 100.0),
                            Style::default().fg(score_color(*rate)),
                        ),
                    ]));
                }
            }
        } else {
            lines.push(Line::from(Span::styled(
                "Not yet evaluated",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }

        lines
    } else {
        vec![Line::from(Span::styled(
            "Select a candidate to view details",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ))]
    };

    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_rationale_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Rationale ")
        .title_style(
            Style::default()
                .fg(HEADER_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let inner_area = block.inner(area);

    let content = if let Some(candidate) = app.selected_candidate() {
        if let Some(rationale) = &candidate.rationale {
            Text::from(
                rationale
                    .lines()
                    .map(|line| {
                        Line::from(Span::styled(
                            line.to_string(),
                            Style::default().fg(Color::White),
                        ))
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            Text::from(Span::styled(
                "No rationale available (initial candidate)",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ))
        }
    } else {
        Text::from(Span::styled(
            "Select a candidate to view rationale",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ))
    };

    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);

    if let Some(candidate) = app.selected_candidate() {
        if let Some(rationale) = &candidate.rationale {
            let line_count = rationale.lines().count();
            if line_count > inner_area.height as usize {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓"));
                let mut scrollbar_state = ScrollbarState::new(line_count).position(0);
                frame.render_stateful_widget(scrollbar, inner_area, &mut scrollbar_state);
            }
        }
    }
}

fn format_objective_value(obj: &ObjectiveConfig, value: f64) -> (String, Color) {
    match obj.name.as_str() {
        "accuracy" => {
            let formatted = format!("{:.1}%", value * 100.0);
            let color = score_color(value);
            (formatted, color)
        }
        "tokens" | "prompt_tokens" | "completion_tokens" => {
            let formatted = format!("{value:.0} tokens");
            let color = if value < 100.0 {
                SCORE_GOOD
            } else if value < 500.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (formatted, color)
        }
        "latency" => {
            let formatted = format!("{:.2}s", value / 1000.0);
            let color = if value < 500.0 {
                SCORE_GOOD
            } else if value < 2000.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (formatted, color)
        }
        name if name.starts_with("accuracy:") => {
            let formatted = format!("{:.1}%", value * 100.0);
            (formatted, score_color(value))
        }
        _ => {
            let formatted = format!("{value:.2}");
            (formatted, Color::White)
        }
    }
}

fn format_default_scores(scores: &CandidateScores) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Pass Rate: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!(
                    "{:.1}% ({}/{} tests)",
                    scores.test_pass_rate * 100.0,
                    scores.tests_passed,
                    scores.tests_total
                ),
                Style::default().fg(score_color(scores.test_pass_rate)),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Tokens: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!(
                    "in: {:.0}  out: {:.0}",
                    scores.avg_prompt_tokens, scores.avg_completion_tokens
                ),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Latency: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2}s", scores.avg_latency_ms / 1000.0),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ]
}

fn render_prompt_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Prompt Preview ")
        .title_style(
            Style::default()
                .fg(HEADER_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let inner_area = block.inner(area);

    let content = if let Some(candidate) = app.selected_candidate() {
        let mut text = String::new();
        text.push_str(&format!(
            "function {} {{\n",
            candidate.function.function_name
        ));
        text.push_str("  prompt #\"\n");
        for line in candidate.function.prompt_text.lines() {
            text.push_str("    ");
            text.push_str(line);
            text.push('\n');
        }
        text.push_str("  \"#\n");
        text.push_str("}\n\n");

        // Render each class / enum the way the applier writes it back to
        // disk: class-level descriptions become a `///` docstring block
        // above the item, not an inline `@@description` attribute (which
        // would render *outside* the braces and look malformed).
        for class in &candidate.function.classes {
            if let Some(desc) = &class.description {
                push_doc_comment(&mut text, desc, "");
            }
            text.push_str(&format!("class {} {{\n", class.class_name));
            for field in &class.fields {
                text.push_str(&format!("  {} {}", field.field_name, field.field_type));
                if let Some(desc) = &field.description {
                    text.push_str(&format!(" @description(\"{}\")", escape_attr_string(desc)));
                }
                if let Some(alias) = &field.alias {
                    text.push_str(&format!(" @alias(\"{}\")", escape_attr_string(alias)));
                }
                text.push('\n');
            }
            text.push_str("}\n\n");
        }

        for enum_def in &candidate.function.enums {
            text.push_str(&format!("enum {} {{\n", enum_def.enum_name));
            for value in &enum_def.values {
                text.push_str(&format!("  {value}"));
                if let Some(desc) = enum_def.value_descriptions.get(value) {
                    text.push_str(&format!(" @description(\"{}\")", escape_attr_string(desc)));
                }
                text.push('\n');
            }
            text.push_str("}\n\n");
        }

        syntax_highlight(&text)
    } else {
        Text::from(Span::styled(
            "Select a candidate to view its prompt",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ))
    };

    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.prompt_scroll, 0));
    frame.render_widget(paragraph, area);

    if let Some(candidate) = app.selected_candidate() {
        let line_count = candidate.function.prompt_text.lines().count()
            + candidate
                .function
                .classes
                .iter()
                .map(|c| c.fields.len() + 3)
                .sum::<usize>()
            + candidate
                .function
                .enums
                .iter()
                .map(|e| e.values.len() + 2)
                .sum::<usize>()
            + 10;

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state =
            ScrollbarState::new(line_count).position(app.prompt_scroll as usize);
        frame.render_stateful_widget(
            scrollbar,
            inner_area.inner(ratatui::layout::Margin {
                horizontal: 0,
                vertical: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(
            " ↑/↓ ",
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Row  "),
        Span::styled(
            " ←/→ ",
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Card  "),
        Span::styled(
            " [/] ",
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Scroll  "),
        Span::styled(
            " ★ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Pareto  "),
    ];

    if app.live_mode {
        spans.push(Span::styled(
            " Enter ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("Apply  "));
    }

    spans.push(Span::styled(
        " q ",
        Style::default()
            .fg(ACCENT_COLOR)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("Quit"));

    let hints = Line::from(spans);
    let paragraph = Paragraph::new(hints).style(Style::default().fg(Color::Gray));
    frame.render_widget(paragraph, area);
}

/// Append `description` as a `///`-prefixed docstring block at the given
/// indent, one line per `\n`-delimited source line. Mirrors the format
/// written by `applier::format_doc_comment` so the preview matches the
/// patched source on disk.
fn push_doc_comment(out: &mut String, description: &str, indent: &str) {
    for line in description.split('\n') {
        out.push_str(indent);
        out.push_str("/// ");
        out.push_str(line);
        out.push('\n');
    }
}

/// Escape characters that would break out of a `@description("...")` or
/// `@alias("...")` string argument. Keeps the preview from rendering as
/// structurally-broken BAML when the LLM proposes a description containing
/// quotes or backslashes.
fn escape_attr_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn syntax_highlight(code: &str) -> Text<'static> {
    let mut lines = Vec::new();

    for line in code.lines() {
        let mut spans = Vec::new();
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        spans.push(Span::raw(indent.to_string()));

        if trimmed.starts_with("function ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("enum ")
        {
            let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
            if parts.len() == 2 {
                spans.push(Span::styled(
                    parts[0].to_string(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
                let rest = parts[1];
                if let Some(brace_pos) = rest.find('{') {
                    spans.push(Span::styled(
                        rest[..brace_pos].trim().to_string(),
                        Style::default().fg(Color::Cyan),
                    ));
                    spans.push(Span::styled(
                        " {".to_string(),
                        Style::default().fg(Color::Gray),
                    ));
                } else {
                    spans.push(Span::raw(rest.to_string()));
                }
            } else {
                spans.push(Span::raw(trimmed.to_string()));
            }
        } else if trimmed.starts_with("prompt ")
            || trimmed.starts_with("\"#")
            || trimmed.starts_with("#\"")
        {
            spans.push(Span::styled(
                trimmed.to_string(),
                Style::default().fg(Color::Green),
            ));
        } else if trimmed.starts_with("//") {
            spans.push(Span::styled(
                trimmed.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        } else if trimmed.starts_with('@') {
            spans.push(Span::styled(
                trimmed.to_string(),
                Style::default().fg(Color::Yellow),
            ));
        } else if trimmed == "}" || trimmed == "{" {
            spans.push(Span::styled(
                trimmed.to_string(),
                Style::default().fg(Color::Gray),
            ));
        } else if trimmed.contains(" @") {
            if let Some(at_pos) = trimmed.find(" @") {
                let (field_part, annotation_part) = trimmed.split_at(at_pos);
                let field_parts: Vec<&str> = field_part.splitn(2, ' ').collect();
                if field_parts.len() == 2 {
                    spans.push(Span::styled(
                        field_parts[0].to_string(),
                        Style::default().fg(Color::White),
                    ));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        field_parts[1].to_string(),
                        Style::default().fg(Color::Cyan),
                    ));
                } else {
                    spans.push(Span::raw(field_part.to_string()));
                }
                spans.push(Span::styled(
                    annotation_part.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                spans.push(Span::raw(trimmed.to_string()));
            }
        } else {
            spans.push(Span::styled(
                trimmed.to_string(),
                Style::default().fg(Color::White),
            ));
        }

        lines.push(Line::from(spans));
    }

    Text::from(lines)
}

fn score_color(score: f64) -> Color {
    if score >= 0.8 {
        SCORE_GOOD
    } else if score >= 0.5 {
        SCORE_MED
    } else {
        SCORE_BAD
    }
}

fn format_compact_metric(obj: &ObjectiveConfig, value: f64) -> (String, Color) {
    match obj.name.as_str() {
        "accuracy" => {
            let text = format!("{:.0}%", value * 100.0);
            (text, score_color(value))
        }
        "tokens" => {
            let text = format!("{value:.0}t");
            let color = if value < 200.0 {
                SCORE_GOOD
            } else if value < 500.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (text, color)
        }
        "prompt_tokens" => {
            let text = format!("p:{value:.0}");
            let color = if value < 100.0 {
                SCORE_GOOD
            } else if value < 300.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (text, color)
        }
        "completion_tokens" => {
            let text = format!("c:{value:.0}");
            let color = if value < 100.0 {
                SCORE_GOOD
            } else if value < 300.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (text, color)
        }
        "latency" => {
            let text = format!("{:.2}s", value / 1000.0);
            let color = if value < 500.0 {
                SCORE_GOOD
            } else if value < 2000.0 {
                SCORE_MED
            } else {
                SCORE_BAD
            };
            (text, color)
        }
        name if name.starts_with("accuracy:") => {
            let text = format!("{:.0}%", value * 100.0);
            (text, score_color(value))
        }
        _ => {
            let text = format!("{value:.1}");
            (text, Color::White)
        }
    }
}

/// Open the TUI against a frozen run directory (no polling).
pub fn run_tui(storage_path: &Path) -> Result<()> {
    run_tui_internal(storage_path, false)
}

/// Open the TUI in live-reload mode — polls the run directory for new
/// candidates every 500ms. Intended to be launched from a background
/// thread while the orchestrator runs in the main thread.
pub fn run_tui_live(storage_path: &Path) -> Result<()> {
    run_tui_internal(storage_path, true)
}

fn run_tui_internal(storage_path: &Path, live_mode: bool) -> Result<()> {
    let storage = Storage::load(storage_path.to_path_buf())
        .context("Failed to open optimization storage")?;

    let mut app = App::from_storage(&storage)?;
    app.live_mode = live_mode;
    app.storage_path_for_reload = Some(storage_path.to_path_buf());
    app.update_status(&storage);

    if !live_mode && app.candidates.is_empty() {
        anyhow::bail!("No candidates found in {}", storage_path.display());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Some(candidate_id) = app.apply_candidate_id {
        write_apply_request(storage_path, candidate_id)?;
    }

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    use std::time::Instant;

    let mut last_reload = Instant::now();
    let reload_interval = Duration::from_millis(500);

    loop {
        terminal.draw(|f| render_ui(f, app))?;

        if app.live_mode && last_reload.elapsed() >= reload_interval {
            app.try_reload();
            last_reload = Instant::now();
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key.code, key.modifiers);
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

// =============================================================================
// Post-optimization Pareto frontier display and selection
// =============================================================================

/// Pretty-print the Pareto frontier and prompt the user to pick one to
/// apply. Returns `Some(id)` if the user confirms a selection. Returns
/// `None` if the frontier is empty or the user skips the prompt.
pub fn display_pareto_and_select(
    candidates: &[Candidate],
    pareto_ids: &[usize],
    objectives: &[ObjectiveConfig],
    function_name: &str,
) -> Option<usize> {
    if pareto_ids.is_empty() {
        println!("\nNo candidates on the Pareto frontier.");
        return None;
    }

    let id_to_candidate: HashMap<usize, &Candidate> =
        candidates.iter().map(|c| (c.id, c)).collect();

    println!("\n{}", "═".repeat(70));
    println!("  ★ Pareto Frontier Candidates for {function_name}");
    println!("{}", "═".repeat(70));

    let mut header = format!("  {:>4} │", "ID");
    for obj in objectives {
        header.push_str(&format!(" {:>12} │", obj.name));
    }
    println!("{header}");
    println!("  {}", "─".repeat(66));

    for &id in pareto_ids {
        if let Some(candidate) = id_to_candidate.get(&id) {
            if let Some(scores) = &candidate.scores {
                let mut row = format!("  #{id:>3} │");
                for obj in objectives {
                    let value = App::get_objective_value(obj, scores);
                    let formatted = match obj.name.as_str() {
                        "accuracy" => format!("{:.1}%", value * 100.0),
                        "tokens" | "prompt_tokens" | "completion_tokens" => {
                            format!("{value:.0}")
                        }
                        "latency" => format!("{value:.0}ms"),
                        name if name.starts_with("accuracy:") => {
                            format!("{:.1}%", value * 100.0)
                        }
                        _ => format!("{value:.2}"),
                    };
                    row.push_str(&format!(" {formatted:>12} │"));
                }
                println!("{row}");
            }
        }
    }

    println!("{}", "═".repeat(70));
    println!();

    if pareto_ids.len() == 1 {
        println!(
            "Only one candidate on the Pareto frontier: #{}",
            pareto_ids[0]
        );
        print!("Apply this candidate? [Y/n]: ");
        io::Write::flush(&mut io::stdout()).ok();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() {
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                return Some(pareto_ids[0]);
            }
        }
        return None;
    }

    println!("Enter candidate ID to apply (or press Enter to skip):");
    print!("> ");
    io::Write::flush(&mut io::stdout()).ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let id_str = input.trim_start_matches('#');
        if let Ok(id) = id_str.parse::<usize>() {
            if pareto_ids.contains(&id) {
                return Some(id);
            } else {
                println!("Candidate #{id} is not on the Pareto frontier.");
            }
        } else {
            println!("Invalid input: {input}");
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::OptimizableFunction;

    fn make_test_candidate(id: usize, iteration: usize, pass_rate: f64) -> Candidate {
        Candidate {
            id,
            iteration,
            parent_ids: if id == 0 { vec![] } else { vec![id - 1] },
            method: if id == 0 {
                CandidateMethod::Initial
            } else {
                CandidateMethod::Reflection
            },
            function: OptimizableFunction {
                function_name: "TestFunction".to_string(),
                prompt_text: "Test prompt".to_string(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: Some(CandidateScores {
                test_pass_rate: pass_rate,
                tests_passed: (pass_rate * 10.0) as usize,
                tests_total: 10,
                avg_prompt_tokens: 100.0,
                avg_completion_tokens: 50.0,
                avg_latency_ms: 500.0,
                per_testset_scores: HashMap::new(),
            }),
            rationale: if id == 0 {
                None
            } else {
                Some("Improved prompt clarity based on test failures.".to_string())
            },
        }
    }

    #[test]
    fn test_app_creation() {
        let candidates = vec![
            make_test_candidate(0, 0, 0.5),
            make_test_candidate(1, 1, 0.7),
            make_test_candidate(2, 2, 0.9),
        ];

        let app = App::from_candidates_with_config(
            candidates,
            "TestFunc".to_string(),
            "/tmp/test".to_string(),
            vec![],
            vec![],
        )
        .unwrap();

        assert_eq!(app.candidates.len(), 3);
        assert_eq!(app.rows.len(), 3);
        assert_eq!(app.rows[0].candidate_ids.len(), 1);
    }

    #[test]
    fn test_navigation() {
        let candidates = vec![
            make_test_candidate(0, 0, 0.5),
            make_test_candidate(1, 1, 0.7),
        ];

        let mut app = App::from_candidates_with_config(
            candidates,
            "TestFunc".to_string(),
            "/tmp/test".to_string(),
            vec![],
            vec![],
        )
        .unwrap();

        assert_eq!(app.selected_row, 0);
        assert_eq!(app.selected_col, 0);
        app.select_next_row();
        assert_eq!(app.selected_row, 1);
        app.select_previous_row();
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn test_multi_candidate_row() {
        let candidates = vec![
            make_test_candidate(0, 0, 0.5),
            Candidate {
                id: 1,
                iteration: 0,
                parent_ids: vec![0],
                method: CandidateMethod::Reflection,
                function: OptimizableFunction {
                    function_name: "TestFunction".to_string(),
                    prompt_text: "Test prompt".to_string(),
                    classes: vec![],
                    enums: vec![],
                    function_source: None,
                },
                scores: Some(CandidateScores {
                    test_pass_rate: 0.6,
                    tests_passed: 6,
                    tests_total: 10,
                    avg_prompt_tokens: 100.0,
                    avg_completion_tokens: 50.0,
                    avg_latency_ms: 500.0,
                    per_testset_scores: HashMap::new(),
                }),
                rationale: Some("Test rationale".to_string()),
            },
        ];

        let mut app = App::from_candidates_with_config(
            candidates,
            "TestFunc".to_string(),
            "/tmp/test".to_string(),
            vec![],
            vec![],
        )
        .unwrap();

        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].candidate_ids.len(), 2);

        assert_eq!(app.selected_col, 0);
        app.select_next_col();
        assert_eq!(app.selected_col, 1);
        app.select_next_col();
        assert_eq!(app.selected_col, 1);
        app.select_previous_col();
        assert_eq!(app.selected_col, 0);
    }

    #[test]
    fn test_score_color() {
        assert_eq!(score_color(1.0), SCORE_GOOD);
        assert_eq!(score_color(0.8), SCORE_GOOD);
        assert_eq!(score_color(0.6), SCORE_MED);
        assert_eq!(score_color(0.3), SCORE_BAD);
    }

    #[test]
    fn test_pareto_detection() {
        let candidates = vec![
            make_test_candidate(0, 0, 0.5),
            make_test_candidate(1, 1, 0.7),
        ];

        let app = App::from_candidates_with_config(
            candidates,
            "TestFunc".to_string(),
            "/tmp/test".to_string(),
            vec![],
            vec![1],
        )
        .unwrap();

        assert!(!app.is_pareto(0));
        assert!(app.is_pareto(1));
    }
}
