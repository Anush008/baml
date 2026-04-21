//! Filter testset tests to the ones that actually call a target function.
//!
//! The optimizer runs with `--function <Name>` to focus on improving one
//! function's prompt / schema. A BAML project may define testsets that
//! exercise other functions; running those during this optimization run
//! is wasted work and drags the accuracy score with irrelevant tests.
//!
//! To decide whether a testset test targets `Name`, we use the LSP
//! find-references machinery:
//!
//! 1. Locate the function's definition (`search_symbols` → name span).
//! 2. `usages_at(db, def_file, def_offset)` → every call site as
//!    `Location { file, range }`.
//! 3. Walk every source file's CST to build a static map
//!    `full_path → (file, text_range)` for each
//!    `testset "g" { test "t" { ... } }` whose name is a string literal.
//! 4. A test is "relevant" iff any usage lies inside its text range.
//!
//! Tests whose path can't be reconstructed statically (e.g. `testset s`
//! where `s` is a loop variable) are kept by default — we can't prove
//! they don't call the target, and silently dropping them would be more
//! surprising than including an occasional unrelated test.

use std::collections::{HashMap, HashSet};

use baml_db::SourceFile;
use baml_compiler_parser::syntax_tree;
use baml_compiler_syntax::{SyntaxKind, SyntaxNode};
use baml_lsp2_actions::{
    DefinitionKind, definition::Location, search::search_symbols, usages::usages_at,
};
use baml_project::ProjectDatabase;
use text_size::TextRange;

use crate::discovery::{DiscoveredTest, TestKind};

/// Drop testset tests whose body does not call `target_function`. Legacy
/// (`test Foo { functions [Bar] ... }`) tests are passed through
/// unchanged — they already declare their target via `functions [...]`.
///
/// Returns the filtered list plus the number of testset tests that were
/// dropped, so the caller can surface a summary line.
pub fn retain_tests_calling_function(
    db: &ProjectDatabase,
    tests: Vec<DiscoveredTest>,
    target_function: &str,
) -> (Vec<DiscoveredTest>, usize) {
    let source_files: Vec<SourceFile> = db.get_source_files();

    // Locate the function's name span. If the project doesn't contain a
    // top-level function with this exact name, we can't do anything smart
    // — return the input untouched.
    let candidates = search_symbols(db, &source_files, target_function);
    let def = match candidates.into_iter().find(|s| {
        s.name == target_function
            && s.container_name.is_none()
            && matches!(s.kind, DefinitionKind::Function)
    }) {
        Some(d) => d,
        None => return (tests, 0),
    };

    // Every reference to the function across the project.
    let usages: Vec<Location> = usages_at(db, def.file, def.name_span.start());

    // Build (full_path → (file, range)) for every statically-named
    // testset test in the project.
    let mut static_paths: HashMap<String, (SourceFile, TextRange)> = HashMap::new();
    for &file in &source_files {
        let tree = syntax_tree(db, file);
        collect_static_test_paths(&tree, file, String::new(), &mut static_paths);
    }

    // Test is relevant iff at least one usage falls inside its range.
    let relevant: HashSet<String> = static_paths
        .iter()
        .filter_map(|(path, (file, range))| {
            let hit = usages
                .iter()
                .any(|u| u.file == *file && range_contains_range(*range, u.range));
            if hit { Some(path.clone()) } else { None }
        })
        .collect();

    let mut dropped = 0;
    let kept: Vec<DiscoveredTest> = tests
        .into_iter()
        .filter_map(|mut t| {
            let keep = match &t.kind {
                TestKind::Legacy => true,
                TestKind::Testset { full_path } => {
                    // If the path was resolved statically, trust the usage
                    // check. Otherwise (dynamic name) keep the test — we
                    // can't prove it doesn't target the function.
                    let resolved = static_paths.contains_key(full_path);
                    !resolved || relevant.contains(full_path)
                }
            };
            if !keep {
                dropped += 1;
                return None;
            }
            // Stamp the raw source text of the `test "..." { ... }`
            // block onto the kept test so reflection's `test_source`
            // field renders the body. Only available for statically-
            // resolved testset tests.
            if let TestKind::Testset { full_path } = &t.kind {
                if let Some((file, range)) = static_paths.get(full_path) {
                    let text = file.text(db);
                    let start: usize = range.start().into();
                    let end: usize = range.end().into();
                    if end <= text.len() {
                        t.test_source = Some(text[start..end].to_string());
                    }
                }
            }
            Some(t)
        })
        .collect();

    (kept, dropped)
}

/// Walk the CST recursively and populate `out` with every statically-
/// named `TEST_EXPR_DEF` keyed by its full slash-joined path.
///
/// `prefix` is the path assembled so far from enclosing `TESTSET_DEF`
/// nodes. Dynamic-named testsets (non-literal expression as name) are
/// skipped — tests under them end up with no entry in `out` and fall
/// into the "include by default" bucket at filter time.
fn collect_static_test_paths(
    node: &SyntaxNode,
    file: SourceFile,
    prefix: String,
    out: &mut HashMap<String, (SourceFile, TextRange)>,
) {
    for child in node.children() {
        match child.kind() {
            SyntaxKind::TESTSET_DEF => {
                if let Some(name) = extract_name_literal(&child) {
                    let new_prefix = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    collect_static_test_paths(&child, file, new_prefix, out);
                }
                // Dynamic-named testset: don't descend with a path.
                // Tests inside still exist at runtime; they'll hit the
                // "not resolved → include by default" branch in the
                // filter.
            }
            SyntaxKind::TEST_EXPR_DEF => {
                if let Some(name) = extract_name_literal(&child) {
                    let full_path = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    out.insert(full_path, (file, child.text_range()));
                }
            }
            _ => {
                collect_static_test_paths(&child, file, prefix.clone(), out);
            }
        }
    }
}

/// Pull a string literal's contents out of the first child of a
/// `TEST_EXPR_DEF` / `TESTSET_DEF` node. Returns `None` if the name is
/// anything other than a single bare string literal (e.g. `"a" + b`,
/// `topic`, `count.to_string()`).
fn extract_name_literal(node: &SyntaxNode) -> Option<String> {
    // First expression child holds the name.
    for child in node.children() {
        if let Some(s) = first_string_literal(&child) {
            return Some(s);
        }
    }
    None
}

/// Find the nearest `STRING_LITERAL` node that is the *sole* expression
/// content of `node` (ignoring trivia). Returns the unquoted text.
fn first_string_literal(node: &SyntaxNode) -> Option<String> {
    // Shallow walk — if the first meaningful descendant is a
    // STRING_LITERAL, return it. If there's anything more complex
    // before it (operator, identifier, call), bail.
    let mut stack = vec![node.clone()];
    while let Some(n) = stack.pop() {
        if n.kind() == SyntaxKind::STRING_LITERAL {
            let text = n.text().to_string();
            return Some(strip_quotes(&text));
        }
        // Only drill into wrappers that typically contain a single
        // expression (EXPR, GROUP_EXPR, etc.). Anything else means the
        // name is a computed expression and we should give up.
        let children: Vec<_> = n.children().collect();
        if children.len() == 1 {
            stack.push(children.into_iter().next().unwrap());
        }
    }
    None
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn range_contains_range(outer: TextRange, inner: TextRange) -> bool {
    outer.start() <= inner.start() && inner.end() <= outer.end()
}
