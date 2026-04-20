//! Schema Extractor - extracts function prompts and referenced types from HIR.
//!
//! Walks the compiler2 HIR `item_tree` to build an [`OptimizableFunction`] for
//! a target function. Prompt-text extraction and referenced-type traversal are
//! stubbed for now — this phase just wires up the lookup path.

use anyhow::{Result, anyhow};
use baml_db::baml_compiler2_hir;
use baml_project::ProjectDatabase;

use crate::candidate::{ClassDefinition, EnumDefinition, OptimizableFunction};

/// Locate the named function in the project HIR and return a skeletal
/// [`OptimizableFunction`]. Prompt text and referenced types are not yet
/// extracted (returns empty placeholders).
pub fn extract_optimizable_function(
    db: &ProjectDatabase,
    function_name: &str,
) -> Result<OptimizableFunction> {
    for source_file in db.get_source_files() {
        let item_tree = baml_compiler2_hir::file_item_tree(db, source_file);

        for (_id, func) in &item_tree.functions {
            if func.name.to_string() == function_name {
                let prompt_text = extract_prompt_text(func);
                let (classes, enums) = extract_referenced_types(func);
                return Ok(OptimizableFunction {
                    function_name: function_name.to_string(),
                    prompt_text,
                    classes,
                    enums,
                    function_source: None,
                });
            }
        }
    }

    Err(anyhow!("function '{function_name}' not found"))
}

/// Placeholder: real implementation will pull the prompt literal out of the
/// function body via HIR spans.
#[allow(clippy::needless_pass_by_value)]
fn extract_prompt_text(_func: &baml_compiler2_hir::item_tree::Function) -> String {
    String::new()
}

/// Placeholder: real implementation will walk param/return types and collect
/// referenced class / enum definitions.
fn extract_referenced_types(
    _func: &baml_compiler2_hir::item_tree::Function,
) -> (Vec<ClassDefinition>, Vec<EnumDefinition>) {
    (Vec::new(), Vec::new())
}
