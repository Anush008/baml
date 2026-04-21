//! Build a [`BexEngine`] from an in-memory source tree.
//!
//! The optimizer needs to rebuild the engine each time a candidate rewrites
//! the source. This helper mirrors the compile-to-engine sequence already
//! used by `baml-cli optimize` (discover → parse → check diagnostics →
//! compile → load) so both the CLI and the orchestrator can call one path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use baml_db::{baml_compiler2_emit, baml_compiler_diagnostics::Severity};
use baml_project::ProjectDatabase;
use bex_engine::BexEngine;
use sys_native::SysOpsExt;

/// Build a [`BexEngine`] from a set of in-memory `.baml` source files.
///
/// `root` anchors the workspace (usually the `baml_src/` directory).
/// `sources` is `path → contents` for every `.baml` file to include; the
/// paths may be absolute or relative to `root` and are passed verbatim to
/// the compiler database.
///
/// Fails with an `anyhow::Error` that lists the compilation diagnostics if
/// any error-severity diagnostic is produced.
pub fn build_engine_from_sources(
    root: &Path,
    sources: &HashMap<PathBuf, String>,
) -> Result<Arc<BexEngine>> {
    let mut db = ProjectDatabase::new();
    let project = db.set_project_root(root);

    for (path, content) in sources {
        db.add_or_update_file(path, content);
    }

    let source_files = db.get_source_files();
    let diagnostics = baml_project::collect_diagnostics(&db, project, &source_files);
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    if !errors.is_empty() {
        let formatted: Vec<String> =
            errors.iter().take(10).map(|d| d.message.clone()).collect();
        let suffix = if errors.len() > 10 {
            format!(" ... and {} more", errors.len() - 10)
        } else {
            String::new()
        };
        return Err(anyhow!(
            "compilation failed ({} error(s)): {}{suffix}",
            errors.len(),
            formatted.join("; "),
        ));
    }

    let compile_options = baml_compiler2_emit::CompileOptions {
        emit_test_cases: true,
    };
    let bytecode = baml_compiler2_emit::generate_project_bytecode(&db, &compile_options)
        .map_err(|e| anyhow!("compilation failed: {e:?}"))?;

    let engine = BexEngine::new(
        bytecode,
        Arc::new(sys_native::SysOps::native()),
        None,
        Vec::new(),
    )
    .map_err(|e| anyhow!("failed to create engine: {e:?}"))?;
    Ok(Arc::new(engine))
}

/// Read every `.baml` file under `root` into a `path → contents` map.
///
/// Hidden directories (`.*`), `node_modules`, and `target` are skipped via
/// [`baml_workspace::discover_baml_files`]. Returns canonical paths.
pub fn read_sources(root: &Path) -> Result<HashMap<PathBuf, String>> {
    let files = baml_workspace::discover_baml_files(root);
    let mut out = HashMap::with_capacity(files.len());
    for path in files {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        out.insert(path, content);
    }
    Ok(out)
}
