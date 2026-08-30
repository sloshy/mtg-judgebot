//! schemars → Anthropic structured-output subset.
//!
//! Anthropic's JSON Schema subset (same for `output_config.format` and
//! strict tools) supports `anyOf` but not `oneOf`, requires
//! `additionalProperties: false` on every object, and rejects numeric/string
//! constraints (`minimum`, `maximum`, `multipleOf`, `minLength`, `maxLength`,
//! `pattern`), array constraints and any string `format` outside a short
//! list. One `RecursiveTransform` applies all of that to every subschema,
//! including `$defs`. Removed constraints are still enforced client-side by
//! serde/nutype when the response is deserialized.

use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings, transform::{Transform, transform_subschemas}};
use serde_json::Value;

/// Keys Anthropic rejects; stripped from every subschema.
pub const UNSUPPORTED_KEYS: &[&str] = &[
    "pattern",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

/// String formats Anthropic accepts; any other `format` is stripped.
pub const SUPPORTED_FORMATS: &[&str] =
    &["date-time", "time", "date", "duration", "email", "hostname", "uri", "ipv4", "ipv6", "uuid"];

/// Rewrites a schema in place so it fits Anthropic's supported subset.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicSubset;

fn is_object(obj: &serde_json::Map<String, Value>) -> bool {
    obj.contains_key("properties")
        || obj.get("type").is_some_and(|t| {
            t == "object" || t.as_array().is_some_and(|a| a.iter().any(|x| x == "object"))
        })
}

impl Transform for AnthropicSubset {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(obj) = schema.as_object_mut() {
            if let Some(one_of) = obj.remove("oneOf") {
                obj.insert("anyOf".to_owned(), one_of);
            }
            if is_object(obj) {
                obj.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            for k in UNSUPPORTED_KEYS {
                obj.remove(*k);
            }
            let bad_format = obj
                .get("format")
                .is_some_and(|f| !f.as_str().is_some_and(|s| SUPPORTED_FORMATS.contains(&s)));
            if bad_format {
                obj.remove("format");
            }
        }
        transform_subschemas(self, schema);
    }
}

/// Generate the schema for `T`, ready for `output_config.format` or a tool `input_schema`.
#[must_use]
pub fn anthropic_schema<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::draft2020_12().with_transform(AnthropicSubset);
    SchemaGenerator::new(settings).into_root_schema_for::<T>().to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::Verdict;

    fn walk(v: &Value, f: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
        match v {
            Value::Object(m) => {
                f(m);
                m.values().for_each(|c| walk(c, f));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
            _ => {}
        }
    }

    /// Every object closed, no `oneOf`, no unsupported keys, no unsupported formats.
    fn assert_subset(schema: &Value) {
        let mut objects = 0;
        walk(schema, &mut |m| {
            assert!(!m.contains_key("oneOf"), "oneOf present: {schema:#}");
            for k in UNSUPPORTED_KEYS {
                assert!(!m.contains_key(*k), "{k} present: {schema:#}");
            }
            if let Some(f) = m.get("format") {
                assert!(f.as_str().is_some_and(|s| SUPPORTED_FORMATS.contains(&s)), "bad format {f}: {schema:#}");
            }
            if is_object(m) {
                objects += 1;
                assert_eq!(m.get("additionalProperties"), Some(&Value::Bool(false)), "open object: {schema:#}");
            }
        });
        assert!(objects >= 1, "{schema:#}");
    }

    #[test]
    fn verdict_schema_fits_anthropic_subset() {
        let schema = anthropic_schema::<Verdict>();
        assert_subset(&schema);
        let s = schema.to_string();
        // The tagged Citation enum must have become anyOf, and Citation::ScryfallRuling's u32 idx
        // must have lost its "uint32" format and "minimum": 0.
        assert!(s.contains("anyOf"), "{schema:#}");
        assert!(s.contains("\"kind\""), "{schema:#}");
        assert!(!s.contains("uint32"), "{schema:#}");
        // uuid is allowed and must survive.
        assert!(s.contains("\"uuid\""), "{schema:#}");
        // cr_version is not model-provided.
        assert!(!s.contains("cr_version"), "{schema:#}");
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Inner {
        n: u8,
        #[schemars(regex(pattern = r"^x+$"), length(min = 1, max = 5))]
        s: String,
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Outer {
        maybe: Option<Inner>,
        #[schemars(length(min = 1))]
        list: Vec<Inner>,
        #[schemars(range(min = 1, max = 10))]
        count: u32,
    }

    #[test]
    fn optional_nested_objects_are_closed_and_constraints_stripped() {
        let schema = anthropic_schema::<Outer>();
        assert_subset(&schema);
        let s = schema.to_string();
        assert!(!s.contains("uint8"), "{schema:#}");
        // The Option<Inner> field is typed ["object","null"] (or a $ref); either way every object is closed.
        assert!(s.contains("\"maybe\""), "{schema:#}");
    }
}
