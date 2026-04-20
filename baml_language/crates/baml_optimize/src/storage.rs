//! Checkpoint and artifact persistence for optimization runs.
//!
//! Layout of `<root>/.baml_optimize/run_<epoch_secs>/`:
//! ```text
//! config.json              // RunConfig (objectives, parallelism, etc.)
//! state.json               // RunState (iteration, frontier, candidate ids)
//! base/                    // immutable snapshot of baml_src at run start
//!   <mirrors the baml_src tree>
//! candidates/
//!   candidate_<id>.json    // structured Candidate metadata
//!   candidate_<id>.patch   // unified diff of modified sources vs base
//! evaluations/             // reserved for per-test results (future)
//! reflections/             // reserved for LLM reflection logs (future)
//! final_results.json       // written once the run completes
//! ```
//!
//! Candidate source changes are stored as unified-diff patches against the
//! base snapshot. This keeps per-candidate artifacts small and lets any UI
//! render the diff with standard tooling. The `base/` directory makes each
//! patch self-contained — `git apply base/` + `<patch>` reproduces the
//! candidate's sources.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::candidate::{Candidate, CandidateScores};

/// Serialisable snapshot of where a run is in its optimisation loop.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub function_name: String,
    pub current_iteration: usize,
    pub total_evals: usize,
    pub candidate_ids: Vec<usize>,
    pub pareto_frontier: Vec<usize>,
}

/// Configuration captured at run start so subsequent invocations can resume
/// with matching parameters.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunConfig {
    pub function_name: String,
    pub max_iterations: u32,
    pub parallel: usize,
    pub objectives: Vec<ObjectiveConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectiveConfig {
    pub name: String,
    pub direction: String,
    pub weight: f64,
}

/// Manages the on-disk artifacts for a single optimization run.
pub struct Storage {
    run_dir: PathBuf,
}

