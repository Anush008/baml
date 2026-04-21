//! GEPA orchestrator — wires together evaluator, GEPA runtime, applier,
//! Pareto frontier, and storage into the full optimization loop.
//!
//! `run()` flow:
//! 1. Evaluate the baseline candidate.
//! 2. Until `max_iterations` (or objectives say stop):
//!    - Select a parent from the Pareto frontier.
//!    - Build `ReflectiveExample`s from the parent's recent test results.
//!    - Call `GEPARuntime::propose_improvements` to get an `ImprovedFunction`.
//!    - Apply the improvement to the project sources via `Applier`.
//!    - Rebuild the engine from the modified sources.
//!    - Re-run tests on the modified engine; score the new candidate.
//!    - Offer the new candidate to the Pareto frontier.
//!    - Save a checkpoint.
//! 3. Pick the best frontier member and write `final_results.json`.
//!
//! Merge iterations (every N steps) and schema (class/enum) rewriting are
//! deferred — see the Phase 10b note in the port plan.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bex_engine::BexEngine;

use crate::applier::Applier;
use crate::candidate::{
    Candidate, CandidateMethod, CandidateScores, CurrentMetrics, ImprovedFunction, ObjectiveStatus,
    OptimizableFunction, OptimizationObjectives, ReflectiveExample,
};
use crate::discovery::DiscoveredTest;
use crate::engine_build::{build_engine_from_sources, read_sources};
use crate::evaluator::{Evaluator, TestResult};
use crate::gepa_runtime::GEPARuntime;
use crate::pareto::{Objective, ParetoFrontier};
use crate::schema_extractor::extract_optimizable_function;
use crate::storage::{ObjectiveConfig, RunConfig, RunState, Storage};
use crate::value_bridge::bex_to_json_value;

/// Caller-supplied configuration for an optimization run.
pub struct OrchestratorConfig {
    pub function_name: String,
    pub max_iterations: u32,
    pub parallel: usize,
    pub objectives: Vec<Objective>,
    /// Attempt a merge iteration every N iterations when the Pareto frontier
    /// has ≥ 2 members. `0` disables merging entirely. Default: `3`.
    pub merge_every: u32,
    pub verbose: bool,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            function_name: String::new(),
            max_iterations: 50,
            parallel: 8,
            objectives: Vec::new(),
            merge_every: 3,
            verbose: false,
        }
    }
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
    root_path: PathBuf,
    /// Original `baml_src/` contents, captured at construction time.
    base_sources: HashMap<PathBuf, String>,
    /// Live engine for the currently-evaluated candidate. Re-built after
    /// each apply+rebuild cycle.
    engine: Arc<BexEngine>,
    gepa_runtime: GEPARuntime,
    evaluator: Evaluator,
    applier: Applier,
    pareto: ParetoFrontier,
    storage: Storage,
    candidates: Vec<Candidate>,
    /// Most recent per-test results for each candidate, keyed by candidate
    /// id. Used to build reflection examples.
    last_results: HashMap<usize, Vec<TestResult>>,
    tests: Vec<DiscoveredTest>,
    initial_function: OptimizableFunction,
    current_iteration: usize,
    total_evals: usize,
}

impl GEPAOrchestrator {
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

        let base_sources = read_sources(&root_path)
            .with_context(|| format!("failed to read baml sources under {}", root_path.display()))?;
        storage.save_base_snapshot(&base_sources)?;

