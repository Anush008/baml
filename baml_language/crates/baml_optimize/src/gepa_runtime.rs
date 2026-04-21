//! GEPA runtime — wraps the embedded reflection BAML files as a live [`Bex`]
//! runtime and exposes the reflection functions (`ProposeImprovements`,
//! `MergeVariants`) to the orchestrator.
//!
//! `GEPARuntime::new()` materialises `gepa.baml` and `clients.baml` into a
//! tempdir and builds an `Arc<dyn Bex>` runtime via [`bex_project::new`].
//! The tempdir is kept alive for the runtime's lifetime.
//!
//! Reflection functions are invoked with Rust values, converted to
//! `BexExternalValue` via the [`value_bridge`](crate::value_bridge) helpers.
//! Responses are parsed back into Rust types the same way.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bex_engine::FunctionCallContextBuilder;
use bex_project::{Bex, BexArgs, FsPath, SysOps};
use sys_native::SysOpsExt;
use sys_types::CallId;
use tempfile::TempDir;

use crate::candidate::{
    CurrentMetrics, ImprovedFunction, OptimizableFunction, OptimizationObjectives,
    ReflectiveExample,
};
use crate::gepa;
use crate::value_bridge::{from_bex, to_bex};

/// Runtime for GEPA reflection functions.
///
/// Holds the compiled BAML runtime (as `Arc<dyn Bex>`) together with the
/// tempdir whose path anchors the runtime's VFS root.
pub struct GEPARuntime {
    bex: Arc<dyn Bex>,
    /// Kept alive so the tempdir isn't dropped under the runtime's feet.
    _temp_dir: TempDir,
}

impl GEPARuntime {
    /// Construct a new GEPA runtime from the embedded BAML sources.
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
    /// Inputs are serialised via `serde` → JSON → [`BexExternalValue`] and
    /// passed as named args matching the BAML function's parameter list.
    /// The return value is deserialised back to [`ImprovedFunction`].
    pub async fn propose_improvements(
        &self,
        current: &OptimizableFunction,
        failures: &[ReflectiveExample],
        successes: &[ReflectiveExample],
        objectives: &OptimizationObjectives,
        metrics: Option<&CurrentMetrics>,
    ) -> Result<ImprovedFunction> {
        let mut args: HashMap<String, bex_external_types::BexExternalValue> = HashMap::new();
        args.insert("current_function".into(), to_bex(current)?);
        args.insert("failed_examples".into(), to_bex(&failures)?);
        args.insert("successful_examples".into(), to_bex(&successes)?);
        args.insert("optimization_objectives".into(), to_bex(objectives)?);
        args.insert(
            "current_metrics".into(),
            match metrics {
                Some(m) => to_bex(m)?,
                None => bex_external_types::BexExternalValue::Null,
            },
        );

        let result = self
            .bex
            .clone()
            .call_function(
                "ProposeImprovements",
                BexArgs(args),
                FunctionCallContextBuilder::new(CallId::next()).build(),
            )
            .await
            .map_err(|e| anyhow!("ProposeImprovements call failed: {e:?}"))?;

        from_bex::<ImprovedFunction>(&result)
            .context("failed to deserialize ImprovedFunction from reflection result")
    }

    /// Call `MergeVariants` to combine two successful candidates.
    ///
    /// Currently unused by the orchestrator (merge iterations are deferred);
    /// kept here as the natural home when that lands.
    #[allow(dead_code)]
    pub async fn merge_variants(
        &self,
        variant_a: &OptimizableFunction,
        variant_b: &OptimizableFunction,
        variant_a_strengths: &[String],
        variant_b_strengths: &[String],
    ) -> Result<ImprovedFunction> {
        let mut args: HashMap<String, bex_external_types::BexExternalValue> = HashMap::new();
        args.insert("variant_a".into(), to_bex(variant_a)?);
        args.insert("variant_b".into(), to_bex(variant_b)?);
        args.insert("variant_a_strengths".into(), to_bex(&variant_a_strengths)?);
        args.insert("variant_b_strengths".into(), to_bex(&variant_b_strengths)?);

        let result = self
            .bex
            .clone()
            .call_function(
                "MergeVariants",
                BexArgs(args),
                FunctionCallContextBuilder::new(CallId::next()).build(),
            )
            .await
            .map_err(|e| anyhow!("MergeVariants call failed: {e:?}"))?;

        from_bex::<ImprovedFunction>(&result)
            .context("failed to deserialize ImprovedFunction from merge result")
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
