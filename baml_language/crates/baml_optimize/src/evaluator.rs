//! Evaluator - runs tests in parallel and collects metrics.
//!
//! The evaluator executes a batch of [`DiscoveredTest`]s against a
//! [`BexEngine`], bounded by a [`Semaphore`] so at most `parallel` tests run
//! concurrently. Per-test metrics (pass/fail, latency, token usage) are
//! aggregated into [`CandidateScores`].

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};
use bex_engine::{
    BexEngine, BexExternalValue, CallId, FunctionCallContextBuilder, test_arg_to_external,
};
use bex_events::Collector;
use tokio::sync::Semaphore;

use crate::candidate::CandidateScores;
use crate::discovery::DiscoveredTest;

/// Result of a single test execution.
#[derive(Clone, Debug)]
pub struct TestResult {
    pub function_name: String,
    pub test_name: String,
    pub testset_name: Option<String>,
    pub passed: bool,
    pub error: Option<String>,
    pub latency_ms: f64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

/// Runs tests in parallel and aggregates scores.
pub struct Evaluator {
    pub parallel: usize,
}

impl Evaluator {
    pub fn new(parallel: usize) -> Self {
        let parallel = parallel.max(1);
        Self { parallel }
    }

    /// Evaluate all tests and return aggregated scores plus individual results.
    ///
    /// Tests run concurrently, bounded by `self.parallel`.
    pub async fn evaluate(
        &self,
        engine: Arc<BexEngine>,
        tests: &[DiscoveredTest],
    ) -> Result<(CandidateScores, Vec<TestResult>)> {
        let semaphore = Arc::new(Semaphore::new(self.parallel));
        let mut handles = Vec::with_capacity(tests.len());

        for test in tests {
            let semaphore = semaphore.clone();
            let engine = engine.clone();
            let func_name = test.function_name.clone();
            let test_name = test.test_name.clone();
            let testset_name = test.testset_name.clone();

            handles.push(tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|e| anyhow!("semaphore closed: {e}"))?;
                run_single_test(&engine, &func_name, &test_name, testset_name).await
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            results.push(handle.await.map_err(|e| anyhow!("task join failed: {e}"))??);
        }

        let scores = CandidateScores::from_test_results(&results);
        Ok((scores, results))
    }
}

async fn run_single_test(
    engine: &Arc<BexEngine>,
    func_name: &str,
    test_name: &str,
    testset_name: Option<String>,
) -> Result<TestResult> {
    // Build ordered args and snapshot asserts before any .await so the
    // engine borrow is released before we hit the await point.
    let (ordered_args, asserts) = {
        let test_case = engine
            .test_case(func_name, test_name)
            .ok_or_else(|| anyhow!("test case not found: {func_name}::{test_name}"))?;
        let params = engine
            .function_params(func_name)
            .map_err(|e| anyhow!("failed to get params for {func_name}: {e:?}"))?;
        let ordered = params
            .into_iter()
            .map(|(name, _ty)| {
                test_case
                    .args
                    .get(name)
                    .map(test_arg_to_external)
                    .ok_or_else(|| anyhow!("missing argument '{name}' for {func_name}"))
            })
            .collect::<Result<Vec<BexExternalValue>>>()?;
        (ordered, test_case.asserts.clone())
    };

    let collector = Arc::new(Collector::new("optimize".into()));
    let ctx = FunctionCallContextBuilder::new(CallId::next())
        .with_collectors(vec![collector.clone()])
        .build();

    let start = Instant::now();
    let call_result = engine.call_function(func_name, ordered_args, ctx, true).await;
    #[allow(clippy::cast_precision_loss)]
    let latency_ms = start.elapsed().as_micros() as f64 / 1000.0;

    let usage = collector.usage();
    let (passed, error) = match call_result {
        Ok(result) => {
            let failures = evaluate_test_asserts(&asserts, &result);
            if failures.is_empty() {
                (true, None)
            } else {
                (false, Some(format!("assert failures: {}", failures.join("; "))))
            }
        }
        Err(e) => (false, Some(format!("{e:?}"))),
    };

    Ok(TestResult {
        function_name: func_name.to_string(),
        test_name: test_name.to_string(),
        testset_name,
        passed,
        error,
        latency_ms,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    })
}

/// Run every `@@assert` attached to a test against the function result.
///
/// Returns a list of human-readable failure descriptions — empty means all
/// assertions passed.
fn evaluate_test_asserts(
    asserts: &[bex_vm_types::TestAssertion],
    result: &BexExternalValue,
) -> Vec<String> {
    asserts
        .iter()
        .filter_map(|a| {
            let label = a
                .label
                .as_ref()
                .map_or_else(|| a.expr.clone(), |l| format!("{l}: {}", a.expr));
            match sys_llm::evaluate_assertion(&a.expr, result) {
                sys_llm::AssertOutcome::Passed => None,
                sys_llm::AssertOutcome::Failed { rendered } => {
                    Some(format!("{label} => {rendered}"))
                }
                sys_llm::AssertOutcome::Error { message } => {
                    Some(format!("{label} (error: {message})"))
                }
            }
        })
        .collect()
}

impl CandidateScores {
    /// Compute aggregate scores from individual test results.
    pub fn from_test_results(results: &[TestResult]) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();

