//! Schema Extractor - extracts function prompts and referenced types from HIR.
//!
//! Walks the compiler2 HIR `item_tree` to build an [`OptimizableFunction`] for
//! a target function. For a given function we produce:
//!
//! * `prompt_text` — the raw template string from the function's
//!   `prompt #" ... "#` block (via `DeclarativeMeta::Llm`).
//! * `classes` / `enums` — the set of user-defined types transitively
//!   reachable from the function's parameters and return type, with
//!   their `@description` / `@alias` / `@@description` annotations.
//!
//! Types are looked up across every source file in the project (not just
//! the file containing the function), since a prompt often references
//! classes defined elsewhere in `baml_src/`.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use baml_compiler2_ast as ast;
use baml_db::baml_compiler2_hir;
use baml_project::ProjectDatabase;

use crate::candidate::{ClassDefinition, EnumDefinition, OptimizableFunction, SchemaFieldDefinition};

/// Locate the named function in the project HIR and return an
/// [`OptimizableFunction`] populated with its prompt text and the
/// transitively-referenced classes / enums.
pub fn extract_optimizable_function(
    db: &ProjectDatabase,
    function_name: &str,
) -> Result<OptimizableFunction> {
    // Build name → Class / Enum indexes across every source file so we can
    // resolve a `Path` type regardless of where it's declared.
    let mut class_index: HashMap<String, baml_compiler2_hir::item_tree::Class> = HashMap::new();
    let mut enum_index: HashMap<String, baml_compiler2_hir::item_tree::Enum> = HashMap::new();
    let mut target_function: Option<baml_compiler2_hir::item_tree::Function> = None;

    for source_file in db.get_source_files() {
        let item_tree = baml_compiler2_hir::file_item_tree(db, source_file);

        for (_id, class) in &item_tree.classes {
            class_index
                .entry(class.name.to_string())
                .or_insert_with(|| class.clone());
        }
        for (_id, en) in &item_tree.enums {
            enum_index
                .entry(en.name.to_string())
                .or_insert_with(|| en.clone());
        }
        for (_id, func) in &item_tree.functions {
            if func.name.as_str() == function_name {
                target_function = Some(func.clone());
            }
        }
    }

    let function = target_function
        .ok_or_else(|| anyhow!("function '{function_name}' not found"))?;

    let prompt_text = extract_prompt_text(&function);
    let (classes, enums) = collect_referenced_types(&function, &class_index, &enum_index);

    Ok(OptimizableFunction {
        function_name: function_name.to_string(),
        prompt_text,
        classes,
        enums,
        function_source: None,
    })
}

