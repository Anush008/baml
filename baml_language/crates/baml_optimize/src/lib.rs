//! BAML Optimize - GEPA prompt optimization for baml_language
//!
//! Implements iterative LLM-driven prompt improvement with multi-objective
//! Pareto optimization.

pub mod candidate;
pub mod evaluator;

// Re-exports
pub use candidate::{Candidate, CandidateScores, OptimizableFunction};
pub use evaluator::Evaluator;
