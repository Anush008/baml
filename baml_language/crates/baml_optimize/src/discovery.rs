//! Test discovery for optimization.
//!
//! Two test styles are supported:
//!
//! 1. **Legacy** — top-level `test Foo { functions [Bar]; args { ... };
//!    @@assert(...) }` blocks, stored as HIR item-tree `Test` entries.
//!    Discovery is purely static — walks every source file's
//!    `item_tree.tests`.
//!
//! 2. **New-style testset** — `testset "g" { test "t" { let r = F(...);
//!    assert.equal(...) } }`. These don't produce HIR `Test` items; the
//!    AST lowerer desugars them into a synthesized `$init_test`
//!    function that, at runtime, registers tests via
//!    `testing.TestRegistry.register_test` / `register_test_set`.
//!    Discovery here requires a live [`BexEngine`]: we call
//!    `engine.collect_tests("user", …)`, repeatedly `expand_set` every
//!    lazy (dynamic) testset, then walk the serialized tree for the
//!    leaf test paths. Mirrors the pipeline in `baml_cli::test_command`.
//!
//! [`discover_all_tests`] handles (1) without any engine; callers that
//! have an engine combine the legacy list with [`discover_testset_tests`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use baml_db::baml_compiler2_hir;
use baml_project::ProjectDatabase;
use bex_engine::{BexEngine, BexExternalValue, CallId, FunctionCallContextBuilder};
use tokio_util::sync::CancellationToken;

/// How a discovered test gets executed. Determines which evaluator
/// codepath runs the test and what signal we can recover for reflection.
#[derive(Clone, Debug)]
pub enum TestKind {
    /// Top-level `test Foo { functions [Bar] args { ... } @@assert(...) }`.
    /// Runs by calling `Bar(args)` directly; the function's return value
    /// and any `@@assert` expressions are evaluated in Rust.
    Legacy,
    /// Test declared inside a `testset { }` block. Runs via
    /// `testing.TestRegistry.run_test(registry, full_path)` — the registry
    /// drives the test's body, which contains the function call and
    /// `assert.*` calls. We get back a pass/fail outcome but not the
    /// function's return value, so reflection examples are limited to the
    /// test name and error message.
    Testset {
        /// Slash-separated path the runtime uses to identify the test,
        /// e.g. `"ExtractSubject/clear/single actor"`. Passed verbatim to
        /// `TestRegistry.run_test`.
        full_path: String,
    },
}

/// A discovered test with its metadata.
#[derive(Clone, Debug)]
pub struct DiscoveredTest {
    pub function_name: String,
    pub test_name: String,
    pub testset_name: Option<String>,
    pub file_path: PathBuf,
    pub kind: TestKind,
    /// Raw source text of the test's block — the full
    /// `test "..." { ... }` span. Populated for testset tests when
    /// `testset_filter::retain_tests_calling_function` can resolve the
    /// path statically; `None` otherwise (dynamic testsets, legacy
    /// tests). Used to feed test body visibility into
    /// `ReflectiveExample.test_source` so the reflection LLM can see
    /// what inputs the test passes and what it asserts.
    pub test_source: Option<String>,
}

/// Discover legacy (`test Foo { ... }`) tests for functions matching the
/// filter, walking the compiler2 HIR item tree. Does **not** include
/// new-style testset tests — callers that need those should also run
/// [`discover_testset_tests`] on a built engine.
///
/// If `function_filter` is non-empty, only tests whose `function_refs`
/// contain an entry that includes one of the filter strings are returned.
/// Deduplicates by `(function_name, test_name)` across files.
pub fn discover_all_tests(db: &ProjectDatabase, function_filter: &[String]) -> Vec<DiscoveredTest> {
    let mut tests = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for source_file in db.get_source_files() {
        let item_tree = baml_compiler2_hir::file_item_tree(db, source_file);
        let file_path = source_file.path(db);

        for (_id, test) in &item_tree.tests {
            for func_ref in &test.function_refs {
                let func_name = func_ref.to_string();
                if func_name.is_empty() {
                    continue;
                }
                let test_name = test.name.to_string();

                if !function_filter.is_empty()
                    && !function_filter.iter().any(|f| func_name.contains(f.as_str()))
                {
                    continue;
                }

                let key = (func_name.clone(), test_name.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);

                tests.push(DiscoveredTest {
                    function_name: func_name,
                    test_name,
                    testset_name: None,
                    file_path: file_path.clone(),
                    kind: TestKind::Legacy,
                    test_source: None,
                });
            }
        }
    }

    tests
}

