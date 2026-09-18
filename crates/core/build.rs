//! Generates `Category` from `data/categories.yaml` (workspace root).
//! The YAML is the single source of truth; the enum is exhaustive by construction.

use std::{collections::HashSet, env, error::Error, fmt::Write as _, fs, path::PathBuf};

#[derive(serde::Deserialize)]
struct File {
    categories: Vec<Entry>,
}

#[derive(serde::Deserialize)]
struct Entry {
    id: String,
    label: String,
    subsections: Vec<String>,
}

fn pascal(id: &str) -> String {
    id.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn generate(file: &File) -> Result<String, Box<dyn Error>> {
    let mut seen = HashSet::new();
    for e in &file.categories {
        let snake =
            e.id.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit());
        // Every `_`-separated word becomes part of an enum variant, so each must start with a letter.
        let words_start_with_letter =
            e.id.split('_')
                .all(|w| w.chars().next().is_some_and(|c| c.is_ascii_lowercase()));
        if !snake || !words_start_with_letter {
            return Err(format!(
                "category id must be snake_case and each word must start with a letter: {}",
                e.id
            )
            .into());
        }
        if !seen.insert(e.id.as_str()) {
            return Err(format!("duplicate category id: {}", e.id).into());
        }
    }

    let mut out = String::new();
    out.push_str("/// Question category. Generated from `data/categories.yaml` by `build.rs`.\n");
    out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]\n");
    out.push_str("#[serde(rename_all = \"snake_case\")]\n");
    out.push_str("pub enum Category {\n");
    for e in &file.categories {
        writeln!(
            out,
            "    /// {}\n    {},",
            e.label.replace('\n', " "),
            pascal(&e.id)
        )?;
    }
    out.push_str("}\n\nimpl Category {\n");
    out.push_str(
        "    /// Every category, in YAML order.\n    pub const ALL: &'static [Category] = &[\n",
    );
    for e in &file.categories {
        writeln!(out, "        Category::{},", pascal(&e.id))?;
    }
    out.push_str("    ];\n\n");
    out.push_str("    /// The stable `snake_case` identifier used in YAML, the DB and the wire.\n");
    out.push_str(
        "    #[must_use]\n    pub const fn id(self) -> &'static str {\n        match self {\n",
    );
    for e in &file.categories {
        writeln!(
            out,
            "            Category::{} => \"{}\",",
            pascal(&e.id),
            e.id
        )?;
    }
    out.push_str("        }\n    }\n\n");
    out.push_str("    /// Human-readable label.\n    #[must_use]\n    pub const fn label(self) -> &'static str {\n        match self {\n");
    for e in &file.categories {
        writeln!(
            out,
            "            Category::{} => {:?},",
            pascal(&e.id),
            e.label
        )?;
    }
    out.push_str("        }\n    }\n\n");
    out.push_str(
        "    /// CR sections/subsections always injected into `Context` for this category.\n",
    );
    out.push_str("    #[must_use]\n    pub const fn subsections(self) -> &'static [&'static str] {\n        match self {\n");
    for e in &file.categories {
        let subs: Vec<String> = e.subsections.iter().map(|s| format!("{s:?}")).collect();
        writeln!(
            out,
            "            Category::{} => &[{}],",
            pascal(&e.id),
            subs.join(", ")
        )?;
    }
    out.push_str("        }\n    }\n}\n");
    Ok(out)
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let yaml_path = manifest.join("../../data/categories.yaml");
    println!("cargo:rerun-if-changed={}", yaml_path.display());

    let raw = fs::read_to_string(&yaml_path)
        .map_err(|e| format!("cannot read {}: {e}", yaml_path.display()))?;
    let file: File =
        serde_yaml_ng::from_str(&raw).map_err(|e| format!("bad categories.yaml: {e}"))?;
    if file.categories.is_empty() {
        return Err("categories.yaml has no entries".into());
    }
    let out = generate(&file)?;
    let dest = PathBuf::from(env::var("OUT_DIR")?).join("category.rs");
    fs::write(&dest, out).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    Ok(())
}
