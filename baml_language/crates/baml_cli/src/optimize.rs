#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use baml_optimize::discover_all_tests;
use baml_project::ProjectDatabase;
use baml_workspace::discover_baml_files;
use clap::Args;

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
}

impl OptimizeArgs {
    pub fn run(&self) -> Result<crate::ExitCode> {
        let from = std::fs::canonicalize(&self.from)
            .with_context(|| format!("Could not resolve path: {}", self.from.display()))?;

        // Set up compiler database
        let mut db = ProjectDatabase::new();
        let _project = db.set_project_root(&from);

        let baml_files = discover_baml_files(&from);
        if baml_files.is_empty() {
            eprintln!("No .baml files found in {}", from.display());
            return Ok(crate::ExitCode::Other);
        }

        for file_path in &baml_files {
            let content = std::fs::read_to_string(file_path)
                .with_context(|| format!("Failed to read {}", file_path.display()))?;
            db.add_or_update_file(file_path, &content);
        }

        // Discover tests for the target function
        let filter = vec![self.function.clone()];
        let tests = discover_all_tests(&db, &filter);

        if tests.is_empty() {
            eprintln!("No tests found for function: {}", self.function);
            return Ok(crate::ExitCode::Other);
        }

        println!("Found {} tests for function '{}':", tests.len(), self.function);
        for test in &tests {
            println!("  {}::{}", test.function_name, test.test_name);
        }

        println!("\nOptimization loop not yet implemented.");
        Ok(crate::ExitCode::Success)
    }
}
