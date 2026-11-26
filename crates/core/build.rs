//! Generates `Category` from `data/categories.yaml` (workspace root).
//! The YAML is the single source of truth; the enum is exhaustive by construction.

use std::{collections::HashSet, env, error::Error, fmt::Write as _, fs, path::PathBuf};

#[derive(serde::Deserialize)]
struct File {
    categories: Vec<Entry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    label: String,
    sections: Vec<Reference>,
}

/// One entry of a category's `sections`: a section's title, or a mapping that
/// qualifies it.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Reference {
    Title(String),
    Qualified(Qualified),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Qualified {
    /// The title of the part the section is in (`Zones`), for a section title
    /// that several parts use (`General`).
    part: Option<String>,
    /// The section's title.
    section: String,
    /// The title of one rule in the section (`Lifelink`).
    rule: Option<String>,
    /// One rule or sub-rule that has no title, by its number within the
    /// section (`"5"`, `"5q"`). Quoted, so YAML does not read it as a number.
    number: Option<String>,
}

/// A title as the CR writes it: not blank, not padded, and not a rule number
/// (the reason this file names titles is that the numbers move).
fn title(category: &str, key: &str, value: &str) -> Result<(), Box<dyn Error>> {
    let numeric = value
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c.is_ascii_lowercase())
        && value.starts_with(|c: char| c.is_ascii_digit());
    if value.is_empty() || value.trim() != value {
        return Err(format!("category {category}: a blank or padded `{key}`: {value:?}").into());
    }
    if numeric {
        return Err(format!(
            "category {category}: `{key}` is a rule number ({value}); name the section by its \
             title, and a rule without a title as {{ section: <title>, number: \"<n>\" }}"
        )
        .into());
    }
    Ok(())
}

/// The Rust literal of one reference (`judge_core::taxonomy::SectionRef`).
fn reference(category: &str, r: &Reference) -> Result<String, Box<dyn Error>> {
    let q = match r {
        Reference::Title(t) => {
            title(category, "sections", t)?;
            return Ok(format!(
                "crate::taxonomy::SectionRef {{ part: None, section: {t:?}, within: crate::taxonomy::Within::Whole }}"
            ));
        }
        Reference::Qualified(q) => q,
    };
    title(category, "section", &q.section)?;
    if let Some(part) = &q.part {
        title(category, "part", part)?;
    }
    let within = match (&q.rule, &q.number) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "category {category}: {:?} has both `rule` and `number`; a rule with a title \
                 is named by it",
                q.section
            )
            .into());
        }
        (Some(rule), None) => {
            title(category, "rule", rule)?;
            format!("crate::taxonomy::Within::Titled({rule:?})")
        }
        (None, Some(number)) => {
            let digits = number.chars().take_while(char::is_ascii_digit).count();
            let letters = number.get(digits..).unwrap_or_default();
            let ok =
                digits > 0 && letters.len() <= 2 && letters.chars().all(|c| c.is_ascii_lowercase());
            if !ok {
                return Err(format!(
                    "category {category}: `number` under {:?} must be digits and at most two \
                     letters (\"5\", \"5q\"), got {number:?}",
                    q.section
                )
                .into());
            }
            format!("crate::taxonomy::Within::Numbered({number:?})")
        }
        (None, None) => "crate::taxonomy::Within::Whole".to_owned(),
    };
    Ok(format!(
        "crate::taxonomy::SectionRef {{ part: {:?}, section: {:?}, within: {within} }}",
        q.part, q.section
    ))
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
        "    /// The CR text always included for this category, named by title\n    /// ([`crate::taxonomy`]): the rule numbers are looked up in the loaded CR.\n",
    );
    out.push_str("    #[must_use]\n    pub const fn sections(self) -> &'static [crate::taxonomy::SectionRef] {\n        match self {\n");
    for e in &file.categories {
        let subs = e
            .sections
            .iter()
            .map(|r| reference(&e.id, r))
            .collect::<Result<Vec<String>, _>>()?;
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
