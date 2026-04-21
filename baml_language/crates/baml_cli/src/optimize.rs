#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use baml_db::{baml_compiler2_emit, baml_compiler_diagnostics::Severity};
use baml_optimize::orchestrator::initial_function_from_db;
use baml_optimize::{
    Applier, Candidate, GEPAOrchestrator, Objective, OrchestratorConfig, discover_all_tests,
    parse_objectives,
};
use baml_project::ProjectDatabase;
use baml_workspace::discover_baml_files;
use bex_engine::BexEngine;
use clap::Args;
use sys_native::SysOpsExt;

#[derive(Args, Clone, Debug)]
pub struct OptimizeArgs {
    /// Function to optimize
    #[arg(long, required = true)]
    pub function: String,

    /// Path to baml_src directory
    #[arg(long, default_value = ".")]
    pub from: PathBuf,

    /// Maximum number of optimization iterations
    #[arg(long, default_value = "50")]
    pub max_iterations: u32,

    /// Number of tests to run in parallel
    #[arg(long, default_value = "8")]
    pub parallel: usize,

    /// Objective weights, e.g. `"accuracy=0.8,tokens=0.2"` or
    /// `"accuracy:testset_a=0.5"` (per-testset objective).
    #[arg(long, default_value = "accuracy=1.0")]
    pub weights: String,

    /// Test filter pattern (same syntax as `baml-cli test -i`):
    /// `FunctionName::TestName`, `FunctionName::`, `::TestName`, or
    /// `Get*::*Bar` with wildcards. May be passed multiple times.
    #[arg(long, short = 'i')]
    pub include: Vec<String>,

    /// Tests to exclude. Same syntax as `--include`; takes precedence.
    #[arg(long, short = 'x')]
    pub exclude: Vec<String>,

    /// Resume from an existing run directory (e.g.
    /// `.baml_optimize/run_20260421_101530`). Prints the saved summary and
    /// exits — useful for inspecting a prior run.
    #[arg(long)]
    pub resume: Option<PathBuf>,

    /// Open the live TUI viewer on an existing run directory and exit
    /// without running optimization. Useful for post-mortem inspection.
    #[arg(long, value_name = "RUN_DIR")]
    pub view: Option<PathBuf>,

    /// Disable the live TUI viewer that otherwise attaches during a run.
    /// Plain-text progress (and orchestrator error output) is printed to
    /// the terminal instead. Pareto selection at the end still prompts
    /// interactively unless `--yes` is passed.
    #[arg(long)]
    pub no_ui: bool,

    /// Auto-apply the best Pareto candidate at the end of a run without
    /// prompting. Implied in non-interactive contexts.
    #[arg(long)]
    pub yes: bool,

    /// Verbose progress output
    #[arg(long)]
    pub verbose: bool,
}

impl OptimizeArgs {
    pub fn run(&self) -> Result<crate::ExitCode> {
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        rt.block_on(self.run_async())
    }