impl Storage {
    /// Create a new run directory under `<root>/.baml_optimize/run_<epoch>/`.
    pub fn new(root: &Path) -> Result<Self> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_dir = root.join(".baml_optimize").join(format!("run_{ts}"));
        Self::init_directories(&run_dir)?;
        Ok(Self { run_dir })
    }

    /// Open an existing run directory for read/append.
    pub fn load(run_dir: PathBuf) -> Result<Self> {
        if !run_dir.exists() {
            anyhow::bail!("run directory does not exist: {}", run_dir.display());
        }
        Ok(Self { run_dir })
    }

    fn init_directories(run_dir: &Path) -> Result<()> {
        fs::create_dir_all(run_dir)
            .with_context(|| format!("failed to create run dir: {}", run_dir.display()))?;
        for sub in ["candidates", "evaluations", "reflections", "base"] {
            fs::create_dir_all(run_dir.join(sub))
                .with_context(|| format!("failed to create {sub}/"))?;
        }
        Ok(())
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Write the run configuration. Idempotent per call.
    pub fn save_config(&self, config: &RunConfig) -> Result<()> {
        let path = self.run_dir.join("config.json");
        fs::write(&path, serde_json::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Persist an in-memory candidate as JSON.
    pub fn save_candidate(&self, candidate: &Candidate) -> Result<PathBuf> {
        let path = self
            .run_dir
            .join("candidates")
            .join(format!("candidate_{}.json", candidate.id));
        fs::write(&path, serde_json::to_string_pretty(candidate)?)?;
        Ok(path)
    }

    /// Load a candidate by id.
    pub fn load_candidate(&self, id: usize) -> Result<Option<Candidate>> {
        let path = self
            .run_dir
            .join("candidates")
            .join(format!("candidate_{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn save_checkpoint(&self, state: &RunState) -> Result<()> {
        let path = self.run_dir.join("state.json");
        fs::write(&path, serde_json::to_string_pretty(state)?)?;
        Ok(())
    }

    pub fn load_checkpoint(&self) -> Result<Option<RunState>> {
        let path = self.run_dir.join("state.json");
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// Mirror the baml_src tree into `base/` so each patch is self-contained.
    pub fn save_base_snapshot(&self, sources: &HashMap<PathBuf, String>) -> Result<()> {
        let base_dir = self.run_dir.join("base");
        for (rel, content) in sources {
            let dest = base_dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, content)?;
        }
        Ok(())
    }

    /// Write a unified-diff patch for candidate `id` describing `modified`'s
    /// deviation from `base`. Only files that differ produce a hunk; new
    /// files (in modified but not in base) and deleted files (in base but not
    /// in modified) are represented with an empty old/new side respectively.
    pub fn save_candidate_patch(
        &self,
        id: usize,
        base: &HashMap<PathBuf, String>,
        modified: &HashMap<PathBuf, String>,
    ) -> Result<PathBuf> {
        let patch = generate_unified_diff(base, modified);
        let path = self
            .run_dir
            .join("candidates")
            .join(format!("candidate_{id}.patch"));
        fs::write(&path, patch)?;
        Ok(path)
    }

    pub fn save_final_results(&self, best_id: usize, scores: &CandidateScores) -> Result<()> {
        let blob = serde_json::json!({
            "best_candidate_id": best_id,
            "scores": scores,
        });
        fs::write(
            self.run_dir.join("final_results.json"),
            serde_json::to_string_pretty(&blob)?,
        )?;
        Ok(())
    }
}

/// Produce a multi-file unified diff comparing `base` to `modified`.
///
/// Files are processed in sorted-path order for deterministic output. Paths
/// are rendered relative (no `a/`, `b/` prefixes) and use forward slashes.
/// Empty output means the two source maps are identical.
pub fn generate_unified_diff(
    base: &HashMap<PathBuf, String>,
    modified: &HashMap<PathBuf, String>,
) -> String {
    let empty = String::new();
    let mut all_paths: Vec<&PathBuf> = base.keys().chain(modified.keys()).collect();
    all_paths.sort();
    all_paths.dedup();

    let mut out = String::new();
    for path in all_paths {
        let old = base.get(path).unwrap_or(&empty);
        let new = modified.get(path).unwrap_or(&empty);
        if old == new {
            continue;
        }
        let label = path.to_string_lossy().replace('\\', "/");
        let diff = TextDiff::from_lines(old, new);
        let hunk = diff
            .unified_diff()
            .header(&label, &label)
            .to_string();
        out.push_str(&hunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{CandidateMethod, OptimizableFunction};
    use tempfile::TempDir;

    fn sample_candidate(id: usize) -> Candidate {
        Candidate {
            id,
            iteration: 1,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "F".into(),
                prompt_text: "prompt".into(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: None,
            rationale: None,
        }
    }

    #[test]
    fn checkpoint_round_trips() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::new(tmp.path()).unwrap();
        let state = RunState {
            function_name: "Foo".into(),
            current_iteration: 7,
            total_evals: 42,
            candidate_ids: vec![1, 2, 3],
            pareto_frontier: vec![2, 3],
        };
        storage.save_checkpoint(&state).unwrap();

        let loaded = storage.load_checkpoint().unwrap().expect("state present");
        assert_eq!(loaded.current_iteration, 7);
        assert_eq!(loaded.candidate_ids, vec![1, 2, 3]);
        assert_eq!(loaded.pareto_frontier, vec![2, 3]);
    }

    #[test]
    fn candidate_round_trips() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::new(tmp.path()).unwrap();
        let original = sample_candidate(5);
        storage.save_candidate(&original).unwrap();

        let loaded = storage.load_candidate(5).unwrap().expect("candidate present");
        assert_eq!(loaded.id, 5);
        assert_eq!(loaded.function.function_name, "F");

        assert!(storage.load_candidate(999).unwrap().is_none());
    }

    #[test]
    fn base_snapshot_mirrors_source_tree() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::new(tmp.path()).unwrap();
        let mut sources = HashMap::new();
        sources.insert(PathBuf::from("a.baml"), "hello".to_string());
        sources.insert(PathBuf::from("sub/b.baml"), "world".to_string());

        storage.save_base_snapshot(&sources).unwrap();

        let base_dir = storage.run_dir().join("base");
        assert_eq!(fs::read_to_string(base_dir.join("a.baml")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(base_dir.join("sub/b.baml")).unwrap(),
            "world"
        );
    }

    #[test]
    fn unified_diff_is_empty_for_identical_sources() {
        let mut base = HashMap::new();
        base.insert(PathBuf::from("a.baml"), "one\ntwo\n".to_string());
        let patch = generate_unified_diff(&base, &base);
        assert!(patch.is_empty());
    }

    #[test]
    fn unified_diff_emits_hunk_for_changed_file() {
        let mut base = HashMap::new();
        base.insert(PathBuf::from("a.baml"), "one\ntwo\n".to_string());
        let mut modified = HashMap::new();
        modified.insert(PathBuf::from("a.baml"), "one\nTWO\n".to_string());

        let patch = generate_unified_diff(&base, &modified);
        assert!(patch.contains("--- a.baml"));
        assert!(patch.contains("+++ a.baml"));
        assert!(patch.contains("-two"));
        assert!(patch.contains("+TWO"));
    }

    #[test]
    fn unified_diff_handles_added_and_removed_files() {
        let mut base = HashMap::new();
        base.insert(PathBuf::from("gone.baml"), "old\n".to_string());
        let mut modified = HashMap::new();
        modified.insert(PathBuf::from("new.baml"), "new\n".to_string());

        let patch = generate_unified_diff(&base, &modified);
        assert!(patch.contains("gone.baml"));
        assert!(patch.contains("new.baml"));
        assert!(patch.contains("-old"));
        assert!(patch.contains("+new"));
    }

    #[test]
    fn save_candidate_patch_writes_file() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::new(tmp.path()).unwrap();
        let mut base = HashMap::new();
        base.insert(PathBuf::from("a.baml"), "x\n".to_string());
        let mut modified = HashMap::new();
        modified.insert(PathBuf::from("a.baml"), "y\n".to_string());

        let path = storage.save_candidate_patch(3, &base, &modified).unwrap();
        assert!(path.ends_with("candidate_3.patch"));
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("-x"));
        assert!(contents.contains("+y"));
    }
}