/// Pull the raw template string out of the function's `prompt #"..."#`
/// block. Returns `""` when the function isn't declared with the
/// declarative LLM syntax (i.e. not an optimize-friendly function).
fn extract_prompt_text(func: &baml_compiler2_hir::item_tree::Function) -> String {
    match &func.declarative_meta {
        Some(ast::DeclarativeMeta::Llm(llm)) => llm
            .prompt
            .as_ref()
            .map(|p| p.text.clone())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// Walk the function's param and return types and collect every user-defined
/// class / enum they touch. Output order mirrors insertion order so tests
/// can assert a predictable shape.
fn collect_referenced_types(
    func: &baml_compiler2_hir::item_tree::Function,
    class_index: &HashMap<String, baml_compiler2_hir::item_tree::Class>,
    enum_index: &HashMap<String, baml_compiler2_hir::item_tree::Enum>,
) -> (Vec<ClassDefinition>, Vec<EnumDefinition>) {
    let mut classes = Vec::new();
    let mut enums = Vec::new();
    let mut visited_classes: HashSet<String> = HashSet::new();
    let mut visited_enums: HashSet<String> = HashSet::new();

    let mut visit = |ty: &ast::TypeExpr| {
        collect_from_type(
            ty,
            class_index,
            enum_index,
            &mut classes,
            &mut enums,
            &mut visited_classes,
            &mut visited_enums,
        );
    };

    for param in &func.params {
        if let Some(spanned) = &param.type_expr {
            visit(&spanned.expr);
        }
    }
    if let Some(ret) = &func.return_type {
        visit(&ret.expr);
    }

    (classes, enums)
}

/// Recursively walk a single type expression, materialising any named
/// class / enum along the way.
fn collect_from_type(
    ty: &ast::TypeExpr,
    class_index: &HashMap<String, baml_compiler2_hir::item_tree::Class>,
    enum_index: &HashMap<String, baml_compiler2_hir::item_tree::Enum>,
    classes: &mut Vec<ClassDefinition>,
    enums: &mut Vec<EnumDefinition>,
    visited_classes: &mut HashSet<String>,
    visited_enums: &mut HashSet<String>,
) {
    match ty {
        ast::TypeExpr::Path { segments, generic_args, .. } => {
            // We handle only single-segment user types; namespaced types
            // like `baml.http.Request` belong to builtins and the reflection
            // loop doesn't need to describe them.
            if let [name] = segments.as_slice() {
                let name_str = name.to_string();

                if let Some(class) = class_index.get(&name_str) {
                    if visited_classes.insert(name_str.clone()) {
                        // Record the class first so we don't recurse forever
                        // on self-referential types; fields go in below.
                        let description = block_attr_value(&class.attributes, "description");
                        let fields: Vec<SchemaFieldDefinition> = class
                            .fields
                            .iter()
                            .map(|f| {
                                if let Some(spanned) = &f.type_expr {
                                    collect_from_type(
                                        &spanned.expr,
                                        class_index,
                                        enum_index,
                                        classes,
                                        enums,
                                        visited_classes,
                                        visited_enums,
                                    );
                                }
                                SchemaFieldDefinition {
                                    field_name: f.name.to_string(),
                                    field_type: f
                                        .type_expr
                                        .as_ref()
                                        .map(|s| format_type(&s.expr))
                                        .unwrap_or_else(|| "unknown".to_string()),
                                    description: field_attr_value(&f.attributes, "description"),
                                    alias: field_attr_value(&f.attributes, "alias"),
                                }
                            })
                            .collect();
                        classes.push(ClassDefinition {
                            class_name: name_str.clone(),
                            description,
                            fields,
                        });
                    }
                } else if let Some(en) = enum_index.get(&name_str) {
                    if visited_enums.insert(name_str.clone()) {
                        let mut values = Vec::new();
                        let mut value_descriptions = HashMap::new();
                        for v in &en.variants {
                            let vn = v.name.to_string();
                            if let Some(desc) = field_attr_value(&v.attributes, "description") {
                                value_descriptions.insert(vn.clone(), desc);
                            }
                            values.push(vn);
                        }
                        enums.push(EnumDefinition {
                            enum_name: name_str,
                            values,
                            value_descriptions,
                        });
                    }
                }
            }
            for arg in generic_args {
                collect_from_type(
                    arg,
                    class_index,
                    enum_index,
                    classes,
                    enums,
                    visited_classes,
                    visited_enums,
                );
            }
        }
        ast::TypeExpr::Optional { inner, .. } | ast::TypeExpr::List { inner, .. } => {
            collect_from_type(
                inner,
                class_index,
                enum_index,
                classes,
                enums,
                visited_classes,
                visited_enums,
            );
        }
        ast::TypeExpr::Map { key, value, .. } => {
            collect_from_type(
                key,
                class_index,
                enum_index,
                classes,
                enums,
                visited_classes,
                visited_enums,
            );
            collect_from_type(
                value,
                class_index,
                enum_index,
                classes,
                enums,
                visited_classes,
                visited_enums,
            );
        }
        ast::TypeExpr::Union { variants, .. } => {
            for v in variants {
                collect_from_type(
                    v,
                    class_index,
                    enum_index,
                    classes,
                    enums,
                    visited_classes,
                    visited_enums,
                );
            }
        }
        ast::TypeExpr::Function { params, ret, .. } => {
            for p in params {
                collect_from_type(
                    &p.ty,
                    class_index,
                    enum_index,
                    classes,
                    enums,
                    visited_classes,
                    visited_enums,
                );
            }
            collect_from_type(
                ret,
                class_index,
                enum_index,
                classes,
                enums,
                visited_classes,
                visited_enums,
            );
        }
        // Primitive / literal / opaque types have no nested named refs.
        ast::TypeExpr::Int { .. }
        | ast::TypeExpr::Float { .. }
        | ast::TypeExpr::String { .. }
        | ast::TypeExpr::Bool { .. }
        | ast::TypeExpr::Null { .. }
        | ast::TypeExpr::Never { .. }
        | ast::TypeExpr::Void { .. }
        | ast::TypeExpr::Uint8Array { .. }
        | ast::TypeExpr::Media { .. }
        | ast::TypeExpr::Literal { .. }
        | ast::TypeExpr::BuiltinUnknown { .. }
        | ast::TypeExpr::Type { .. }
        | ast::TypeExpr::Rust { .. }
        | ast::TypeExpr::Error { .. }
        | ast::TypeExpr::Unknown { .. } => {}
    }
}

/// Render a type expression as the short display string used in the
/// reflection prompt (e.g. `Person`, `string[]`, `map<string, int>`).
fn format_type(ty: &ast::TypeExpr) -> String {
    match ty {
        ast::TypeExpr::Path { segments, generic_args, .. } => {
            let base = segments
                .iter()
                .map(|s| s.as_str().to_string())
                .collect::<Vec<String>>()
                .join(".");
            if generic_args.is_empty() {
                base
            } else {
                let args = generic_args
                    .iter()
                    .map(format_type)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{base}<{args}>")
            }
        }
        ast::TypeExpr::Optional { inner, .. } => format!("{}?", format_type(inner)),
        ast::TypeExpr::List { inner, .. } => format!("{}[]", format_type(inner)),
        ast::TypeExpr::Map { key, value, .. } => {
            format!("map<{}, {}>", format_type(key), format_type(value))
        }
        ast::TypeExpr::Union { variants, .. } => variants
            .iter()
            .map(format_type)
            .collect::<Vec<_>>()
            .join(" | "),
        ast::TypeExpr::Int { .. } => "int".into(),
        ast::TypeExpr::Float { .. } => "float".into(),
        ast::TypeExpr::String { .. } => "string".into(),
        ast::TypeExpr::Bool { .. } => "bool".into(),
        ast::TypeExpr::Null { .. } => "null".into(),
        ast::TypeExpr::Never { .. } => "never".into(),
        ast::TypeExpr::Void { .. } => "void".into(),
        ast::TypeExpr::Uint8Array { .. } => "Uint8Array".into(),
        ast::TypeExpr::Media { kind, .. } => format!("{kind:?}").to_lowercase(),
        ast::TypeExpr::Literal { value, .. } => format!("{value:?}"),
        ast::TypeExpr::Function { .. } => "function".into(),
        ast::TypeExpr::BuiltinUnknown { .. } | ast::TypeExpr::Unknown { .. } => "unknown".into(),
        ast::TypeExpr::Type { .. } => "type".into(),
        ast::TypeExpr::Rust { .. } => "rust".into(),
        ast::TypeExpr::Error { .. } => "<error>".into(),
    }
}

/// Find `@<name>(...)` on a field/variant and return the first argument's
/// string value (with surrounding quotes stripped). Returns `None` if the
/// attribute is absent, has no args, or the arg isn't a quoted string.
fn field_attr_value(
    attrs: &[baml_compiler2_hir::item_tree::Attribute],
    name: &str,
) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name.as_str() == name)
        .and_then(|a| a.args.first())
        .and_then(|arg| strip_string_literal(&arg.value))
}

