//! File discovery utilities.

use walkdir::{DirEntry, WalkDir};

/// Return `true` if `entry` should be traversed.
///
/// Prunes hidden directories (except the starting directory itself, which has
/// depth `0`), `node_modules`, and `target`. Applied via
/// `WalkDir::filter_entry` so the whole subtree is skipped — a bare `continue`
/// in the iterator loop would still descend into the directory's children.
fn should_traverse(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if !entry.file_type().is_dir() {
        return true;
    }
    let name = entry
        .file_name()
        .to_str()
        .unwrap_or("");
    !(name.starts_with('.') || name == "node_modules" || name == "target")
}

/// Discover all BAML files in a project directory.
///
/// Skips hidden directories (`.*`), `node_modules`, and `target`. Returns
/// paths sorted for deterministic ordering.
pub fn discover_baml_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(should_traverse)
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("baml") {
            files.push(path.to_path_buf());
        }
    }

    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use super::*;

    #[test]
    fn test_discovers_baml_files() {
        let temp_dir = std::env::temp_dir().join("baml_workspace_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let mut file1 = fs::File::create(temp_dir.join("test1.baml")).unwrap();
        file1.write_all(b"// test").unwrap();

        let mut file2 = fs::File::create(temp_dir.join("test2.baml")).unwrap();
        file2.write_all(b"// test").unwrap();

        let files = discover_baml_files(&temp_dir);

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.file_name().unwrap() == "test1.baml"));
        assert!(files.iter().any(|p| p.file_name().unwrap() == "test2.baml"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prunes_hidden_directories() {
        let temp_dir = std::env::temp_dir().join("baml_workspace_test_prune");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let visible = temp_dir.join("visible.baml");
        fs::write(&visible, "// visible").unwrap();

        let hidden_dir = temp_dir.join(".baml_optimize/candidates");
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::write(hidden_dir.join("c0.baml"), "// c0").unwrap();
        fs::write(hidden_dir.join("c1.baml"), "// c1").unwrap();

        let files = discover_baml_files(&temp_dir);

        assert_eq!(files.len(), 1, "hidden subtree must be fully pruned");
        assert_eq!(files[0], visible);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prunes_target_and_node_modules() {
        let temp_dir = std::env::temp_dir().join("baml_workspace_test_prune2");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        fs::write(temp_dir.join("keep.baml"), "// keep").unwrap();
        let nm = temp_dir.join("node_modules/pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("skip.baml"), "// skip").unwrap();
        let tgt = temp_dir.join("target/deps");
        fs::create_dir_all(&tgt).unwrap();
        fs::write(tgt.join("skip.baml"), "// skip").unwrap();

        let files = discover_baml_files(&temp_dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "keep.baml");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
