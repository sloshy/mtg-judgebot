//! The `judge.toml` side: the file as JSON for the form, and the form's
//! JSON merged back into the file without disturbing what did not change.
//!
//! The merge works on a `toml_edit` document, so comments, blank lines, key
//! order and the spelling of every unchanged value survive. A changed value
//! keeps its key's comments. A removed key or table goes with its comments.
//! A new table is appended. What the result *means* is not judged here:
//! the caller runs the loader over the rendered text (`check`).

use serde_json::{Map, Number, Value as Json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

/// Why the form's JSON cannot be written as TOML.
#[derive(Debug, thiserror::Error)]
pub enum BadDraft {
    /// The top level is not an object.
    #[error("the draft must be a JSON object")]
    NotATable,
    /// A number TOML cannot hold.
    #[error("{0}: not a TOML number")]
    Number(String),
}

/// The file's text as a document.
///
/// # Errors
/// The TOML syntax error, with its position.
pub fn parse(text: &str) -> Result<DocumentMut, toml_edit::TomlError> {
    text.parse()
}

/// The document as JSON: tables as objects, dates as strings.
#[must_use]
pub fn to_json(doc: &DocumentMut) -> Json {
    table_json(doc.as_table())
}

fn table_json(t: &Table) -> Json {
    Json::Object(
        t.iter()
            .filter_map(|(k, item)| item_json(item).map(|v| (k.to_owned(), v)))
            .collect(),
    )
}

fn item_json(item: &Item) -> Option<Json> {
    match item {
        Item::None => None,
        Item::Value(v) => Some(value_json(v)),
        Item::Table(t) => Some(table_json(t)),
        Item::ArrayOfTables(a) => Some(Json::Array(a.iter().map(table_json).collect())),
    }
}

fn value_json(v: &Value) -> Json {
    match v {
        Value::String(s) => Json::String(s.value().clone()),
        Value::Integer(i) => Json::Number((*i.value()).into()),
        Value::Float(f) => Number::from_f64(*f.value()).map_or(Json::Null, Json::Number),
        Value::Boolean(b) => Json::Bool(*b.value()),
        Value::Datetime(d) => Json::String(d.value().to_string()),
        Value::Array(a) => Json::Array(a.iter().map(value_json).collect()),
        Value::InlineTable(t) => Json::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), value_json(v)))
                .collect(),
        ),
    }
}

/// Make `doc` say `draft`, touching only what differs.
///
/// # Errors
/// [`BadDraft`].
pub fn apply(doc: &mut DocumentMut, draft: &Json) -> Result<(), BadDraft> {
    let Json::Object(draft) = draft else {
        return Err(BadDraft::NotATable);
    };
    // The file's opening comment is stored on its first table. Keep it when
    // that table goes: the part up to its last blank line is the file's,
    // the rest the table's own.
    let first = first_table(doc.as_table());
    merge_table(doc.as_table_mut(), draft, "", 0)?;
    if let Some((path, prefix)) = first
        && let Some(cut) = prefix.rfind("\n\n")
        && !has_table(doc.as_table(), &path)
        && let Some(table) = first_table_mut(doc.as_table_mut())
    {
        // One blank line between the file's comment and the table's.
        let header = prefix.get(..=cut).unwrap_or_default();
        let own = table
            .decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        let gap = if own.starts_with('\n') { "" } else { "\n" };
        let joined = format!("{header}{gap}{own}");
        table.decor_mut().set_prefix(joined);
    }
    Ok(())
}

/// The table printed first (by its place in the source), with its path and
/// leading decor.
fn first_table(root: &Table) -> Option<(Vec<String>, String)> {
    fn walk(t: &Table, path: &mut Vec<String>, best: &mut Option<(isize, Vec<String>, String)>) {
        for (k, item) in t {
            if let Item::Table(sub) = item {
                path.push(k.to_owned());
                if let Some(pos) = sub.position()
                    && best.as_ref().is_none_or(|(b, _, _)| pos < *b)
                {
                    let prefix = sub
                        .decor()
                        .prefix()
                        .and_then(|p| p.as_str())
                        .unwrap_or_default();
                    *best = Some((pos, path.clone(), prefix.to_owned()));
                }
                walk(sub, path, best);
                path.pop();
            }
        }
    }
    let mut best = None;
    walk(root, &mut vec![], &mut best);
    best.map(|(_, path, prefix)| (path, prefix))
}