    async fn run_async(&self) -> Result<crate::ExitCode> {
        // Viewer mode: open the TUI against an existing run and exit.
        if let Some(view_dir) = &self.view {
            let canon = std::fs::canonicalize(view_dir)
                .with_context(|| format!("cannot resolve view dir: {}", view_dir.display()))?;
            baml_optimize::run_tui(&canon).context("TUI viewer failed")?;
            return Ok(crate::ExitCode::Success);
        }

        // Resume short-circuits — we just print the saved summary and exit.
        if let Some(resume_dir) = &self.resume {
            return resume_from_run_dir(resume_dir);
        }

        let from = std::fs::canonicalize(&self.from)
            .with_context(|| format!("Could not resolve path: {}", self.from.display()))?;

        let mut db = ProjectDatabase::new();
        let project = db.set_project_root(&from);

        let baml_files = discover_baml_files(&from);
        if baml_files.is_empty() {
            eprintln!("No .baml files found in {}", from.display());
            return Ok(crate::ExitCode::Other);
        }
        if self.verbose {
            println!(
                "Discovered {} .baml file(s) under {}",
                baml_files.len(),
                from.display()
            );
            for (i, p) in baml_files.iter().enumerate().take(10) {
                println!("  [{i}] {}", p.display());
            }
            if baml_files.len() > 10 {
                println!("  ... and {} more", baml_files.len() - 10);
            }
        }
        for file_path in &baml_files {
            let content = std::fs::read_to_string(file_path)
                .with_context(|| format!("failed to read {}", file_path.display()))?;
            db.add_or_update_file(file_path, &content);
        }

        // Bail if the sources don't compile cleanly.
        let source_files = db.get_source_files();
        let diagnostics = baml_project::collect_diagnostics(&db, project, &source_files);
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        if !errors.is_empty() {
            const MAX_SHOWN: usize = 20;
            eprintln!("Compilation errors found ({}):", errors.len());
            for d in errors.iter().take(MAX_SHOWN) {
                eprintln!("  error: {}", d.message);
            }
            if errors.len() > MAX_SHOWN {
                eprintln!("  ... and {} more errors", errors.len() - MAX_SHOWN);
            }
            return Ok(crate::ExitCode::Other);
        }

        let initial_function = initial_function_from_db(&db, &self.function)
            .with_context(|| format!("failed to locate function '{}'", self.function))?;

        let compile_options = baml_compiler2_emit::CompileOptions {
            emit_test_cases: true,
        };
        let bytecode = baml_compiler2_emit::generate_project_bytecode(&db, &compile_options)
            .map_err(|e| anyhow!("compilation failed: {e:?}"))?;

        let engine = BexEngine::new(
            bytecode,
            Arc::new(sys_native::SysOps::native()),
            None,
            Vec::new(),
        )
        .map_err(|e| anyhow!("failed to create engine: {e:?}"))?;
        let engine = Arc::new(engine);

        // Discovery in two halves: legacy `test { ... }` blocks come
        // from the HIR item tree (no engine needed); new-style
        // `testset { }` tests live in a runtime-built registry and
        // require the engine to materialise.
        //
        // Testset tests don't carry a target function name in their
        // registered path (their body can call any function), so we
        // stamp `--function` onto each of them here. This keeps
        // per-testset scoring + display consistent with legacy tests,
        // and lets the existing include/exclude filter match on name.
        let mut tests = discover_all_tests(&db, &[self.function.clone()]);
        let (_registry, mut testset_tests) = baml_optimize::discover_testset_tests(&engine)
            .await
            .context("failed to discover testset tests")?;
        for t in &mut testset_tests {
            t.function_name = self.function.clone();
        }
        tests.extend(testset_tests);

        // Keep only testset tests whose body actually calls the target
        // function (via LSP find-references). Legacy tests already
        // declare their target explicitly via `functions [...]`, so they
        // pass through untouched. Tests with dynamically-named testsets
        // are also kept — we can't prove they don't call the function.
        let (retained, dropped) =
            baml_optimize::retain_tests_calling_function(&db, tests, &self.function);
        tests = retained;
        if dropped > 0 && self.verbose {
            println!(
                "Dropped {dropped} testset test(s) that don't reference {}",
                self.function
            );
        }
        let discovered = tests.len();

        // Apply include/exclude filters (shared with `baml-cli test`).
        if !self.include.is_empty() || !self.exclude.is_empty() {
            let filter = crate::test_filter::TestFilter::new(
                self.include.iter().map(String::as_str),
                self.exclude.iter().map(String::as_str),
            );
            tests.retain(|t| filter.includes(&t.function_name, &t.test_name));
        }

        if tests.is_empty() {
            if discovered == 0 {
                eprintln!("No tests found for function: {}", self.function);
            } else {
                eprintln!(
                    "No tests selected for function '{}' — all {} were filtered out.",
                    self.function, discovered
                );
            }
            return Ok(crate::ExitCode::Other);
        }
        if tests.len() < discovered {
            println!(
                "Found {} tests for function '{}' ({} selected after filters)",
                discovered,
                self.function,
                tests.len(),
            );
        } else {
            println!(
                "Found {} tests for function '{}'",
                tests.len(),
                self.function
            );
        }

        let parsed = parse_objectives(&self.weights);
        let objectives = if parsed.is_empty() {
            vec![Objective::new(
                "accuracy",
                baml_optimize::Direction::Maximize,
                1.0,
            )]
        } else {
            parsed
        };

        if self.verbose {
            println!("Objectives:");
            for o in &objectives {
                println!(
                    "  - {} ({:?}, weight={})",
                    o.name, o.direction, o.weight
                );
            }
        }

        let config = OrchestratorConfig {
            function_name: self.function.clone(),
            max_iterations: self.max_iterations,
            parallel: self.parallel,
            objectives,
            merge_every: 3,
            verbose: self.verbose,
        };

        let mut orchestrator =
            GEPAOrchestrator::new(config, engine, from.clone(), tests, initial_function)?;

        // Launch the live TUI in a background thread (default behavior
        // unless --no-ui). The orchestrator and TUI communicate through
        // files inside the run directory (`stop_requested`,
        // `apply_request.json`), so no shared state crosses the thread
        // boundary.
        let tui_handle = if self.no_ui {
            None
        } else {
            let run_dir = orchestrator_run_dir(&orchestrator).to_path_buf();
            println!("Launching live TUI viewer (press 'q' to close, Enter to apply and stop)...");
            // Give the storage dir a beat to settle before the TUI opens it.
            std::thread::sleep(std::time::Duration::from_millis(100));
            Some(std::thread::spawn(move || {
                if let Err(e) = baml_optimize::run_tui_live(&run_dir) {
                    eprintln!("TUI error: {e}");
                }
            }))
        };

        let run_result = orchestrator.run().await;

        // Whatever happened, tear the TUI thread down before printing
        // anything else so it doesn't clobber our output.
        if let Some(handle) = tui_handle {
            let _ = handle.join();
        }

        let result = match run_result {
            Ok(r) => r,
            Err(e) => {
                // Record the error so a lingering TUI attached to the same
                // dir can surface it. Failure here is best-effort.
                let _ = std::fs::write(
                    std::path::Path::new(".baml_optimize").join("last_error.txt"),
                    format!("{e:?}"),
                );
                return Err(e);
            }
        };

        println!("\n=== Optimization Complete ===");
        if result.stopped_early {
            println!("(stopped early via TUI signal)");
        }
        println!("Function: {}", result.function_name);
        println!("Best candidate: #{}", result.best_candidate_id);
        println!(
            "Pass rate: {:.1}% ({}/{})",
            result.best_scores.test_pass_rate * 100.0,
            result.best_scores.tests_passed,
            result.best_scores.tests_total,
        );
        println!("Avg latency: {:.0}ms", result.best_scores.avg_latency_ms);
        println!("Total iterations: {}", result.total_iterations);
        println!("Total evaluations: {}", result.total_evaluations);
        println!("Pareto frontier size: {}", result.pareto_frontier_size);
        println!("Run dir: {}", result.run_dir.display());

        // Decide which candidate to apply back into the user's baml_src.
        // 1. If the TUI wrote an apply request, honor it.
        // 2. Else if --yes or only one Pareto candidate, pick the best.
        // 3. Else prompt the user with the Pareto frontier.
        let apply_id = if let Some(id) = baml_optimize::read_apply_request(&result.run_dir) {
            Some(id)
        } else if result.pareto_frontier.is_empty() {
            None
        } else if self.yes {
            Some(result.best_candidate_id)
        } else {
            // Load objectives for display
            let objectives = baml_optimize::Storage::load(result.run_dir.clone())
                .ok()
                .and_then(|s| s.load_config().ok())
                .map(|c| c.objectives)
                .unwrap_or_default();
            baml_optimize::display_pareto_and_select(
                &result.candidates,
                &result.pareto_frontier,
                &objectives,
                &result.function_name,
            )
        };

        if let Some(id) = apply_id {
            match apply_candidate_to_disk(&from, &result.candidates, id) {
                Ok(()) => println!("Applied candidate #{id} to {}", from.display()),
                Err(e) => eprintln!("Failed to apply candidate #{id}: {e:?}"),
            }
        } else {
            println!("No candidate applied. Inspect the run with:");
            println!("  baml-cli optimize --view {}", result.run_dir.display());
        }

        Ok(crate::ExitCode::Success)
    }
}

