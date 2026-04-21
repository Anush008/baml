//! BAML Optimize - GEPA prompt optimization for baml_language
//!
//! Implements iterative LLM-driven prompt improvement with multi-objective
//! Pareto optimization.

pub mod applier;
pub mod candidate;
pub mod discovery;
pub mod engine_build;
pub mod evaluator;
mod gepa;
pub mod gepa_runtime;
pub mod orchestrator;
pub mod pareto;
pub mod schema_extractor;
pub mod storage;
pub mod testset_filter;
pub mod tui;
pub mod value_bridge;

// Re-exports
pub use applier::Applier;
pub use candidate::{
    Candidate, CandidateScores, CurrentMetrics, ImprovedFunction, ObjectiveStatus,
    OptimizableFunction, OptimizationObjectives, ReflectiveExample,
};
pub use discovery::{
    DiscoveredTest, TestKind, discover_all_tests, discover_testset_tests, group_by_testset,
};
pub use evaluator::{Evaluator, TestResult};
pub use gepa_runtime::GEPARuntime;
pub use orchestrator::{GEPAOrchestrator, OptimizationResult, OrchestratorConfig};
pub use pareto::{Direction, Objective, ParetoFrontier, parse_objectives};
pub use schema_extractor::extract_optimizable_function;
pub use storage::{
    FinalResults, ObjectiveConfig, RunConfig, RunState, Storage, is_stop_requested,
    read_apply_request, write_apply_request, write_stop_request,
};
pub use testset_filter::retain_tests_calling_function;
pub use tui::{App, display_pareto_and_select, run_tui, run_tui_live};
