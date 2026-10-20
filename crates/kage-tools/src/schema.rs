//! JSON Schema derivation helpers for tool inputs.
//!
//! Tools typically declare a strongly-typed `Input` struct with
//! `#[derive(JsonSchema, Deserialize)]` and call [`schema_for`] from
//! [`Tool::schema`](crate::Tool::schema) to surface the schema to the model.

use schemars::JsonSchema;

/// Derive a JSON Schema for a tool input type and return it as a generic
/// `serde_json::Value` ready to embed in [`kage_core::ToolSpec::schema`].
///
/// A schema that fails to serialize (a `schemars` bug or an exotic
/// custom impl) degrades to a permissive `{"type": "object"}` so one bad
/// derivation cannot unwind a session.
#[must_use]
pub fn schema_for<T: JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(T);
    let mut value = serialize_schema(schema);
    // The draft marker is tooling metadata; Gemini's OpenAPI subset
    // rejects it and no provider can use it.
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
    }
    value
}

/// Point the doc text of property `key` at a configured default: every
/// `from` in its description becomes `to`. A tool whose limit is set in
/// `[tools.<name>]` uses this so the schema the model reads states the
/// value actually in effect. Unchanged when the property is missing.
pub(crate) fn retarget_property_doc(
    schema: &mut serde_json::Value,
    key: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    if let Some(serde_json::Value::String(text)) = schema
        .get_mut("properties")
        .and_then(|p| p.get_mut(key))
        .and_then(|p| p.get_mut("description"))
    {
        *text = text.replace(from, to);
    }
}

/// Serialize a derived schema, falling back to a permissive
/// `{"type": "object"}` when serialization fails.
fn serialize_schema(schema: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(schema).unwrap_or_else(|err| {
        eprintln!("kage: tool schema failed to serialize ({err}); using a permissive schema");
        serde_json::json!({ "type": "object" })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;

    #[derive(JsonSchema)]
    #[expect(dead_code, reason = "the fields only feed the derived schema")]
    struct ReadInput {
        path: String,
        start_line: Option<u32>,
        end_line: Option<u32>,
    }

    /// The draft key is pure tooling metadata and is stripped before
    /// any provider sees the schema.
    #[test]
    fn schema_carries_no_draft_marker() {
        #[derive(JsonSchema)]
        #[expect(dead_code, reason = "the fields only feed the derived schema")]
        struct Probe {
            path: String,
        }
        let s = schema_for::<Probe>();
        assert!(s.get("$schema").is_none(), "{s}");
        assert_eq!(s["type"], "object");
    }

    #[test]
    fn derived_schema_has_properties() {
        let s = schema_for::<ReadInput>();
        assert_eq!(s["type"], "object");
        let props = s["properties"]
            .as_object()
            .expect("properties is an object");
        assert!(props.contains_key("path"));
        assert!(props.contains_key("start_line"));
        assert!(props.contains_key("end_line"));
        assert_eq!(props["path"]["type"], "string");
    }

    #[test]
    fn required_fields_are_listed() {
        let s = schema_for::<ReadInput>();
        let required = s["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(required.contains(&"path"), "got required {required:?}");
        assert!(
            !required.contains(&"start_line"),
            "Optional fields must not be required"
        );
    }

    /// A schema whose serialization fails takes the permissive fallback
    /// instead of unwinding the session.
    #[test]
    fn a_schema_that_fails_to_serialize_falls_back_to_a_permissive_object() {
        struct Poisoned;
        impl serde::Serialize for Poisoned {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("poisoned"))
            }
        }
        assert_eq!(
            serialize_schema(Poisoned),
            serde_json::json!({ "type": "object" })
        );
    }
}
