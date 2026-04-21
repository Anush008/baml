use minijinja::value::Value;

/// Filter: `regex_match` - Returns true if value matches regex pattern.
pub(crate) fn regex_match(value: &str, pattern: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|re| re.is_match(value))
        .unwrap_or(false)
}

/// Filter: `tojson` - Serialise any value to its JSON representation.
///
/// Matches the Jinja2/Ansible `tojson` convention. Used by the GEPA reflection
/// prompts to embed structured data.
pub(crate) fn tojson(value: Value) -> Result<String, minijinja::Error> {
    serde_json::to_string(&value).map_err(|e| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("tojson: {e}"),
        )
    })
}

/// Filter: sum - Sum numeric values in a list.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn sum(values: Vec<Value>) -> Value {
    let mut int_sum: i64 = 0;
    let mut float_sum: f64 = 0.0;
    let mut has_float = false;

    for val in values {
        if let Ok(i) = i64::try_from(val.clone()) {
            int_sum += i;
        } else if let Ok(f) = f64::try_from(val) {
            float_sum += f;
            has_float = true;
        }
    }

    if has_float {
        Value::from(int_sum as f64 + float_sum)
    } else {
        Value::from(int_sum)
    }
}