fn has_table(root: &Table, path: &[String]) -> bool {
    let mut t = root;
    for k in path {
        match t.get(k) {
            Some(Item::Table(sub)) => t = sub,
            _ => return false,
        }
    }
    true
}

/// The first table that prints a header: by source position, else (all
/// new) the first in order.
fn first_table_mut(root: &mut Table) -> Option<&mut Table> {
    let path = first_table(root).map(|(p, _)| p).or_else(|| {
        fn first_explicit(t: &Table, path: &mut Vec<String>) -> bool {
            for (k, item) in t {
                if let Item::Table(sub) = item {
                    path.push(k.to_owned());
                    if !sub.is_implicit() || first_explicit(sub, path) {
                        return true;
                    }
                    path.pop();
                }
            }
            false
        }
        let mut path = vec![];
        first_explicit(root, &mut path).then_some(path)
    })?;
    let mut t = root;
    for k in &path {
        t = t.get_mut(k)?.as_table_mut()?;
    }
    Some(t)
}

/// The text of a new file saying `draft`.
///
/// # Errors
/// [`BadDraft`].
pub fn render_new(draft: &Json) -> Result<String, BadDraft> {
    let mut doc = DocumentMut::new();
    apply(&mut doc, draft)?;
    Ok(doc.to_string())
}

fn merge_table(
    table: &mut Table,
    draft: &Map<String, Json>,
    path: &str,
    depth: usize,
) -> Result<(), BadDraft> {
    let gone: Vec<String> = table
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| draft.get(k).is_none_or(Json::is_null))
        .collect();
    for k in gone {
        table.remove(&k);
    }
    for (k, v) in draft {
        if v.is_null() {
            continue;
        }
        let here = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        match (table.get_mut(k), v) {
            (Some(Item::Table(t)), Json::Object(o)) => {
                merge_table(t, o, &here, depth + 1)?;
                if t.is_empty() && !o.is_empty() {
                    // Every key of a sub-table moved below: nothing to do.
                } else if t.is_empty() && t.is_implicit() {
                    table.remove(k);
                }
            }
            (Some(Item::Value(old)), new) if same(&value_json(old), new) => {}
            (Some(Item::Value(old)), new) => {
                let decor = old.decor().clone();
                let mut value = to_value(new, &here)?;
                *value.decor_mut() = decor;
                table.insert(k, Item::Value(value));
            }
            (_, Json::Object(o)) => {
                let mut t = Table::new();
                // `[providers]` and `[models]` hold only tables: their own
                // header would be noise.
                t.set_implicit(depth == 0);
                merge_table(&mut t, o, &here, depth + 1)?;
                table.insert(k, Item::Table(t));
            }
            (_, new) => {
                table.insert(k, Item::Value(to_value(new, &here)?));
            }
        }
    }
    Ok(())
}

/// Equal as the loader would read them: `5.0` in the file and `5` from the
/// page are one number, so the file keeps its spelling.
fn same(old: &Json, new: &Json) -> bool {
    match (old, new) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64() && a.as_f64().is_some(),
        _ => old == new,
    }
}

