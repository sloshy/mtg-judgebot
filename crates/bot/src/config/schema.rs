//! The `judge.toml` shape as JSON Schema, for the config editor
//! (`judge-config`). It is generated from the loader's own types, so a knob
//! added to the file is a field in the editor with no edit there: the doc
//! comments are the help text, the enums the choices. What the types cannot
//! say is added from the tables the loader itself checks against:
//! `x-endpoints` on each endpoint-dependent key of an `anthropic` provider
//! ([`EndpointKey::on`]), and `x-built-endpoints`, the endpoints this binary was
//! built with. Everything else (a stage naming a missing provider, an
//! unpriced model) is left to the loader, which the editor runs on every
//! draft.

use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use super::{EndpointKey, EndpointKind, File};

/// The schema of a `judge.toml`.
#[must_use]
pub fn file_schema() -> Value {
    let schema = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<File>();
    let mut value = schema.to_value();
    // An absent optional key means "the loader's default", which the help
    // text states and which is often not one value (`auth` is x-api-key on a
    // proxy, `refusal_fallbacks` depends on the endpoint). schemars says `null`
    // there, which an editor would show as the default: drop it.
    drop_null_defaults(&mut value);
    if let Some(variant) = anthropic_variant(&mut value) {
        for key in EndpointKey::ALL {
            let endpoints: Map<String, Value> = EndpointKind::ALL
                .into_iter()
                .map(|d| (d.name().to_owned(), json!(key.on(d))))
                .collect();
            if let Some(prop) = variant.get_mut(key.name()).and_then(Value::as_object_mut) {
                prop.insert("x-endpoints".to_owned(), Value::Object(endpoints));
            }
        }
    }
    if let Some(root) = value.as_object_mut() {
        let built: Vec<&str> = EndpointKind::ALL
            .into_iter()
            .filter(|d| d.built())
            .map(EndpointKind::name)
            .collect();
        root.insert("x-built-endpoints".to_owned(), json!(built));
    }
    value
}

fn drop_null_defaults(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if o.get("default") == Some(&Value::Null) {
                o.remove("default");
            }
            o.values_mut().for_each(drop_null_defaults);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_null_defaults),
        _ => {}
    }
}

/// The `properties` of the `kind = "anthropic"` branch of `ProviderEntry`.
fn anthropic_variant(schema: &mut Value) -> Option<&mut Map<String, Value>> {
    schema
        .pointer_mut("/$defs/ProviderEntry/oneOf")?
        .as_array_mut()?
        .iter_mut()
        .find(|v| v.pointer("/properties/kind/const") == Some(&json!("anthropic")))?
        .pointer_mut("/properties")?
        .as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn every_endpoint_key_is_annotated() -> R {
        let mut schema = file_schema();
        let variant = anthropic_variant(&mut schema).ok_or("no anthropic variant")?;
        for key in EndpointKey::ALL {
            let endpoints = variant
                .get(key.name())
                .and_then(|p| p.get("x-endpoints"))
                .ok_or(key.name())?;
            assert_eq!(
                endpoints.as_object().map(Map::len),
                Some(EndpointKind::ALL.len())
            );
        }
        Ok(())
    }

    #[test]
    fn no_default_is_null() {
        assert!(!file_schema().to_string().contains(r#""default":null"#));
    }

    #[test]
    fn the_schema_names_every_table_of_the_example() {
        let schema = file_schema();
        for def in [
            "ProviderEntry",
            "ModelsEntry",
            "StageEntry",
            "PricingEntry",
            "EmbedEntry",
        ] {
            assert!(schema.pointer(&format!("/$defs/{def}")).is_some(), "{def}");
        }
    }
}