        Ok(Self {
            config,
            root_path,
            base_sources,
            engine,
            gepa_runtime,
            evaluator,
            applier,
            pareto,
            storage,
            candidates: Vec::new(),
            last_results: HashMap::new(),
            tests,
            initial_function,
            current_iteration: 0,
            total_evals: 0,
        })
    }

    /// Drive the optimization run to completion.
    pub async fn run(&mut self) -> Result<OptimizationResult> {
        self.initialize().await?;

        while self.current_iteration < self.config.max_iterations as usize {
            self.current_iteration += 1;
            let do_merge = self.should_merge_this_iteration();
            let result = if do_merge {
                self.run_merge_iteration().await
            } else {
                self.run_reflection_iteration().await
            };
            if let Err(e) = result {
                // Surface the error but don't abort the whole run — a flaky
                // LLM call shouldn't throw away the baseline and any
                // previously-accepted candidates.
                eprintln!(
                    "iteration {} failed: {e:?} — continuing with prior frontier",
                    self.current_iteration
                );
            }
            self.save_checkpoint()?;
        }

        self.finalize()
    }

    /// Decide whether this iteration should attempt a merge. True when
    /// `merge_every` is non-zero, the iteration index is a multiple of it,
    /// and the Pareto frontier has ≥ 2 members to merge.
    fn should_merge_this_iteration(&self) -> bool {
        let every = self.config.merge_every as usize;
        every > 0
            && self.current_iteration % every == 0
            && self.pareto.frontier().len() >= 2
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

        let (scores, results) = self
            .evaluator
            .evaluate(self.engine.clone(), &self.tests)
            .await?;

        self.candidates[0].scores = Some(scores.clone());
        self.pareto.add(0, &scores, &self.candidates);
        self.total_evals += self.tests.len();
        self.last_results.insert(0, results);

        self.storage.save_candidate(&self.candidates[0])?;
        self.save_checkpoint()?;

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

    /// One full reflection step: select → propose → apply → evaluate.
    async fn run_reflection_iteration(&mut self) -> Result<()> {
        let parent_id = self
            .pareto
            .select_for_reflection(&self.candidates)
            .context("pareto frontier is empty — nothing to reflect on")?;

        if self.config.verbose {
            println!(
                "\n--- Iteration {}/{} ---",
                self.current_iteration, self.config.max_iterations
            );
            println!("Selected parent candidate #{parent_id} for reflection");
        }

        let parent_results = self
            .last_results
            .get(&parent_id)
            .cloned()
            .unwrap_or_default();
        let failures = build_examples(&parent_results, false);
        let successes = build_examples(&parent_results, true);

        if failures.is_empty() && successes.is_empty() {
            anyhow::bail!("no test results recorded for parent candidate");
        }

        let parent_function = self.candidates[parent_id].function.clone();
        let objectives = build_objectives(&self.config.objectives, &self.candidates[parent_id]);
        let metrics = self.candidates[parent_id]
            .scores
            .as_ref()
            .map(build_metrics);

        let improved: ImprovedFunction = self
            .gepa_runtime
            .propose_improvements(
                &parent_function,
                &failures,
                &successes,
                &objectives,
                metrics.as_ref(),
            )
            .await
            .context("propose_improvements failed")?;

        if self.config.verbose {
            println!("Reflection rationale: {}", improved.rationale);
        }

        let new_function = OptimizableFunction {
            function_name: parent_function.function_name.clone(),
            prompt_text: improved.prompt_text.clone(),
            classes: improved.classes.clone(),
            enums: improved.enums.clone(),
            function_source: parent_function.function_source.clone(),
        };
        let new_candidate = Candidate {
            id: self.candidates.len(),
            iteration: self.current_iteration,
            parent_ids: vec![parent_id],
            method: CandidateMethod::Reflection,
            function: new_function,
            scores: None,
            rationale: Some(improved.rationale.clone()),
        };
        self.apply_and_evaluate(new_candidate).await
    }

    /// One merge step: select two Pareto parents → ask the reflection model
    /// to combine their strengths → apply and evaluate like any other
    /// candidate.
    async fn run_merge_iteration(&mut self) -> Result<()> {
        let (id_a, id_b) = self
            .pareto
            .select_for_merge(&self.candidates)
            .context("cannot merge: fewer than two frontier members")?;

        if self.config.verbose {
            println!(
                "\n--- Iteration {}/{} (merge) ---",
                self.current_iteration, self.config.max_iterations
            );
            println!("Merging candidates #{id_a} and #{id_b}");
        }

        let variant_a = self.candidates[id_a].function.clone();
        let variant_b = self.candidates[id_b].function.clone();
        let strengths_a = candidate_strengths(&self.candidates[id_a], &self.config.objectives);
        let strengths_b = candidate_strengths(&self.candidates[id_b], &self.config.objectives);

        let improved: ImprovedFunction = self
            .gepa_runtime
            .merge_variants(&variant_a, &variant_b, &strengths_a, &strengths_b)
            .await
            .context("merge_variants failed")?;

        if self.config.verbose {
            println!("Merge rationale: {}", improved.rationale);
        }

        // Preserve the function name / source-of-truth from variant A; the
        // merged function targets the same source location.
        let new_function = OptimizableFunction {
            function_name: variant_a.function_name.clone(),
            prompt_text: improved.prompt_text.clone(),
            classes: improved.classes.clone(),
            enums: improved.enums.clone(),
            function_source: variant_a.function_source.clone(),
        };
        let new_candidate = Candidate {
            id: self.candidates.len(),
            iteration: self.current_iteration,
            parent_ids: vec![id_a, id_b],
            method: CandidateMethod::Merge,
            function: new_function,
            scores: None,
            rationale: Some(improved.rationale.clone()),
        };
        self.apply_and_evaluate(new_candidate).await
    }

    /// Shared tail for reflection + merge iterations: rewrite sources,
    /// rebuild the engine, evaluate the candidate, update the Pareto
    /// frontier, persist artifacts.
    async fn apply_and_evaluate(&mut self, candidate: Candidate) -> Result<()> {
        let new_id = candidate.id;
        self.candidates.push(candidate);

        let modified_sources = self
            .applier
            .generate_modified_files(&self.candidates[new_id], &self.base_sources)
            .context("applier failed to generate modified sources")?;
        let changed = modified_sources
            .iter()
            .any(|(p, c)| self.base_sources.get(p).map_or(true, |orig| orig != c));
        if !changed {
            if self.config.verbose {
                println!(
                    "Candidate #{new_id} produced no source changes — skipping re-evaluation"
                );
            }
            return Ok(());
        }

        let new_engine = build_engine_from_sources(&self.root_path, &modified_sources)
            .context("failed to rebuild engine from modified sources")?;
        self.storage
            .save_candidate_patch(new_id, &self.base_sources, &modified_sources)?;

        let (scores, results) = self
            .evaluator
            .evaluate(new_engine.clone(), &self.tests)
            .await
            .context("failed to evaluate new candidate")?;

        self.total_evals += self.tests.len();
        self.candidates[new_id].scores = Some(scores.clone());
        self.last_results.insert(new_id, results);
        self.pareto.add(new_id, &scores, &self.candidates);
        self.storage.save_candidate(&self.candidates[new_id])?;

        if self.config.verbose {
            println!(
                "Candidate #{new_id}: {:.1}% pass rate ({}/{}), {:.0}ms avg latency",
                scores.test_pass_rate * 100.0,
                scores.tests_passed,
                scores.tests_total,
                scores.avg_latency_ms,
            );
            println!("Pareto frontier: {:?}", self.pareto.frontier());
        }

        if self.pareto.frontier().contains(&new_id) {
            self.engine = new_engine;
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

/// Build reflection examples from recent test results, selecting either
/// failures (`passed == false`) or successes.
fn build_examples(results: &[TestResult], successes_only: bool) -> Vec<ReflectiveExample> {
    results
        .iter()
        .filter(|r| r.passed == successes_only)
        .map(|r| {
            let inputs: HashMap<String, String> = r
                .input_args
                .iter()
                .map(|(k, v)| (k.clone(), render_value(v)))
                .collect();
            let generated_outputs: HashMap<String, String> = match &r.output {
                Some(v) => {
                    let mut m = HashMap::new();
                    m.insert("result".to_string(), render_value(v));
                    m
                }
                None => HashMap::new(),
            };
            let feedback = r.error.clone().unwrap_or_else(|| {
                if r.passed {
                    "ok".to_string()
                } else {
                    "test failed without a specific error".to_string()
                }
            });
            ReflectiveExample {
                inputs,
                generated_outputs,
                feedback,
                failure_location: if r.passed { None } else { Some("assertion".into()) },
                test_source: None,
                test_name: Some(r.test_name.clone()),
                prompt_tokens: r.input_tokens.map(|t| t as f64),
                completion_tokens: r.output_tokens.map(|t| t as f64),
                latency_ms: Some(r.latency_ms),
            }
        })
        .collect()
}

/// Render a `BexExternalValue` to a short string for the reflection prompt.
/// Serialising to JSON produces a stable, type-agnostic form the LLM can
/// read; primitives pass through cleanly.
fn render_value(value: &bex_external_types::BexExternalValue) -> String {
    match bex_to_json_value(value) {
        Ok(json) => json.to_string(),
        Err(_) => format!("<unrenderable:{}>", value.type_name()),
    }
}

/// Build an [`OptimizationObjectives`] view for the current parent
/// candidate, suitable for passing to `ProposeImprovements`.
fn build_objectives(objectives: &[Objective], parent: &Candidate) -> OptimizationObjectives {
    let scores = parent.scores.as_ref();
    let statuses: Vec<ObjectiveStatus> = objectives
        .iter()
        .map(|o| {
            let current_value = scores
                .map(|s| match o.name.as_str() {
                    "accuracy" => s.test_pass_rate,
                    "tokens" => s.avg_prompt_tokens + s.avg_completion_tokens,
                    "latency" => s.avg_latency_ms,
                    other => {
                        // Per-testset: "accuracy:testset_a" style.
                        if let Some((_, testset)) = other.split_once(':') {
                            *s.per_testset_scores.get(testset).unwrap_or(&0.0)
                        } else {
                            0.0
                        }
                    }
                })
                .unwrap_or(0.0);
            ObjectiveStatus {
                name: o.name.clone(),
                weight: o.weight,
                direction: format!("{:?}", o.direction).to_ascii_lowercase(),
                current_value,
                status: String::new(),
            }
        })
        .collect();
    OptimizationObjectives {
        objectives: statuses,
    }
}

/// Build a short list of human-readable "strengths" for a candidate,
/// suitable as input to `merge_variants`.
///
/// Today: one string per configured objective, naming it and the candidate's
/// current value for it. This is what the engine's equivalent does too —
/// the reflection model treats these as hints, not facts.
fn candidate_strengths(candidate: &Candidate, objectives: &[Objective]) -> Vec<String> {
    let Some(scores) = candidate.scores.as_ref() else {
        return Vec::new();
    };
    objectives
        .iter()
        .map(|o| match o.name.as_str() {
            "accuracy" => format!("accuracy={:.1}%", scores.test_pass_rate * 100.0),
            "tokens" => format!(
                "tokens={:.0}",
                scores.avg_prompt_tokens + scores.avg_completion_tokens
            ),
            "latency" => format!("latency={:.0}ms", scores.avg_latency_ms),
            other => {
                if let Some((_, testset)) = other.split_once(':') {
                    let v = scores.per_testset_scores.get(testset).copied().unwrap_or(0.0);
                    format!("{other}={:.2}", v)
                } else {
                    format!("{other}=0")
                }
            }
        })
        .collect()
}

fn build_metrics(scores: &CandidateScores) -> CurrentMetrics {
    CurrentMetrics {
        test_pass_rate: scores.test_pass_rate,
        tests_passed: scores.tests_passed as i64,
        tests_total: scores.tests_total as i64,
        avg_prompt_tokens: scores.avg_prompt_tokens,
        avg_completion_tokens: scores.avg_completion_tokens,
        avg_total_tokens: scores.avg_prompt_tokens + scores.avg_completion_tokens,
        avg_latency_ms: scores.avg_latency_ms,
    }
}

/// Helper for the CLI: walk the project HIR to build the initial
/// [`OptimizableFunction`].
pub fn initial_function_from_db(
    db: &baml_project::ProjectDatabase,
    function_name: &str,
) -> Result<OptimizableFunction> {
    extract_optimizable_function(db, function_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pareto::Direction;
    use bex_external_types::BexExternalValue;
    use indexmap::IndexMap;

    fn result(passed: bool, sentence: &str, error: Option<&str>) -> TestResult {
        let mut args: IndexMap<String, BexExternalValue> = IndexMap::new();
        args.insert(
            "sentence".to_string(),
            BexExternalValue::String(sentence.to_string()),
        );
        TestResult {
            function_name: "ExtractSubject".into(),
            test_name: "Test1".into(),
            testset_name: None,
            passed,
            error: error.map(ToString::to_string),
            latency_ms: 120.0,
            input_tokens: Some(30),
            output_tokens: Some(12),
            input_args: args,
            output: if passed {
                Some(BexExternalValue::String("Meg".into()))
            } else {
                None
            },
        }
    }

    #[test]
    fn build_examples_partitions_by_pass_fail() {
        let results = vec![
            result(false, "failing input", Some("assert failed: this != null")),
            result(true, "successful input", None),
        ];
        let failures = build_examples(&results, false);
        let successes = build_examples(&results, true);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].inputs.get("sentence").unwrap(), "\"failing input\"");
        assert_eq!(
            failures[0].feedback,
            "assert failed: this != null"
        );
        assert_eq!(failures[0].failure_location.as_deref(), Some("assertion"));
        assert!(failures[0].generated_outputs.is_empty());

        assert_eq!(successes.len(), 1);
        assert_eq!(successes[0].feedback, "ok");
        assert_eq!(
            successes[0].generated_outputs.get("result").unwrap(),
            "\"Meg\""
        );
    }

    #[test]
    fn build_objectives_reflects_parent_scores() {
        let objectives = vec![
            Objective::new("accuracy", Direction::Maximize, 1.0),
            Objective::new("latency", Direction::Minimize, 0.5),
        ];
        let parent = Candidate {
            id: 0,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "F".into(),
                prompt_text: String::new(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: Some(CandidateScores {
                test_pass_rate: 0.75,
                tests_passed: 3,
                tests_total: 4,
                avg_prompt_tokens: 50.0,
                avg_completion_tokens: 20.0,
                avg_latency_ms: 180.0,
                per_testset_scores: HashMap::new(),
            }),
            rationale: None,
        };
        let out = build_objectives(&objectives, &parent);
        assert_eq!(out.objectives.len(), 2);
        assert_eq!(out.objectives[0].name, "accuracy");
        assert!((out.objectives[0].current_value - 0.75).abs() < 1e-9);
        assert_eq!(out.objectives[0].direction, "maximize");
        assert_eq!(out.objectives[1].name, "latency");
        assert!((out.objectives[1].current_value - 180.0).abs() < 1e-9);
        assert_eq!(out.objectives[1].direction, "minimize");
    }

    #[test]
    fn candidate_strengths_names_configured_objectives() {
        let objectives = vec![
            Objective::new("accuracy", Direction::Maximize, 1.0),
            Objective::new("latency", Direction::Minimize, 0.5),
            Objective::new("tokens", Direction::Minimize, 0.2),
        ];
        let candidate = Candidate {
            id: 1,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "F".into(),
                prompt_text: String::new(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: Some(CandidateScores {
                test_pass_rate: 0.8,
                tests_passed: 4,
                tests_total: 5,
                avg_prompt_tokens: 40.0,
                avg_completion_tokens: 10.0,
                avg_latency_ms: 250.0,
                per_testset_scores: HashMap::new(),
            }),
            rationale: None,
        };
        let strengths = candidate_strengths(&candidate, &objectives);
        assert_eq!(
            strengths,
            vec![
                "accuracy=80.0%".to_string(),
                "latency=250ms".to_string(),
                "tokens=50".to_string(),
            ]
        );
    }

    #[test]
    fn build_metrics_sums_tokens() {
        let scores = CandidateScores {
            test_pass_rate: 0.5,
            tests_passed: 2,
            tests_total: 4,
            avg_prompt_tokens: 100.0,
            avg_completion_tokens: 50.0,
            avg_latency_ms: 250.0,
            per_testset_scores: HashMap::new(),
        };
        let m = build_metrics(&scores);
        assert_eq!(m.tests_passed, 2);
        assert_eq!(m.tests_total, 4);
        assert!((m.avg_total_tokens - 150.0).abs() < 1e-9);
    }
}
