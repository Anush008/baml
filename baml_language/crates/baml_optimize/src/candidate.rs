//! Candidate data structures for prompt/schema optimization

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Represents a field in a class schema that can be optimized
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaFieldDefinition {
    pub field_name: String,
    pub field_type: String,
    pub description: Option<String>,
    pub alias: Option<String>,
}

/// Represents a class definition with its fields
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassDefinition {
    pub class_name: String,
    pub description: Option<String>,
    pub fields: Vec<SchemaFieldDefinition>,
}

/// Represents an enum definition with value descriptions
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnumDefinition {
    pub enum_name: String,
    pub values: Vec<String>,
    pub value_descriptions: HashMap<String, String>,
}

/// The complete optimizable context for a function
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptimizableFunction {
    pub function_name: String,
    pub prompt_text: String,
    pub classes: Vec<ClassDefinition>,
    pub enums: Vec<EnumDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_source: Option<String>,
}

/// How a candidate was created
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CandidateMethod {
    Initial,
    Reflection,
    Merge,
}

/// Scores from evaluating a candidate
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CandidateScores {
    pub test_pass_rate: f64,
    pub tests_passed: usize,
    pub tests_total: usize,
    pub avg_prompt_tokens: f64,
    pub avg_completion_tokens: f64,
    pub avg_latency_ms: f64,
    /// Per-testset pass rates for secondary objectives
    pub per_testset_scores: HashMap<String, f64>,
}

/// A candidate prompt/schema version
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub id: usize,
    pub iteration: usize,
    pub parent_ids: Vec<usize>,
    pub method: CandidateMethod,
    pub function: OptimizableFunction,
    pub scores: Option<CandidateScores>,
    pub rationale: Option<String>,
}

/// A single test execution packaged for the reflection model: inputs, the
/// LLM's generated output, and feedback about why it passed or failed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReflectiveExample {
    pub inputs: HashMap<String, String>,
    pub generated_outputs: HashMap<String, String>,
    pub feedback: String,
    pub failure_location: Option<String>,
    pub test_source: Option<String>,
    pub test_name: Option<String>,
    pub prompt_tokens: Option<f64>,
    pub completion_tokens: Option<f64>,
    pub latency_ms: Option<f64>,
}

/// Collection of optimization objectives passed to the reflection function.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OptimizationObjectives {
    pub objectives: Vec<ObjectiveStatus>,
}

/// Status of a single optimization objective (e.g. accuracy, tokens, latency).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectiveStatus {
    pub name: String,
    pub weight: f64,
    /// `"maximize"` or `"minimize"`.
    pub direction: String,
    pub current_value: f64,
    pub status: String,
}

/// Current scalar metrics for the candidate being improved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CurrentMetrics {
    pub test_pass_rate: f64,
    pub tests_passed: i64,
    pub tests_total: i64,
    pub avg_prompt_tokens: f64,
    pub avg_completion_tokens: f64,
    pub avg_total_tokens: f64,
    pub avg_latency_ms: f64,
}

/// The result of a reflection call: an improved prompt and schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImprovedFunction {
    pub prompt_text: String,
    pub classes: Vec<ClassDefinition>,
    pub enums: Vec<EnumDefinition>,
    pub rationale: String,
}
