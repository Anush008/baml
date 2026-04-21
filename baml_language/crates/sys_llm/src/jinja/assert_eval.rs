//! Jinja-based evaluation of `@@assert(...)` test expressions.
//!
//! The BAML test syntax allows `@@assert({{ this != null }})` block attributes.
//! We treat the argument as a Jinja template with `this` bound to the function's
//! return value, then interpret the rendered output as a truthy/falsy string.

use std::collections::HashMap;

use bex_external_types::BexExternalValue;
use minijinja::Environment;

use super::value_conversion::external_value_to_jinja;

/// Result of evaluating a single assertion.
#[derive(Debug, Clone)]
pub enum AssertOutcome {
    Passed,
    Failed { rendered: String },
    Error { message: String },
}

impl AssertOutcome {
    pub fn is_passed(&self) -> bool {
        matches!(self, AssertOutcome::Passed)
    }
}

/// Evaluate a single `@@assert` expression against a function result.
///
/// `template` is the raw Jinja source captured from the source file — normally
/// `{{ expression }}`, though callers may also pass a bare expression; we wrap
/// it in `{{ ... }}` in that case so the caller doesn't have to care about the
/// difference.
///
/// Returns [`AssertOutcome::Passed`] when the rendered output is a
/// recognised truthy value (`true` / `1` / non-empty non-`false` string),
/// [`AssertOutcome::Failed`] when the rendered output is falsy, and
/// [`AssertOutcome::Error`] when either the conversion or rendering fails.
pub fn evaluate_assertion(template: &str, this: &BexExternalValue) -> AssertOutcome {
    let template = if template.contains("{{") || template.contains("{%") {
        template.to_string()
    } else {
        format!("{{{{ {template} }}}}")
    };

    let mut env = Environment::new();
    env.set_debug(true);
    // Treat `null` as the none sentinel so `{{ this != null }}` works as users
    // expect. Jinja's own literal is `none`, but BAML source code uses `null`.
    env.add_global("null", minijinja::value::Value::from(()));
    // Render `none`/`undefined` as `null`, matching prompt-render semantics.
    env.set_formatter(|out, _state, value| {
        if value.is_none() || value.is_undefined() {
            write!(out, "null").map_err(|e| {
                minijinja::Error::new(minijinja::ErrorKind::WriteFailure, e.to_string())
            })
        } else {
            write!(out, "{value}").map_err(|e| {
                minijinja::Error::new(minijinja::ErrorKind::WriteFailure, e.to_string())
            })
        }
    });

    if let Err(e) = env.add_template("__assert__", &template) {
        return AssertOutcome::Error {
            message: format!("failed to compile assert template: {e}"),
        };
    }

    let mut media_handles = HashMap::new();
    let this_jinja = match external_value_to_jinja(this, &mut media_handles) {
        Ok(v) => v,
        Err(e) => {
            return AssertOutcome::Error {
                message: format!("failed to convert result for assertion: {e}"),
            };
        }
    };

    let tmpl = match env.get_template("__assert__") {
        Ok(t) => t,
        Err(e) => {
            return AssertOutcome::Error {
                message: format!("failed to load assert template: {e}"),
            };
        }
    };

    let rendered = match tmpl.render(minijinja::context! { this => this_jinja }) {
        Ok(s) => s,
        Err(e) => {
            return AssertOutcome::Error {
                message: format!("failed to render assertion: {e}"),
            };
        }
    };

    let trimmed = rendered.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "true" | "1" => AssertOutcome::Passed,
        "false" | "0" | "" | "null" | "none" => AssertOutcome::Failed { rendered },
        _ => AssertOutcome::Failed { rendered },
    }
}

#[cfg(test)]
mod tests {
    use bex_external_types::BexExternalValue;
    use indexmap::IndexMap;

    use super::*;

    fn person(name: &str, age: Option<i64>) -> BexExternalValue {
        let mut fields: IndexMap<String, BexExternalValue> = IndexMap::new();
        fields.insert("name".into(), BexExternalValue::String(name.into()));
        fields.insert(
            "age".into(),
            age.map_or(BexExternalValue::Null, BexExternalValue::Int),
        );
        BexExternalValue::Instance {
            class_name: "Person".into(),
            fields,
        }
    }

    #[test]
    fn passes_non_null_check() {
        let result = evaluate_assertion("{{ this != null }}", &person("Meg", None));
        assert!(result.is_passed(), "expected pass, got {result:?}");
    }

    #[test]
    fn fails_null_check_against_null() {
        let result = evaluate_assertion("{{ this != null }}", &BexExternalValue::Null);
        assert!(!result.is_passed(), "expected fail, got {result:?}");
    }

    #[test]
    fn field_equality_passes() {
        let result = evaluate_assertion("{{ this.name == 'Meg' }}", &person("Meg", None));
        assert!(result.is_passed(), "expected pass, got {result:?}");
    }

    #[test]
    fn field_equality_fails_on_mismatch() {
        let result = evaluate_assertion("{{ this.name == 'Pam' }}", &person("Meg", None));
        assert!(!result.is_passed(), "expected fail, got {result:?}");
    }

    #[test]
    fn bare_expression_is_wrapped_in_braces() {
        let result = evaluate_assertion("this != null", &person("Meg", None));
        assert!(result.is_passed(), "expected pass, got {result:?}");
    }

    #[test]
    fn age_comparison_with_int() {
        let result = evaluate_assertion("{{ this.age == 4 }}", &person("Ellie", Some(4)));
        assert!(result.is_passed(), "expected pass, got {result:?}");
    }
}