/// Borrow the orchestrator's run directory. The orchestrator owns its
/// `Storage`; this helper keeps the CLI from having to care where the
/// dir lives beyond the returned path.
fn orchestrator_run_dir(orch: &GEPAOrchestrator) -> &Path {
    orch.run_dir()
}

/// Overwrite `baml_src` under `root` with the sources that produced
/// candidate `id`. Reads the current tree, applies the candidate's
/// prompt/schema changes through [`Applier`], and writes each file back.
fn apply_candidate_to_disk(root: &Path, candidates: &[Candidate], id: usize) -> Result<()> {
    let candidate = candidates
        .iter()
        .find(|c| c.id == id)
        .with_context(|| format!("candidate #{id} not found"))?;

    let sources = baml_optimize::engine_build::read_sources(root)
        .with_context(|| format!("failed to read sources under {}", root.display()))?;
    let applier = Applier::new(root.to_path_buf());
    let modified = applier
        .generate_modified_files(candidate, &sources)
        .context("failed to generate modified files for candidate")?;

    for (rel, content) in &modified {
        if sources.get(rel).map(|s| s == content).unwrap_or(false) {
            continue;
        }
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Inspect a prior run directory and print a summary of where it ended up.
///
/// Reads `config.json`, `state.json`, and (if present) `final_results.json`.
/// The reflection loop is still stubbed in Phase 8, so "resume" today just
/// surfaces the saved state — continuation across runs lands with the loop.
fn resume_from_run_dir(run_dir: &std::path::Path) -> Result<crate::ExitCode> {
    use baml_optimize::Storage;

    let canon = std::fs::canonicalize(run_dir)
        .with_context(|| format!("cannot resolve resume dir: {}", run_dir.display()))?;
    let storage = Storage::load(canon.clone())
        .with_context(|| format!("failed to open run dir: {}", canon.display()))?;

    println!("=== Resuming from {} ===", storage.run_dir().display());

    let config_path = storage.run_dir().join("config.json");
    if let Ok(text) = std::fs::read_to_string(&config_path) {
        match serde_json::from_str::<baml_optimize::RunConfig>(&text) {
            Ok(cfg) => {
                println!("Function: {}", cfg.function_name);
                println!("Max iterations: {}", cfg.max_iterations);
                println!("Parallel: {}", cfg.parallel);
                if !cfg.objectives.is_empty() {
                    println!("Objectives:");
                    for o in &cfg.objectives {
                        println!("  - {} ({}, weight={})", o.name, o.direction, o.weight);
                    }
                }
            }
            Err(e) => eprintln!("warning: could not parse config.json: {e}"),
        }
    } else {
        eprintln!("warning: config.json not found in {}", storage.run_dir().display());
    }

    match storage.load_checkpoint()? {
        Some(state) => {
            println!(
                "\nLast checkpoint: iteration {}, {} candidate(s), {} evaluation(s)",
                state.current_iteration,
                state.candidate_ids.len(),
                state.total_evals,
            );
            println!(
                "Pareto frontier: {:?}",
                state.pareto_frontier
            );
        }
        None => {
            eprintln!("warning: no state.json in run directory — run did not checkpoint");
        }
    }

    let final_path = storage.run_dir().join("final_results.json");
    if final_path.exists() {
        match std::fs::read_to_string(&final_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        {
            Some(blob) => {
                println!("\nFinal results:");
                println!("{}", serde_json::to_string_pretty(&blob).unwrap_or_default());
            }
            None => eprintln!("warning: could not parse final_results.json"),
        }
    } else {
        println!("\n(No final_results.json — run may not have finished.)");
    }

    println!(
        "\nResume is read-only in this phase. The reflection loop will \
         pick up from this checkpoint once Phase 10 lands."
    );
    Ok(crate::ExitCode::Success)
}
