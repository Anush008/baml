//! Test discovery for optimization
//!
//! Discovers tests from old-style `test { }` syntax via the compiler2 HIR
//! item tree. New-style `testset { }` support is a future TODO.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use baml_db::baml_compiler2_hir;
use baml_project::ProjectDatabase;

/// A discovered test with its metadata.
#[derive(Clone, Debug)]
pub struct DiscoveredTest {
    pub function_name: String,
    pub test_name: String,
    pub testset_name: Option<String>,
    pub file_path: PathBuf,
}

/// Discover all tests for functions matching the filter.
///
/// Walks all source files in the project and collects tests from the compiler2
/// HIR item tree (old-style `test { }` syntax).
///
/// If `function_filter` is non-empty, only tests whose `function_refs` contain
/// at least one entry that includes one of the filter strings are returned.
///
/// Deduplicates by `(function_name, test_name)` pair across files.
///
/// New-style `testset { }` collection is not yet implemented (TODO).
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
                });
            }
        }
    }

    tests
}

/// Group tests by testset name for per-testset pass rate tracking.
///
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