        #[allow(clippy::cast_precision_loss)]
        let avg_latency_ms = if total > 0 {
            results.iter().map(|r| r.latency_ms).sum::<f64>() / total as f64
        } else {
            0.0
        };

        let (total_input, count_input) = results
            .iter()
            .filter_map(|r| r.input_tokens)
            .fold((0i64, 0usize), |(sum, count), t| (sum + t, count + 1));
        let (total_output, count_output) = results
            .iter()
            .filter_map(|r| r.output_tokens)
            .fold((0i64, 0usize), |(sum, count), t| (sum + t, count + 1));

        #[allow(clippy::cast_precision_loss)]
        let avg_prompt_tokens = if count_input > 0 {
            total_input as f64 / count_input as f64
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss)]
        let avg_completion_tokens = if count_output > 0 {
            total_output as f64 / count_output as f64
        } else {
            0.0
        };

        // Per-testset pass rates (tests without a testset grouped under "default").
        let mut testset_counts: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for r in results {
            let key = r
                .testset_name
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let entry = testset_counts.entry(key).or_insert((0, 0));
            entry.1 += 1;
            if r.passed {
                entry.0 += 1;
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let per_testset_scores = testset_counts
            .into_iter()
            .map(|(k, (p, t))| (k, if t > 0 { p as f64 / t as f64 } else { 0.0 }))
            .collect();

        #[allow(clippy::cast_precision_loss)]
        let test_pass_rate = if total > 0 {
            passed as f64 / total as f64
        } else {
            0.0
        };

        Self {
            test_pass_rate,
            tests_passed: passed,
            tests_total: total,
            avg_prompt_tokens,
            avg_completion_tokens,
            avg_latency_ms,
            per_testset_scores,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(
        func: &str,
        test: &str,
        testset: Option<&str>,
        passed: bool,
        latency_ms: f64,
        input: Option<i64>,
        output: Option<i64>,
    ) -> TestResult {
        TestResult {
            function_name: func.to_string(),
            test_name: test.to_string(),
            testset_name: testset.map(ToString::to_string),
            passed,
            error: if passed { None } else { Some("x".into()) },
            latency_ms,
            input_tokens: input,
            output_tokens: output,
        }
    }

    fn asserts(exprs: &[&str]) -> Vec<bex_vm_types::TestAssertion> {
        exprs
            .iter()
            .map(|e| bex_vm_types::TestAssertion {
                expr: (*e).to_string(),
                label: None,
            })
            .collect()
    }

    #[test]
    fn asserts_pass_on_non_null_instance() {
        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), BexExternalValue::String("Meg".into()));
        fields.insert("age".to_string(), BexExternalValue::Null);
        let result = BexExternalValue::Instance {
            class_name: "Person".into(),
            fields,
        };

        let failures = evaluate_test_asserts(
            &asserts(&["{{ this != null }}", "{{ this.name == \"Meg\" }}"]),
            &result,
        );
        assert!(failures.is_empty(), "expected no failures, got {failures:?}");
    }

    #[test]
    fn asserts_fail_against_null_result() {
        let failures =
            evaluate_test_asserts(&asserts(&["{{ this != null }}"]), &BexExternalValue::Null);
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].contains("this != null"),
            "failure should mention the expr, got {:?}",
            failures[0]
        );
    }

    #[test]
    fn empty_results_yield_zero_scores() {
        let scores = CandidateScores::from_test_results(&[]);
        assert_eq!(scores.tests_total, 0);
        assert_eq!(scores.tests_passed, 0);
        assert!((scores.test_pass_rate - 0.0).abs() < f64::EPSILON);
        assert!(scores.per_testset_scores.is_empty());
    }

    #[test]
    fn aggregate_scores_computed_correctly() {
        let results = vec![
            mk("F", "t1", Some("a"), true, 100.0, Some(10), Some(20)),
            mk("F", "t2", Some("a"), false, 200.0, Some(30), Some(40)),
            mk("F", "t3", Some("b"), true, 300.0, None, None),
        ];

        let scores = CandidateScores::from_test_results(&results);
        assert_eq!(scores.tests_total, 3);
        assert_eq!(scores.tests_passed, 2);
        assert!((scores.test_pass_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!((scores.avg_latency_ms - 200.0).abs() < 1e-9);
        // Only two results have tokens; averages are over the non-None subset.
        assert!((scores.avg_prompt_tokens - 20.0).abs() < 1e-9);
        assert!((scores.avg_completion_tokens - 30.0).abs() < 1e-9);

        assert!((scores.per_testset_scores["a"] - 0.5).abs() < 1e-9);
        assert!((scores.per_testset_scores["b"] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn missing_testset_groups_under_default() {
        let results = vec![
            mk("F", "t1", None, true, 10.0, None, None),
            mk("F", "t2", None, false, 10.0, None, None),
        ];
        let scores = CandidateScores::from_test_results(&results);
        assert!((scores.per_testset_scores["default"] - 0.5).abs() < 1e-9);
    }
}