/// Discover new-style testset tests by asking the engine to materialise
/// its `testing.TestRegistry` and walking the resulting serialized tree.
///
/// Returns `(Some(registry), tests)` when the project has tests — the
/// caller reuses the registry to execute them via `TestRegistry.run_test`.
/// Returns `(None, vec![])` when the project has no `$init_test`
/// function (no tests at all).
///
/// Testset tests don't embed the target function name in their
/// registered path (their body can call any function), so
/// `function_name` on every returned [`DiscoveredTest`] is empty. The
/// caller is expected to stamp its target function name (typically the
/// CLI's `--function` arg) before handing the list to the evaluator or
/// any function-name filter — see `baml_cli::optimize` for the pattern.
pub async fn discover_testset_tests(
    engine: &Arc<BexEngine>,
) -> Result<(Option<BexExternalValue>, Vec<DiscoveredTest>)> {
    let cancel = CancellationToken::new();
    let registry = engine
        .collect_tests("user", CallId::next(), cancel.clone())
        .await
        .map_err(|e| anyhow!("collect_tests failed: {e:?}"))?;

    match &registry {
        BexExternalValue::Null => return Ok((None, Vec::new())),
        BexExternalValue::Handle(_) => {}
        other => {
            return Err(anyhow!(
                "unexpected collect_tests result type: {}",
                other.type_name()
            ));
        }
    }

    // Eagerly expand every lazy (dynamic) testset. Each expansion can
    // surface more lazies nested inside, so we loop until the tree is
    // fully materialised.
    loop {
        let serialized = serialize_registry(engine, &cancel, &registry).await?;
        let lazies = collect_lazy_names(&serialized);
        if lazies.is_empty() {
            break;
        }
        for name in lazies {
            let ctx = FunctionCallContextBuilder::new(CallId::next())
                .with_cancel_token(cancel.clone())
                .build();
            engine
                .call_function(
                    "testing.TestRegistry.expand_set",
                    vec![registry.clone(), BexExternalValue::String(name.clone())],
                    ctx,
                    true,
                )
                .await
                .map_err(|e| anyhow!("expand_set({name:?}) failed: {e:?}"))?;
        }
    }

    let final_tree = serialize_registry(engine, &cancel, &registry).await?;
    let mut tests = Vec::new();
    flatten_tree(&final_tree, &mut tests);

    Ok((Some(registry), tests))
}

/// Materialise just the `TestRegistry` handle for an engine — same
/// `collect_tests` + recursive `expand_set` dance as
/// [`discover_testset_tests`] but without flattening tests back out.
///
/// Used by the evaluator each iteration to get a fresh registry tied to
/// the rebuilt engine, without re-walking the serialized tree (the test
/// list itself is stable across iterations since only prompts / schemas
/// change between candidates).
pub async fn collect_registry_handle(
    engine: &Arc<BexEngine>,
) -> Result<Option<BexExternalValue>> {
    let cancel = CancellationToken::new();
    let registry = engine
        .collect_tests("user", CallId::next(), cancel.clone())
        .await
        .map_err(|e| anyhow!("collect_tests failed: {e:?}"))?;

    match &registry {
        BexExternalValue::Null => return Ok(None),
        BexExternalValue::Handle(_) => {}
        other => {
            return Err(anyhow!(
                "unexpected collect_tests result type: {}",
                other.type_name()
            ));
        }
    }

    loop {
        let serialized = serialize_registry(engine, &cancel, &registry).await?;
        let lazies = collect_lazy_names(&serialized);
        if lazies.is_empty() {
            break;
        }
        for name in lazies {
            let ctx = FunctionCallContextBuilder::new(CallId::next())
                .with_cancel_token(cancel.clone())
                .build();
            engine
                .call_function(
                    "testing.TestRegistry.expand_set",
                    vec![registry.clone(), BexExternalValue::String(name.clone())],
                    ctx,
                    true,
                )
                .await
                .map_err(|e| anyhow!("expand_set({name:?}) failed: {e:?}"))?;
        }
    }

    Ok(Some(registry))
}

async fn serialize_registry(
    engine: &Arc<BexEngine>,
    cancel: &CancellationToken,
    registry: &BexExternalValue,
) -> Result<BexExternalValue> {
    let ctx = FunctionCallContextBuilder::new(CallId::next())
        .with_cancel_token(cancel.clone())
        .build();
    engine
        .call_function(
            "testing.TestRegistry.serialize",
            vec![registry.clone()],
            ctx,
            true,
        )
        .await
        .map_err(|e| anyhow!("TestRegistry.serialize failed: {e:?}"))
}

