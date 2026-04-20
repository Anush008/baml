//! GEPA runtime - wraps the embedded reflection BAML files as a live [`Bex`]
//! runtime, ready to call reflection functions.
//!
//! `GEPARuntime::new()` materialises `gepa.baml` and `clients.baml` into a
//! tempdir (used as the VFS anchor) and constructs an `Arc<dyn Bex>` runtime
//! via [`bex_project::new`]. The tempdir is kept alive for the runtime's
//! lifetime.
//!
//! Reflection entry points (`propose_improvements`, `merge_variants`, etc.)
//! are stubbed for now — wiring them up requires a Rust ↔
//! `BexExternalValue` conversion layer which is not yet available. See
//! `baml_optimize/README` or the plan for the follow-on phase.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bex_project::{Bex, FsPath, SysOps};
use sys_native::SysOpsExt;
use tempfile::TempDir;

use crate::candidate::{
    CurrentMetrics, ImprovedFunction, OptimizableFunction, OptimizationObjectives,
    ReflectiveExample,
};
use crate::gepa;

/// Runtime for GEPA reflection functions.
///
/// Holds the compiled BAML runtime (as `Arc<dyn Bex>`) together with the
/// tempdir whose path anchors the runtime's VFS root.
pub struct GEPARuntime {
    #[allow(dead_code)]
    bex: Arc<dyn Bex>,
    /// Kept alive so the tempdir isn't dropped under the runtime's feet.
    _temp_dir: TempDir,
}

impl GEPARuntime {
    /// Construct a new GEPA runtime from the embedded BAML sources.
    ///
    /// A tempdir is created to serve as the VFS root. `gepa.baml` and
    /// `clients.baml` are also written to disk inside that tempdir so the
    /// VFS has a real path to resolve; the source content is additionally
    /// passed via the in-memory `files` map that `bex_project::new` expects.
    pub fn new() -> Result<Self> {
        let temp_dir = TempDir::new().context("failed to create GEPA temp dir")?;
        let root = temp_dir.path().to_path_buf();

        std::fs::write(root.join("gepa.baml"), gepa::GEPA_BAML)
            .context("failed to write gepa.baml to temp dir")?;
        std::fs::write(root.join("clients.baml"), gepa::CLIENTS_BAML)
            .context("failed to write clients.baml to temp dir")?;

        let mut files: HashMap<FsPath, String> = HashMap::new();
        files.insert(
            FsPath::from_str("gepa.baml".to_string()),
            gepa::GEPA_BAML.to_string(),
        );
        files.insert(
            FsPath::from_str("clients.baml".to_string()),
            gepa::CLIENTS_BAML.to_string(),
        );

        let vfs_root = vfs::VfsPath::new(vfs::PhysicalFS::new("/"));
        let vfs_path = vfs_root
            .join(root.to_string_lossy().trim_start_matches('/'))
            .map_err(|e| anyhow!("failed to build VFS path for GEPA root: {e}"))?;

        let bex = bex_project::new(vfs_path, SysOps::native(), files, None)
            .map_err(|e| anyhow!("failed to construct GEPA runtime: {e:?}"))?;
        let bex: Arc<dyn Bex> = bex;

        Ok(Self {
            bex,
            _temp_dir: temp_dir,
        })
    }

    /// Call `ProposeImprovements` to generate an improved function.
    ///
    /// Not yet wired — requires a Rust-struct ↔ `BexExternalValue` bridge
    /// that doesn't exist in the codebase yet. Tracked as a follow-on.
    pub async fn propose_improvements(
        &self,
        _current: &OptimizableFunction,
        _failures: &[ReflectiveExample],
        _successes: &[ReflectiveExample],
        _objectives: &OptimizationObjectives,
        _metrics: Option<&CurrentMetrics>,
    ) -> Result<ImprovedFunction> {
        Err(anyhow!(
            "GEPARuntime::propose_improvements not yet implemented: \
             needs Rust ↔ BexExternalValue conversion layer"
        ))
    }

    /// Call `MergeVariants` to combine two successful candidates.
    ///
    /// Not yet wired — same blocker as [`GEPARuntime::propose_improvements`].
    pub async fn merge_variants(
        &self,
        _variant_a: &OptimizableFunction,
        _variant_b: &OptimizableFunction,
        _variant_a_strengths: &[String],
        _variant_b_strengths: &[String],
    ) -> Result<ImprovedFunction> {
        Err(anyhow!(
            "GEPARuntime::merge_variants not yet implemented: \
             needs Rust ↔ BexExternalValue conversion layer"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_baml_files_are_non_empty() {
        assert!(gepa::GEPA_BAML.contains("function ProposeImprovements"));
        assert!(gepa::CLIENTS_BAML.contains("ReflectionModel"));
    }

    #[test]
    fn runtime_constructs_from_embedded_sources() {
        // This proves the embedded BAML compiles in the baml_language stack.
        let runtime = GEPARuntime::new();
        match runtime {
            Ok(_) => (),
            Err(e) => panic!("GEPARuntime::new failed: {e:?}"),
        }
    }
}
