#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use baml_db::{baml_compiler2_emit, baml_compiler_diagnostics::Severity};
use baml_optimize::orchestrator::initial_function_from_db;
use baml_optimize::{
    GEPAOrchestrator, Objective, OrchestratorConfig, discover_all_tests, parse_objectives,
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

        let mut tests = discover_all_tests(&db, &[self.function.clone()]);
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
            GEPAOrchestrator::new(config, engine, from, tests, initial_function)?;
        let result = orchestrator.run().await?;

        println!("\n=== Optimization Complete ===");
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

        Ok(crate::ExitCode::Success)
    }
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
