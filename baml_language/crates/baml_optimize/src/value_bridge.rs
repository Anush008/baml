//! `serde_json::Value` ↔ [`BexExternalValue`] conversion for crossing the
//! Rust/BAML boundary.
//!
//! Callers (notably [`GEPARuntime::propose_improvements`]) already have their
//! inputs as Rust structs deriving `Serialize`; they can call
//! [`json_to_bex_value`] on `serde_json::to_value(&rust_struct)?` to produce
//! the `BexExternalValue` the runtime expects as an argument. The inverse,
//! [`bex_to_json_value`], turns the runtime's return value back into JSON so
//! a matching `Deserialize` struct (`ImprovedFunction`, etc.) can be parsed.
//!
//! Two simplifications vs. the engine's equivalent (`json_to_baml_value` in
//! `engine/baml-runtime/src/optimize/gepa_runtime.rs`):
//!
//! - JSON objects lower to `BexExternalValue::Map { value_type: Ty::unknown() }`.
//!   BAML coerces the map to a class instance at call-time based on the target
//!   parameter's declared type. The engine uses the same strategy.
//! - We don't track union provenance. Optional fields come back as `Null` and
//!   are simply `serde_json::Value::Null` in the output.

use anyhow::{Result, anyhow};
use baml_type::Ty;
use bex_external_types::BexExternalValue;
use indexmap::IndexMap;
use serde_json::Value as Json;

/// Convert a `serde_json::Value` to a [`BexExternalValue`] suitable for use
/// as an argument to [`bex_project::Bex::call_function`].
pub fn json_to_bex_value(json: &Json) -> BexExternalValue {
    match json {
        Json::Null => BexExternalValue::Null,
        Json::Bool(b) => BexExternalValue::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                BexExternalValue::Int(i)
            } else {
                // as_f64 is infallible for serde_json::Number.
                BexExternalValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Json::String(s) => BexExternalValue::String(s.clone()),
        Json::Array(items) => BexExternalValue::Array {
            element_type: Ty::unknown(),
            items: items.iter().map(json_to_bex_value).collect(),
        },
        Json::Object(map) => BexExternalValue::Map {
            key_type: Ty::string(),
            value_type: Ty::unknown(),
            entries: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_bex_value(v)))
                .collect::<IndexMap<_, _>>(),
        },
    }
}

/// Convert a [`BexExternalValue`] returned by the runtime into
/// `serde_json::Value`. The inverse of [`json_to_bex_value`] for the shapes
/// the reflection functions return.
///
/// Handles the variants a BAML function result can produce: primitives,
/// arrays, maps, class instances (projected to JSON objects), enum variants
/// (projected to their variant name), and unions (unwrapped to the inner
/// value).
pub fn bex_to_json_value(value: &BexExternalValue) -> Result<Json> {
    match value {
        BexExternalValue::Null => Ok(Json::Null),
        BexExternalValue::Int(i) => Ok(Json::from(*i)),
        BexExternalValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or_else(|| anyhow!("non-finite float cannot be encoded as JSON: {f}")),
        BexExternalValue::Bool(b) => Ok(Json::from(*b)),
        BexExternalValue::String(s) => Ok(Json::from(s.clone())),
        BexExternalValue::Array { items, .. } => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(bex_to_json_value(item)?);
            }
            Ok(Json::Array(out))
        }
        BexExternalValue::Map { entries, .. } => {
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                out.insert(k.clone(), bex_to_json_value(v)?);
            }
            Ok(Json::Object(out))
        }
        BexExternalValue::Instance { fields, .. } => {
            let mut out = serde_json::Map::with_capacity(fields.len());
            for (k, v) in fields {
                out.insert(k.clone(), bex_to_json_value(v)?);
            }
            Ok(Json::Object(out))
        }
        BexExternalValue::Variant { variant_name, .. } => Ok(Json::from(variant_name.clone())),
        BexExternalValue::Union { value, .. } => bex_to_json_value(value),
        BexExternalValue::Handle(_)
        | BexExternalValue::Uint8Array(_)
        | BexExternalValue::RustData(_)
        | BexExternalValue::Adt(_)
        | BexExternalValue::FunctionRef { .. } => Err(anyhow!(
            "cannot convert {} to JSON",
            value.type_name()
        )),
    }
}

/// Convenience: take anything `Serialize` and produce a [`BexExternalValue`].
pub fn to_bex<T: serde::Serialize>(value: &T) -> Result<BexExternalValue> {
    let json = serde_json::to_value(value)?;
    Ok(json_to_bex_value(&json))
}

/// Convenience: take a [`BexExternalValue`] and deserialize it into a
/// `DeserializeOwned` Rust type.
pub fn from_bex<T: serde::de::DeserializeOwned>(value: &BexExternalValue) -> Result<T> {
    let json = bex_to_json_value(value)?;
    Ok(serde_json::from_value(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[test]
    fn primitives_round_trip() {
        let samples: Vec<Json> = vec![
            Json::Null,
            Json::from(true),
            Json::from(42_i64),
            Json::from(3.14_f64),
            Json::from("hello".to_string()),
        ];
        for s in samples {
            let bex = json_to_bex_value(&s);
            let back = bex_to_json_value(&bex).expect("round-trip");
            assert_eq!(back, s, "round-trip should preserve {s:?}");
        }
    }

    #[test]
    fn nested_object_round_trips() {
        let src = serde_json::json!({
            "name": "Alice",
            "age": 30,
            "tags": ["admin", "qa"],
            "address": { "city": "SF", "zip": 94107 },
            "active": true,
            "notes": null,
        });
        let bex = json_to_bex_value(&src);
        let back = bex_to_json_value(&bex).expect("round-trip");
        assert_eq!(back, src);
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Person {
        name: String,
        age: i64,
        tags: Vec<String>,
    }

    #[test]
    fn to_and_from_bex_for_struct() {
        let p = Person {
            name: "Meg".into(),
            age: 21,
            tags: vec!["engineer".into(), "writer".into()],
        };
        let bex = to_bex(&p).expect("to_bex");
        let back: Person = from_bex(&bex).expect("from_bex");
        assert_eq!(back, p);
    }

    #[test]
    fn instance_value_projects_to_object() {
        // Simulate what BAML returns for a `class` type result.
        let mut fields = IndexMap::new();
        fields.insert("name".to_string(), BexExternalValue::String("Meg".into()));
        fields.insert("age".to_string(), BexExternalValue::Int(21));
        let inst = BexExternalValue::Instance {
            class_name: "Person".into(),
            fields,
        };
        let json = bex_to_json_value(&inst).expect("instance to json");
        assert_eq!(json, serde_json::json!({ "name": "Meg", "age": 21 }));
    }

    #[test]
    fn union_unwraps_to_inner_value() {
        use bex_external_types::UnionMetadata;

        // Optional<int> → union with null and int as members.
        let union_type = Ty::union([Ty::null(), Ty::int()]);
        let metadata = UnionMetadata::new(union_type, Ty::int());
        let union_value = BexExternalValue::Union {
            value: Box::new(BexExternalValue::Int(5)),
            metadata,
        };
        let json = bex_to_json_value(&union_value).expect("union to json");
        assert_eq!(json, Json::from(5_i64));
    }
}
