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
pub mod value_bridge;

// Re-exports
pub use applier::Applier;
pub use candidate::{
    Candidate, CandidateScores, CurrentMetrics, ImprovedFunction, ObjectiveStatus,
    OptimizableFunction, OptimizationObjectives, ReflectiveExample,
};
pub use discovery::{DiscoveredTest, discover_all_tests, group_by_testset};
pub use evaluator::{Evaluator, TestResult};
pub use gepa_runtime::GEPARuntime;
pub use orchestrator::{GEPAOrchestrator, OptimizationResult, OrchestratorConfig};
pub use pareto::{Direction, Objective, ParetoFrontier, parse_objectives};
pub use schema_extractor::extract_optimizable_function;
pub use storage::{ObjectiveConfig, RunConfig, RunState, Storage};
