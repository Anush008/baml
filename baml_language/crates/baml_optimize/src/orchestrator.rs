//! GEPA orchestrator — wires together evaluator, GEPA runtime, applier,
//! Pareto frontier, and storage into the full optimization loop.
//!
//! This is the scaffolding for the full loop. The propose-apply-evaluate
//! inner step is gated on two pieces that are currently stubbed —
//! [`GEPARuntime::propose_improvements`] and the
//! [`Applier`] source-rewriting code. Until those land, `run()` evaluates
//! the baseline candidate, writes a checkpoint, and exits with a clear
//! message. This lets us exercise the plumbing end-to-end
//! (test discovery → compile → engine → evaluator → Pareto → storage)
//! against real BAML projects today.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bex_engine::BexEngine;

use crate::applier::Applier;
use crate::candidate::{Candidate, CandidateMethod, CandidateScores, OptimizableFunction};
use crate::discovery::DiscoveredTest;
use crate::evaluator::Evaluator;
use crate::gepa_runtime::GEPARuntime;
use crate::pareto::{Objective, ParetoFrontier};
use crate::schema_extractor::extract_optimizable_function;
use crate::storage::{ObjectiveConfig, RunConfig, RunState, Storage};

/// Caller-supplied configuration for an optimization run.
pub struct OrchestratorConfig {
    pub function_name: String,
    pub max_iterations: u32,
    pub parallel: usize,
    pub objectives: Vec<Objective>,
    pub verbose: bool,
}

/// Summary produced when a run finishes.
pub struct OptimizationResult {
    pub function_name: String,
    pub best_candidate_id: usize,
    pub best_scores: CandidateScores,
    pub total_iterations: usize,
    pub total_evaluations: usize,
    pub pareto_frontier_size: usize,
}

/// Main GEPA orchestrator.
pub struct GEPAOrchestrator {
    config: OrchestratorConfig,
    engine: Arc<BexEngine>,
    #[allow(dead_code)]
    gepa_runtime: GEPARuntime,
    evaluator: Evaluator,
    #[allow(dead_code)]
    applier: Applier,
    pareto: ParetoFrontier,
    storage: Storage,
    candidates: Vec<Candidate>,
    tests: Vec<DiscoveredTest>,
    initial_function: OptimizableFunction,
    current_iteration: usize,
    total_evals: usize,
}

impl GEPAOrchestrator {
    /// Create a new orchestrator, initialising the GEPA runtime and writing
    /// `config.json` to the run directory.
    pub fn new(
        config: OrchestratorConfig,
        engine: Arc<BexEngine>,
        root_path: PathBuf,
        tests: Vec<DiscoveredTest>,
        initial_function: OptimizableFunction,
    ) -> Result<Self> {
        let gepa_runtime =
            GEPARuntime::new().context("failed to initialize GEPA runtime")?;

        let evaluator = Evaluator::new(config.parallel);
        let applier = Applier::new(root_path.clone());
        let pareto = ParetoFrontier::new(config.objectives.clone());
        let storage = Storage::new(&root_path)?;

        let run_config = RunConfig {
            function_name: config.function_name.clone(),
            max_iterations: config.max_iterations,
            parallel: config.parallel,
            objectives: config
                .objectives
                .iter()
                .map(|o| ObjectiveConfig {
                    name: o.name.clone(),
                    direction: format!("{:?}", o.direction),
                    weight: o.weight,
                })
                .collect(),
        };
        storage.save_config(&run_config)?;

        Ok(Self {
            config,
            engine,
            gepa_runtime,
            evaluator,
            applier,
            pareto,
            storage,
            candidates: Vec::new(),
            tests,
            initial_function,
            current_iteration: 0,
            total_evals: 0,
        })
    }

    /// Drive the optimization run to completion.
    ///
    /// Today: evaluate the baseline candidate, save a checkpoint, and
    /// return. The iterative loop (propose → apply → evaluate → update
    /// frontier) is gated on the GEPA runtime and applier being fully
    /// implemented.
    pub async fn run(&mut self) -> Result<OptimizationResult> {
        self.initialize().await?;

        if self.config.max_iterations == 0 {
            // Caller opted out of iteration.
        } else if self.config.verbose {
            println!(
                "\nReflection loop not yet implemented — GEPA runtime and applier \
                 are stubbed. Baseline evaluation complete."
            );
        }

        self.save_checkpoint()?;
        self.finalize()
    }

    /// Seed the run with the baseline candidate and evaluate it.
    async fn initialize(&mut self) -> Result<()> {
        if self.config.verbose {
            println!(
                "Initializing optimization for function: {}",
                self.config.function_name
            );
        }

        let candidate = Candidate {
            id: 0,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: self.initial_function.clone(),
            scores: None,
            rationale: Some("Baseline from source".to_string()),
        };
        self.candidates.push(candidate);

        if self.config.verbose {
            println!(
                "Evaluating baseline on {} tests ({} in parallel)...",
                self.tests.len(),
                self.config.parallel
            );
        }

        let (scores, _results) = self
            .evaluator
            .evaluate(self.engine.clone(), &self.tests)
            .await?;

        self.candidates[0].scores = Some(scores.clone());
        self.pareto.add(0, &scores, &self.candidates);
        self.total_evals += self.tests.len();

        self.storage.save_candidate(&self.candidates[0])?;

        if self.config.verbose {
            println!(
                "Baseline: {:.1}% pass rate ({}/{}), {:.0}ms avg latency",
                scores.test_pass_rate * 100.0,
                scores.tests_passed,
                scores.tests_total,
                scores.avg_latency_ms,
            );
        }

        Ok(())
    }

    fn save_checkpoint(&self) -> Result<()> {
        let state = RunState {
            function_name: self.config.function_name.clone(),
            current_iteration: self.current_iteration,
            total_evals: self.total_evals,
            candidate_ids: (0..self.candidates.len()).collect(),
            pareto_frontier: self.pareto.frontier().to_vec(),
        };
        self.storage.save_checkpoint(&state)
    }

    fn finalize(&self) -> Result<OptimizationResult> {
        let best_id = self
            .pareto
            .select_for_reflection(&self.candidates)
            .unwrap_or(0);
        let best_scores = self
            .candidates
            .get(best_id)
            .and_then(|c| c.scores.clone())
            .unwrap_or_default();

        self.storage.save_final_results(best_id, &best_scores)?;

        Ok(OptimizationResult {
            function_name: self.config.function_name.clone(),
            best_candidate_id: best_id,
            best_scores,
            total_iterations: self.current_iteration,
            total_evaluations: self.total_evals,
            pareto_frontier_size: self.pareto.frontier().len(),
        })
    }
}

/// Helper for the CLI: walk the project HIR to build the initial
/// [`OptimizableFunction`]. Returns an empty-prompt placeholder when the
/// prompt/type extraction is still stubbed — real extraction lands in a
/// follow-on.
pub fn initial_function_from_db(
    db: &baml_project::ProjectDatabase,
    function_name: &str,
) -> Result<OptimizableFunction> {
    extract_optimizable_function(db, function_name)
}