/// Same as [`field_attr_value`], but for `@@<name>(...)` on a class/enum.
/// The HIR stores both field-level and block-level attributes in the same
/// `Attribute` shape; the `@@` vs. `@` distinction is preserved by where the
/// attribute is attached, so the lookup is identical.
fn block_attr_value(
    attrs: &[baml_compiler2_hir::item_tree::Attribute],
    name: &str,
) -> Option<String> {
    field_attr_value(attrs, name)
}

/// Strip outer `"..."` from a literal-source string, unescaping the minimal
/// set we care about (`\"`, `\\`, `\n`). Returns `None` if the input is not
/// a quoted string — we conservatively surface nothing in that case rather
/// than feeding raw source into the reflection prompt.
fn strip_string_literal(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baml_project::ProjectDatabase;

    fn db_from(source: &str) -> ProjectDatabase {
        let mut db = ProjectDatabase::new();
        let root = std::path::PathBuf::from("/virtual");
        let _ = db.set_project_root(&root);
        let path = root.join("test.baml");
        db.add_or_update_file(&path, source);
        db
    }

    #[test]
    fn extracts_prompt_text_from_function() {
        let db = db_from(
            r##"
            class Person {
              name string
              age int?
            }

            function ExtractSubject(sentence: string) -> Person? {
              client Foo
              prompt #"
              Extract the subject from this sentence: {{ sentence }}
              {{ ctx.output_format }}
              "#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "ExtractSubject").unwrap();
        assert!(f.prompt_text.contains("Extract the subject from this sentence"));
        assert!(f.prompt_text.contains("{{ sentence }}"));
        assert!(f.prompt_text.contains("{{ ctx.output_format }}"));
    }

    #[test]
    fn collects_referenced_class_from_return_type() {
        let db = db_from(
            r##"
            class Person {
              name string
              age int?
            }

            function ExtractSubject(sentence: string) -> Person? {
              client Foo
              prompt #"p"#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "ExtractSubject").unwrap();
        assert_eq!(f.classes.len(), 1);
        let person = &f.classes[0];
        assert_eq!(person.class_name, "Person");
        assert_eq!(person.fields.len(), 2);
        assert_eq!(person.fields[0].field_name, "name");
        assert_eq!(person.fields[0].field_type, "string");
        assert_eq!(person.fields[1].field_name, "age");
        assert_eq!(person.fields[1].field_type, "int?");
    }

    #[test]
    fn extracts_field_descriptions_and_aliases() {
        let db = db_from(
            r##"
            /// A person extracted from the sentence.
            class Person {
              name string @description("The person's given name") @alias("full_name")
              age int? @description("Age in years")
            }

            function ExtractSubject(sentence: string) -> Person {
              client Foo
              prompt #"p"#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "ExtractSubject").unwrap();
        let person = &f.classes[0];
        assert_eq!(
            person.fields[0].description.as_deref(),
            Some("The person's given name")
        );
        assert_eq!(person.fields[0].alias.as_deref(), Some("full_name"));
        assert_eq!(person.fields[1].description.as_deref(), Some("Age in years"));
    }

    #[test]
    fn collects_referenced_enum_with_variant_descriptions() {
        let db = db_from(
            r##"
            enum Mood {
              Happy @description("feeling good")
              Sad
            }

            function Classify(s: string) -> Mood {
              client Foo
              prompt #"p"#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "Classify").unwrap();
        assert_eq!(f.enums.len(), 1);
        let mood = &f.enums[0];
        assert_eq!(mood.enum_name, "Mood");
        assert_eq!(mood.values, vec!["Happy".to_string(), "Sad".to_string()]);
        assert_eq!(
            mood.value_descriptions.get("Happy").map(String::as_str),
            Some("feeling good")
        );
        assert!(!mood.value_descriptions.contains_key("Sad"));
    }

    #[test]
    fn recurses_into_nested_types() {
        let db = db_from(
            r##"
            class Owner {
              pet Pet
            }
            class Pet {
              name string
            }

            function GetOwner(s: string) -> Owner {
              client Foo
              prompt #"p"#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "GetOwner").unwrap();
        let names: Vec<&str> = f.classes.iter().map(|c| c.class_name.as_str()).collect();
        assert!(names.contains(&"Owner"));
        assert!(names.contains(&"Pet"));
    }

    #[test]
    fn handles_lists_and_optionals() {
        let db = db_from(
            r##"
            class Item { name string }

            function List(s: string) -> Item[]? {
              client Foo
              prompt #"p"#
            }
            "##,
        );
        let f = extract_optimizable_function(&db, "List").unwrap();
        assert_eq!(f.classes.len(), 1);
        assert_eq!(f.classes[0].class_name, "Item");
    }

    #[test]
    fn missing_function_returns_error() {
        let db = db_from("class C { x int }");
        let err = extract_optimizable_function(&db, "Nope").unwrap_err();
        assert!(err.to_string().contains("Nope"));
    }

    #[test]
    fn strip_string_literal_handles_escapes() {
        assert_eq!(strip_string_literal(r#""hello""#).as_deref(), Some("hello"));
        assert_eq!(
            strip_string_literal(r#""a\"b""#).as_deref(),
            Some("a\"b")
        );
        assert_eq!(strip_string_literal(r#""line\nbreak""#).as_deref(), Some("line\nbreak"));
        assert_eq!(strip_string_literal("no-quotes"), None);
    }
}