fn to_value(v: &Json, path: &str) -> Result<Value, BadDraft> {
    Ok(match v {
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match (n.as_i64(), n.is_f64(), n.as_f64()) {
            (Some(i), _, _) => Value::from(i),
            // A float as such; an integer past i64 is not one TOML can hold.
            (None, true, Some(f)) if f.is_finite() => Value::from(f),
            _ => return Err(BadDraft::Number(path.to_owned())),
        },
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(a) => {
            let mut arr = Array::new();
            for x in a {
                arr.push(to_value(x, path)?);
            }
            Value::Array(arr)
        }
        Json::Object(o) => {
            let mut t = InlineTable::new();
            for (k, x) in o {
                if !x.is_null() {
                    t.insert(k, to_value(x, &format!("{path}.{k}"))?);
                }
            }
            Value::InlineTable(t)
        }
        Json::Null => Value::from(""),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn table<'a>(
        json: &'a mut Json,
        pointer: &str,
    ) -> Result<&'a mut Map<String, Json>, Box<dyn std::error::Error>> {
        json.pointer_mut(pointer)
            .and_then(Json::as_object_mut)
            .ok_or_else(|| pointer.into())
    }

    const FILE: &str = r#"# header comment

[providers.anthropic]
kind = "anthropic"                 # trailing
api_key_env = "ANTHROPIC_API_KEY"

# about ollama
[providers.ollama]
kind = "openai"
base_url = "http://ollama:11434/v1"

[models.extract]                   # cheap
provider = "ollama"
model = "qwen3:8b"
max_tokens = 2000

[models.synth]
provider = "anthropic"
model = "claude-opus-5-5"
"#;

    #[test]
    fn an_unchanged_draft_is_the_same_text() -> R {
        let mut doc = parse(FILE)?;
        let json = to_json(&doc);
        apply(&mut doc, &json)?;
        assert_eq!(doc.to_string(), FILE);
        Ok(())
    }

    #[test]
    fn a_change_keeps_comments_and_order() -> R {
        let mut doc = parse(FILE)?;
        let mut json = to_json(&doc);
        table(&mut json, "/providers/anthropic")?.insert("kind".into(), json!("anthropic"));
        let extract = table(&mut json, "/models/extract")?;
        extract.insert("model".into(), json!("qwen3:14b"));
        extract.remove("max_tokens");
        table(&mut json, "/models/synth")?
            .insert("pricing".into(), json!({"input": 4, "output": 20.5}));
        table(&mut json, "/providers")?.remove("ollama");
        apply(&mut doc, &json)?;
        let text = doc.to_string();
        assert!(text.starts_with("# header comment\n"), "{text}");
        assert!(
            text.contains("kind = \"anthropic\"                 # trailing"),
            "{text}"
        );
        assert!(
            text.contains("[models.extract]                   # cheap"),
            "{text}"
        );
        assert!(text.contains("model = \"qwen3:14b\""), "{text}");
        assert!(!text.contains("max_tokens"), "{text}");
        assert!(
            !text.contains("ollama]") && !text.contains("about ollama"),
            "{text}"
        );
        assert!(
            text.contains("[models.synth.pricing]\ninput = 4\noutput = 20.5"),
            "{text}"
        );
        Ok(())
    }

    #[test]
    fn the_file_comment_outlives_the_first_table() -> R {
        let mut doc = parse(FILE)?;
        let mut json = to_json(&doc);
        table(&mut json, "/providers")?.remove("anthropic");
        table(&mut json, "/models/synth")?.insert("provider".into(), json!("ollama"));
        apply(&mut doc, &json)?;
        let text = doc.to_string();
        assert!(
            text.starts_with("# header comment\n\n# about ollama\n[providers.ollama]"),
            "{text}"
        );
        Ok(())
    }

    #[test]
    fn numbers_keep_their_spelling_and_range() -> R {
        let text = "[models.synth.pricing]\ninput = 5.0  # usd\noutput = 25\n";
        let mut doc = parse(text)?;
        apply(
            &mut doc,
            &json!({"models": {"synth": {"pricing": {"input": 5, "output": 25}}}}),
        )?;
        assert_eq!(doc.to_string(), text);
        let big = json!({"x": u64::MAX});
        assert!(matches!(render_new(&big), Err(BadDraft::Number(_))));
        Ok(())
    }

    #[test]
    fn a_new_file_has_no_empty_parent_headers() -> R {
        let text = render_new(&json!({
            "providers": {"anthropic": {"kind": "anthropic", "api_key_env": "K"}},
            "models": {"extract": {"provider": "anthropic", "model": "m"},
                       "synth": {"provider": "anthropic", "model": "m", "effort": null}}
        }))?;
        assert!(
            !text.contains("[providers]\n") && !text.contains("[models]\n"),
            "{text}"
        );
        assert!(text.contains("[providers.anthropic]"), "{text}");
        assert!(!text.contains("effort"), "{text}");
        let back = to_json(&parse(&text)?);
        assert_eq!(back.pointer("/models/synth/model"), Some(&json!("m")));
        Ok(())
    }
}
