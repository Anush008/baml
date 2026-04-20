//! BAML Optimize - GEPA prompt optimization for baml_language
//!
//! Implements iterative LLM-driven prompt improvement with multi-objective
//! Pareto optimization.

pub mod candidate;
pub mod discovery;
pub mod evaluator;

// Re-exports
pub use candidate::{Candidate, CandidateScores, OptimizableFunction};
pub use discovery::{DiscoveredTest, discover_all_tests, group_by_testset};
pub use evaluator::Evaluator;
