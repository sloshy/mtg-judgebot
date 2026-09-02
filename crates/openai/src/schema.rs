//! schemars → `OpenAI` strict-mode schema (the same subset for
//! `response_format.json_schema` with `strict: true` and for strict tools).
//!
//! Strict mode supports `anyOf` but not `oneOf`, requires
//! `additionalProperties: false` on every object, requires **every property
//! to be listed in `required`**, and rejects `format`, `default` and the
//! numeric/string/array constraints Anthropic's subset also rejects. One
//! `RecursiveTransform` applies all of that to every subschema, `$defs`
//! included.
//!
//! A property that was optional becomes required *as its own type*: an
//! `Option<T>` field is already nullable in the schemars output (`anyOf
//! [T, null]` or `type: [T, "null"]`), and a `#[serde(default)]` field is
//! not — serde would reject `null` for it on the way back in, so widening it
//! to `null` here would only turn a satisfiable schema into a decode
//! failure. The model must emit such a field (as `[]`, say); it always can.
//! Removed constraints are still enforced client-side by serde/nutype when
//! the response is deserialized.

use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings, transform::{Transform, transform_subschemas}};
use serde_json::Value;

/// Keys strict mode rejects; stripped from every subschema. The constraint
/// keys are the ones Anthropic's subset strips too; `format` and `default`
/// are `OpenAI`'s own exclusions.
pub const UNSUPPORTED_KEYS: &[&str] = &[
    "format",
    "default",
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

/// Rewrites a schema in place so it fits `OpenAI` strict mode.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiStrict;

fn is_object(obj: &serde_json::Map<String, Value>) -> bool {
    obj.contains_key("properties")
        || obj.get("type").is_some_and(|t| t == "object" || t.as_array().is_some_and(|a| a.iter().any(|x| x == "object")))
}

impl Transform for OpenAiStrict {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(obj) = schema.as_object_mut() {
            if let Some(one_of) = obj.remove("oneOf") {
                obj.insert("anyOf".to_owned(), one_of);
            }
            if is_object(obj) {
                obj.insert("additionalProperties".to_owned(), Value::Bool(false));
                let names: Vec<Value> = obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|p| p.keys().map(|k| Value::String(k.clone())).collect())
                    .unwrap_or_default();
                obj.insert("required".to_owned(), Value::Array(names));
            }
            for k in UNSUPPORTED_KEYS {
                obj.remove(*k);
            }
        }
        transform_subschemas(self, schema);
    }
}

/// Generate the strict schema for `T`.
#[must_use]
pub fn openai_schema<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::draft2020_12().with_transform(OpenAiStrict);
    SchemaGenerator::new(settings).into_root_schema_for::<T>().to_value()
}

/// Apply the strict subset to an already generated (untransformed) schema,
/// as a [`judge_llm::OutputSchema`] or [`judge_llm::ToolSpec`] carries it.
/// Gives the same result as [`openai_schema`] for the same type.
#[must_use]
pub fn to_openai_strict(schema: &Schema) -> Value {
    let mut s = schema.clone();
    OpenAiStrict.transform(&mut s);
    s.to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{Extraction, Verdict};
    use judge_llm::LookupRulesInput;

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

    /// Every object closed with every property required, no `oneOf`, no unsupported keys anywhere.
    fn assert_strict(schema: &Value) {
        let mut objects = 0;
        walk(schema, &mut |m| {
            assert!(!m.contains_key("oneOf"), "oneOf present: {schema:#}");
            for k in UNSUPPORTED_KEYS {
                assert!(!m.contains_key(*k), "{k} present: {schema:#}");
            }
            if is_object(m) {
                objects += 1;
                assert_eq!(m.get("additionalProperties"), Some(&Value::Bool(false)), "open object: {schema:#}");
                let props: Vec<&String> = m.get("properties").and_then(Value::as_object).map(|p| p.keys().collect()).unwrap_or_default();
                let required: Vec<&str> = m.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                assert_eq!(props.iter().map(|s| s.as_str()).collect::<Vec<_>>(), required, "not every property required: {schema:#}");
            }
        });
        assert!(objects >= 1, "{schema:#}");
    }

    #[test]
    fn transforming_after_generation_equals_generating_with_the_transform() {
        assert_eq!(to_openai_strict(&judge_llm::schema_of::<Verdict>()), openai_schema::<Verdict>());
        assert_eq!(to_openai_strict(&judge_llm::schema_of::<Extraction>()), openai_schema::<Extraction>());
        assert_eq!(to_openai_strict(&judge_llm::schema_of::<Outer>()), openai_schema::<Outer>());
    }

    #[test]
    fn the_pipeline_schemas_fit_strict_mode() {
        for (name, schema) in [("Verdict", openai_schema::<Verdict>()), ("Extraction", openai_schema::<Extraction>()), ("LookupRulesInput", openai_schema::<LookupRulesInput>())] {
            assert_strict(&schema);
            let s = schema.to_string();
            assert!(!s.contains("uint32") && !s.contains("\"uuid\"") && !s.contains("\"format\""), "{name}: {schema:#}");
        }
        // The tagged Citation enum became anyOf and kept its discriminator.
        let s = openai_schema::<Verdict>().to_string();
        assert!(s.contains("anyOf") && s.contains("\"kind\""), "{s}");
        // `secondary` (serde default, so not required by schemars) is now required, still an array, not nullable.
        let e = openai_schema::<Extraction>();
        let required = e.pointer("/required").and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(required.iter().any(|r| r == "secondary"), "{e:#}");
        assert_eq!(e.pointer("/properties/secondary/type"), Some(&Value::String("array".into())), "{e:#}");
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
        #[serde(default)]
        tags: Vec<String>,
        #[schemars(default)]
        name: String,
    }

    #[test]
    fn optionals_stay_nullable_defaults_become_required_and_constraints_go() {
        let schema = openai_schema::<Outer>();
        assert_strict(&schema);
        let s = schema.to_string();
        assert!(!s.contains("uint8") && !s.contains("\"default\""), "{schema:#}");
        // The Option<Inner> field keeps whatever nullable form schemars gave it.
        let maybe = schema.pointer("/properties/maybe").cloned().unwrap_or_default().to_string();
        assert!(maybe.contains("null"), "{maybe}");
        let required: Vec<&str> = schema.pointer("/required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        assert_eq!(required, ["count", "list", "maybe", "name", "tags"], "every property, in the map's (sorted) order");
        // $defs are transformed too.
        let inner = schema.pointer("/$defs/Inner").cloned().unwrap_or_default();
        assert_eq!(inner.pointer("/additionalProperties"), Some(&Value::Bool(false)));
        assert!(!inner.to_string().contains("pattern"), "{inner:#}");
    }
}