/// Walk a `SerializedTestDef[]` tree and push every leaf `type: "test"`
/// node into `out` as a [`DiscoveredTest`].
fn flatten_tree(value: &BexExternalValue, out: &mut Vec<DiscoveredTest>) {
    match unwrap_union(value) {
        BexExternalValue::Array { items, .. } => {
            for item in items {
                flatten_tree(item, out);
            }
        }
        BexExternalValue::Instance { class_name, fields } => {
            if class_matches(class_name, "SerializedTest") {
                let kind = fields.get("type").and_then(as_string).unwrap_or_default();
                if kind == "test" {
                    if let Some(name) = fields.get("name").and_then(as_string) {
                        let (testset, test) = split_path(name);
                        out.push(DiscoveredTest {
                            // Caller stamps the target function name; the
                            // registered path doesn't carry one.
                            function_name: String::new(),
                            test_name: test,
                            testset_name: testset,
                            file_path: PathBuf::new(),
                            kind: TestKind::Testset {
                                full_path: name.to_string(),
                            },
                            test_source: None,
                        });
                    }
                }
            } else if class_matches(class_name, "SerializedTestSet") {
                if let Some(items) = fields.get("items") {
                    flatten_tree(items, out);
                }
            }
        }
        _ => {}
    }
}

/// Collect the names of every `lazyTestSet` still in the tree. An empty
/// result means the registry is fully expanded.
fn collect_lazy_names(value: &BexExternalValue) -> Vec<String> {
    let mut out = Vec::new();
    collect_lazy_names_inner(value, &mut out);
    out
}

fn collect_lazy_names_inner(value: &BexExternalValue, out: &mut Vec<String>) {
    match unwrap_union(value) {
        BexExternalValue::Array { items, .. } => {
            for item in items {
                collect_lazy_names_inner(item, out);
            }
        }
        BexExternalValue::Instance { class_name, fields } => {
            if class_matches(class_name, "SerializedTest") {
                let kind = fields.get("type").and_then(as_string).unwrap_or_default();
                if kind == "lazyTestSet" {
                    if let Some(name) = fields.get("name").and_then(as_string) {
                        out.push(name.to_string());
                    }
                }
            } else if class_matches(class_name, "SerializedTestSet") {
                if let Some(items) = fields.get("items") {
                    collect_lazy_names_inner(items, out);
                }
            }
        }
        _ => {}
    }
}

fn unwrap_union(value: &BexExternalValue) -> &BexExternalValue {
    match value {
        BexExternalValue::Union { value, .. } => unwrap_union(value),
        other => other,
    }
}

fn as_string(value: &BexExternalValue) -> Option<&str> {
    match value {
        BexExternalValue::String(s) => Some(s.as_str()),
        BexExternalValue::Union { value, .. } => as_string(value),
        _ => None,
    }
}

/// Match on the last segment of a namespaced class name so `testing.X`
/// and `X` both match `"X"`.
fn class_matches(class_name: &str, leaf: &str) -> bool {
    class_name == leaf
        || class_name
            .rsplit_once('.')
            .is_some_and(|(_, last)| last == leaf)
}

/// Split a registered testset-test path into `(testset_name, test_name)`.
///
/// Paths come from `testing.TestRegistry.register_test`, which
/// concatenates every enclosing testset's name with `/` (see
/// `baml_std/testing/registry.baml:26-34`). So for:
///
/// ```text
/// testset "clear" {           // prefix = "clear"
///   test "single actor" { }   // registered as "clear/single actor"
/// }
/// ```
///
/// * `"clear/single actor"` → `(Some("clear"), "single actor")`.
/// * `"outer/inner/t"` → `(Some("outer/inner"), "t")` — nested testsets
///   collapse into a single testset key so per-testset scores still
///   aggregate the tree sensibly.
/// * `"bare test"` (no slash) → `(None, "bare test")` — shouldn't
///   happen for testset-style tests but we handle it defensively.
fn split_path(full_path: &str) -> (Option<String>, String) {
    match full_path.rsplit_once('/') {
        Some((prefix, test)) if !prefix.is_empty() => {
            (Some(prefix.to_string()), test.to_string())
        }
        _ => (None, full_path.to_string()),
    }
}

/// Group tests by testset name for per-testset pass rate tracking.
/// Tests without a `testset_name` are grouped under `"default"`.
pub fn group_by_testset(tests: &[DiscoveredTest]) -> HashMap<String, Vec<&DiscoveredTest>> {
    let mut groups: HashMap<String, Vec<&DiscoveredTest>> = HashMap::new();
    for test in tests {
        let key = test
            .testset_name
            .clone()
            .unwrap_or_else(|| "default".to_string());
        groups.entry(key).or_default().push(test);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_bare_name() {
        let (ts, t) = split_path("single actor");
        assert_eq!(ts, None);
        assert_eq!(t, "single actor");
    }

    #[test]
    fn split_path_one_testset() {
        let (ts, t) = split_path("clear/single actor");
        assert_eq!(ts.as_deref(), Some("clear"));
        assert_eq!(t, "single actor");
    }

    #[test]
    fn split_path_nested_testsets_collapse() {
        let (ts, t) = split_path("outer/inner/t");
        assert_eq!(ts.as_deref(), Some("outer/inner"));
        assert_eq!(t, "t");
    }
}
