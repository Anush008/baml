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
    /// `"accuracy:testset_a=0.5"`
    #[arg(long, default_value = "accuracy=1.0")]
    pub weights: String,

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

        let tests = discover_all_tests(&db, &[self.function.clone()]);
        if tests.is_empty() {
            eprintln!("No tests found for function: {}", self.function);
            return Ok(crate::ExitCode::Other);
        }
        println!(
            "Found {} tests for function '{}'",
            tests.len(),
            self.function
        );

        let initial_function = initial_function_from_db(&db, &self.function)
            .with_context(|| format!("failed to locate function '{}'", self.function))?;

        let compile_options = baml_compiler2_emit::CompileOptions {
            emit_test_cases: true,
        };
        let bytecode = baml_compiler2_emit::generate_project_bytecode(&db, &compile_options)
            .map_err(|e| anyhow!("compilation failed: {e:?}"))?;

        let engine = BexEngine::new(bytecode, Arc::new(sys_native::SysOps::native()), None)
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
