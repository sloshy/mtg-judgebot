//! Comprehensive Rules parser: txt (local path or URL) -> `rules` chunks + `glossary`.
//!
//! Chunking (ARCHITECTURE.md §2), at *rule* granularity:
//!
//! * One row per rule `X.Y` (e.g. `702.19`): `body` = the rule's own line followed by
//!   every lettered sub-rule (`702.19a`, `702.19b`, …) in order, one per line, with
//!   each `Example:` paragraph on its own line directly after the line it belongs to
//!   (so 903.3's example sits under 903.3, not under 903.3e); `examples` is empty;
//!   `parent_id = NULL`; `subsection = "702"`; `heading` = the rule's first-line text
//!   up to its first sentence-ending period (so `702.19. Trample` → `Trample`), or the
//!   section title (`Interaction of Continuous Effects`) when that first sentence is
//!   prose longer than `MAX_HEADING_CHARS`.
//! * One row per lettered sub-rule (`702.19b`): `body` = just that line, `examples` =
//!   just its own examples, `parent_id = "702.19"`, `heading` = the parent's heading.
//!   Rows with a `parent_id` are leaves; rows without are rule chunks.
//! * No rows for three-digit sections (`702.`) or top-level parts (`7.`); the section
//!   title is only used as the heading fallback.
//! * The table of contents is skipped: nothing is emitted until the first line that
//!   carries a rule body (`X.Y. text`), and section titles seen before that only set
//!   the "current section" so the first real section is still known.
//! * Indented continuation lines are appended to the preceding rule line or example.
//! * The glossary (`Glossary` line after the rules) is parsed as blank-line separated
//!   blocks: first line = term, remaining lines = definition. Parsing stops at `Credits`.
//!
//! The CR version is taken from the file name (`MagicCompRules 20260819.txt` -> `20260819`),
//! falling back to the "These rules are effective as of …" line.
//!
//! `ingest rules latest` ([`run_latest`]) finds the current release itself: Wizards'
//! rules page links the `.txt` of the release in force, and its file name carries the
//! version, so a new CR is detected without downloading it — the page is compared
//! against `max(rules.cr_version)` and only a differing version is fetched and stored.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use judge_core::taxonomy::{Outline, Section, Unresolved};
use judge_core::{Category, CrVersion, EXAMPLE_PREFIX, GlossaryEntry, RuleChunk, RuleId};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::RefreshLease;
use super::renumber::{StoredRule, renumber_map, rewrite_call};

/// Rows per `INSERT` statement.
const BATCH: usize = 200;
/// A first sentence longer than this is prose, not a title (`Trample`, `Lifelink`):
/// the section title is used as the heading instead so `tsv` and the rendered
/// prompt do not repeat the rule's opening sentence.
const MAX_HEADING_CHARS: usize = 60;
/// Where Wizards publishes the current Comprehensive Rules, one download link per
/// format. Only the `.txt` is parsed here.
pub const RULES_PAGE_URL: &str = "https://magic.wizards.com/en/rules";
/// Scryfall-style polite identification; Wizards' CDN does not require it but it costs nothing.
const USER_AGENT: &str = concat!(
    "mtg-judgebot-ingest/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/sloshy/mtg-judgebot)"
);

/// Parse and load the CR from `source` (path or `http(s)://` URL, cached under `cache_dir`).
///
/// Returns what the taxonomy names in it ([`Taxonomy`]).
///
/// # Errors
/// On download, parse or database failure.
pub async fn run(
    lease: &mut RefreshLease,
    source: &str,
    cache_dir: &Path,
) -> anyhow::Result<Taxonomy> {
    let text = fetch(source, cache_dir).await?;
    let parsed = match parse(&text, source) {
        Ok(parsed) => parsed,
        Err(e) => {
            forget_cached(source, cache_dir);
            return Err(e);
        }
    };
    tracing::info!(
        cr_version = %parsed.cr_version,
        rules = parsed.rules.iter().filter(|r| r.parent_id.is_none()).count(),
        leaves = parsed.rules.iter().filter(|r| r.parent_id.is_some()).count(),
        glossary = parsed.glossary.len(),
        sections = parsed.sections.len(),
        "parsed comprehensive rules"
    );
    if parsed.rules.is_empty() {
        anyhow::bail!("no rules parsed from {source}");
    }
    store(lease.pool(), &parsed).await
}

/// What the taxonomy (`data/categories.yaml`, [`judge_core::taxonomy`]) names in
/// the loaded CR, as the write of the `categories` table found it. Part of
/// [`Outcome`], so stored in a refresh run's record.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Taxonomy {
    /// Every entry that names nothing in the loaded CR. Its category is
    /// answered without that text until a release names the current title.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<Unresolved>,
    /// Those of [`Self::unresolved`] that resolved at the previous write: the
    /// news, told once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<Unresolved>,
    /// The sections no category names, whole or in part. Listed by a run that
    /// loaded a release; a run that found the CR unchanged leaves it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncovered: Vec<Section>,
    /// Those of [`Self::uncovered`] whose title the previous release did not
    /// have: sections a new release added that no category names. Empty when
    /// no release was stored before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<Section>,
}

impl Taxonomy {
    /// Whether there is nothing to report.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.unresolved.is_empty()
            && self.stopped.is_empty()
            && self.uncovered.is_empty()
            && self.added.is_empty()
    }

    /// One line per finding. An entry that stopped resolving and a new
    /// section nobody names are warnings; the standing lists are INFO.
    fn log(&self) {
        for u in &self.stopped {
            tracing::warn!(
                category = u.category,
                reference = u.reference,
                reason = %u.reason,
                "a taxonomy entry names nothing in the loaded CR; its category is answered \
                 without that text until data/categories.yaml names the current title"
            );
        }
        if !self.unresolved.is_empty() {
            let entries: Vec<String> = self.unresolved.iter().map(ToString::to_string).collect();
            tracing::info!(count = entries.len(), entries = ?entries, "taxonomy entries that name nothing in the loaded CR");
        }
        for s in &self.added {
            tracing::warn!(
                section = %s,
                "the loaded CR adds a section no category names; decide in data/categories.yaml \
                 whether one should"
            );
        }
        if !self.uncovered.is_empty() {
            let sections: Vec<String> = self.uncovered.iter().map(ToString::to_string).collect();
            tracing::info!(count = sections.len(), sections = ?sections, "CR sections no category names");
        }
    }
}

/// What [`run_latest`] found on the rules page and did about it. Stored in a
/// refresh run's record ([`super::runs`]), tagged `"cr"`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cr", rename_all = "snake_case")]
pub enum Outcome {
    /// The published release is the one already in `rules`; nothing was downloaded.
    Unchanged {
        /// The stored and published version.
        version: String,
        /// What the taxonomy names in it. Left out of the stored object when
        /// there is nothing to report.
        #[serde(default, skip_serializing_if = "Taxonomy::is_empty")]
        taxonomy: Taxonomy,
    },
    /// A release was fetched, parsed and stored: a different one is published,
    /// or the stored one has no section titles yet (it was loaded by a release
    /// that kept none), so it was loaded again.
    Updated {
        /// The version in the link's file name, or `"unknown"` when it carries none.
        version: String,
        /// Where it was downloaded from.
        url: String,
        /// What the taxonomy names in it. Left out of the stored object when
        /// there is nothing to report.
        #[serde(default, skip_serializing_if = "Taxonomy::is_empty")]
        taxonomy: Taxonomy,
    },
}

impl Outcome {
    /// What the taxonomy names in the CR this run left loaded.
    #[must_use]
    pub const fn taxonomy(&self) -> &Taxonomy {
        match self {
            Self::Unchanged { taxonomy, .. } | Self::Updated { taxonomy, .. } => taxonomy,
        }
    }
}

/// Load the CR release currently linked from [`RULES_PAGE_URL`], unless `rules`
/// already holds that version.
///
/// A link whose file name carries no version is loaded unconditionally: `store` is
/// idempotent, so the only cost of being unable to tell is a download.
///
/// # Errors
/// If the rules page is unreachable or links no `MagicCompRules*.txt`, or on any
/// failure of [`run`].
pub async fn run_latest(lease: &mut RefreshLease, cache_dir: &Path) -> anyhow::Result<Outcome> {
    let client = super::http_client(USER_AGENT)?;
    let html = client
        .get(RULES_PAGE_URL)
        .send()
        .await
        .with_context(|| format!("GET {RULES_PAGE_URL}"))?
        .error_for_status()
        .with_context(|| format!("GET {RULES_PAGE_URL}"))?
        .text()
        .await?;
    let url = find_cr_txt_url(&html)
        .ok_or_else(|| anyhow::anyhow!("no MagicCompRules .txt link found on {RULES_PAGE_URL}"))?;
    let published = version_from_source(&url);
    let current: Option<String> = sqlx::query_scalar!("SELECT max(cr_version) FROM rules")
        .fetch_one(lease.pool())
        .await
        .context("reading the stored CR version")?;
    tracing::info!(url, published = ?published, stored = ?current, "current comprehensive rules release");
    if let (Some(version), Some(stored)) = (&published, &current)
        && version == stored
    {
        // The taxonomy's titles are looked up in `rule_sections`, which a
        // release loaded before that table existed left empty: without them
        // the stored release is loaded again (`store` is idempotent).
        let titled: bool = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM rule_sections WHERE cr_version = $1) AS "titled!""#,
            version
        )
        .fetch_one(lease.pool())
        .await
        .context("reading the stored section titles")?;
        if titled {
            tracing::info!(%version, "comprehensive rules unchanged; nothing to do");
            // The taxonomy is compiled in, so a release can change it with no
            // new CR to load: the table follows at that release's first refresh.
            let mut tx = lease.pool().begin().await.context("begin")?;
            let taxonomy = sync_categories(&mut tx, &[]).await?;
            tx.commit().await.context("committing categories")?;
            taxonomy.log();
            return Ok(Outcome::Unchanged {
                version: version.clone(),
                // The standing list is in the record of the run that loaded
                // the release; a daily copy of it would say nothing new.
                taxonomy: Taxonomy {
                    uncovered: Vec::new(),
                    ..taxonomy
                },
            });
        }
        tracing::info!(%version, "the stored comprehensive rules have no section titles; loading the release again");
    }
    let taxonomy = run(lease, &url, cache_dir).await?;
    let version = published.unwrap_or_else(|| "unknown".to_owned());
    Ok(Outcome::Updated {
        version,
        url,
        taxonomy,
    })
}

/// The download link for the current CR text on the rules page: the `href` whose
/// file name mentions `MagicCompRules` and ends in `.txt`. When several qualify the
/// highest version wins (ties: first seen). Spaces are percent-encoded so the result
/// is a valid URL; Wizards links `MagicCompRules 20260819.txt` with a literal space.
#[must_use]
fn find_cr_txt_url(html: &str) -> Option<String> {
    let mut best: Option<(Option<String>, String)> = None;
    for quote in ['"', '\''] {
        let opener = format!("href={quote}");
        for (i, _) in html.match_indices(opener.as_str()) {
            let Some(rest) = html.get(i + opener.len()..) else {
                continue;
            };
            let Some(href) = rest.find(quote).and_then(|end| rest.get(..end)) else {
                continue;
            };
            let href = href.trim();
            let file = href.rsplit('/').next().unwrap_or(href);
            let file = file.split(['?', '#']).next().unwrap_or(file);
            if !(file.contains("MagicCompRules") && file.to_ascii_lowercase().ends_with(".txt")) {
                continue;
            }
            let version = version_from_source(href);
            let better = best.as_ref().is_none_or(|(v, _)| version > *v);
            if better {
                best = Some((version, href.replace(' ', "%20")));
            }
        }
    }
    best.map(|(_, url)| url)
}

/// Read `source`: a local path, or an `http(s)` URL downloaded once into `cache_dir`
/// (file name = the URL's last path segment, percent-decoded).
async fn fetch(source: &str, cache_dir: &Path) -> anyhow::Result<String> {
    if !(source.starts_with("http://") || source.starts_with("https://")) {
        return std::fs::read_to_string(source).with_context(|| format!("reading {source}"));
    }
    let name = cache_file_name(source);
    let path = cache_dir.join(&name);
    if let Ok(cached) = std::fs::read_to_string(&path) {
        tracing::info!(path = %path.display(), "using cached CR");
        return Ok(cached);
    }
    tracing::info!(url = source, "downloading CR");
    let client = super::http_client(USER_AGENT)?;
    let resp = client
        .get(source)
        .header(reqwest::header::ACCEPT, "text/plain, */*")
        .send()
        .await
        .with_context(|| format!("GET {source}"))?
        .error_for_status()
        .with_context(|| format!("GET {source}"))?;
    let bytes = resp.bytes().await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;
    // Written whole, then renamed, so a process killed mid-write leaves a
    // `.part` file a later run ignores, never a short file it trusts.
    let part = cache_dir.join(format!("{name}.part"));
    std::fs::write(&part, &text).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, &path).with_context(|| format!("renaming {}", part.display()))?;
    Ok(text)
}

/// Remove the cached copy of a downloaded `source` that did not parse, so the
/// next run downloads it again instead of failing on it forever.
fn forget_cached(source: &str, cache_dir: &Path) {
    if !(source.starts_with("http://") || source.starts_with("https://")) {
        return;
    }
    let path = cache_dir.join(cache_file_name(source));
    if std::fs::remove_file(&path).is_ok() {
        tracing::warn!(path = %path.display(), "removed a cached CR that did not parse; the next run downloads it again");
    }
}

/// `…/MagicCompRules%2020260819.txt` -> `MagicCompRules 20260819.txt`.
fn cache_file_name(url: &str) -> String {
    let last = url.rsplit('/').next().unwrap_or(url);
    let last = last.split(['?', '#']).next().unwrap_or(last);
    let decoded = percent_decode(last);
    let safe: String = decoded
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " ._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.trim().is_empty() {
        "MagicCompRules.txt".to_owned()
    } else {
        safe
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let decoded = (b == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(d) = decoded {
            out.push(d);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Everything the parser extracts from one CR text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCr {
    /// CR effective date, `YYYYMMDD`.
    pub cr_version: CrVersion,
    /// Rule chunks and leaves, in document order (each rule chunk before its leaves).
    pub rules: Vec<RuleChunk>,
    /// Glossary entries in document order (terms unique).
    pub glossary: Vec<GlossaryEntry>,
    /// The sections that have rules, in document order, each with its part's
    /// title: what the taxonomy's titles are looked up in.
    pub sections: Vec<Section>,
}

/// What one line of the CR is.
#[derive(Debug, Clone, Copy)]
enum Line<'a> {
    Blank,
    /// Indented continuation of the previous line.
    Continuation(&'a str),
    Example(&'a str),
    /// `7. Additional Rules`
    Part {
        title: &'a str,
    },
    /// `702. Keyword Abilities`
    Section {
        id: &'a str,
        title: &'a str,
    },
    /// `702.19. Trample`
    Rule {
        id: &'a str,
        text: &'a str,
    },
    /// `702.19b The controller …`
    Leaf {
        id: &'a str,
        parent: &'a str,
        text: &'a str,
    },
    /// Anything else (prose, part headers such as `7. Additional Rules`, `Glossary`).
    Other(&'a str),
}

fn classify(raw: &str) -> Line<'_> {
    let line = raw.trim_end_matches(['\r', '\n']);
    if line
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .is_empty()
    {
        return Line::Blank;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return Line::Continuation(line.trim());
    }
    let line = line.trim_start_matches('\u{feff}').trim_end();
    if let Some(rest) = line.strip_prefix("Example:") {
        return Line::Example(rest.trim());
    }
    if let Some(l) = classify_numbered(line) {
        return l;
    }
    Line::Other(line)
}

/// `7. Title` | `702. Title` | `702.19. text` | `702.19b text`, ASCII digits only.
fn classify_numbered(line: &str) -> Option<Line<'_>> {
    if let Some(title) = line
        .strip_prefix(|c: char| c.is_ascii_digit())
        .and_then(|rest| rest.strip_prefix(". "))
    {
        return Some(Line::Part {
            title: title.trim(),
        });
    }
    let id = line
        .get(..3)
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?;
    let after = line.get(3..)?;
    let after = after.strip_prefix('.')?;
    if let Some(title) = after.strip_prefix(' ') {
        return Some(Line::Section {
            id,
            title: title.trim(),
        });
    }
    let digits = after.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rule_id = line.get(..3 + 1 + digits)?;
    let tail = after.get(digits..)?;
    // `702.19. Trample` is the usual form; `606.5 If the total cost…` (no period) also occurs.
    if let Some(text) = tail.strip_prefix(". ").or_else(|| tail.strip_prefix(' ')) {
        return Some(Line::Rule {
            id: rule_id,
            text: text.trim(),
        });
    }
    if tail == "." {
        return Some(Line::Rule {
            id: rule_id,
            text: "",
        });
    }
    // One or two lowercase letters (`704.5z`, `704.5aa`), optionally followed by a
    // period (`119.1d. In a two-player Brawl game…`), then a space.
    let letters = tail.bytes().take_while(u8::is_ascii_lowercase).count();
    if !(1..=2).contains(&letters) {
        return None;
    }
    let rest = tail.get(letters..)?;
    let text = rest.strip_prefix('.').unwrap_or(rest).strip_prefix(' ')?;
    let leaf_id = line.get(..3 + 1 + digits + letters)?;
    Some(Line::Leaf {
        id: leaf_id,
        parent: rule_id,
        text: text.trim(),
    })
}

/// A rule under construction: its own line, sub-rule lines, and examples.
#[derive(Debug, Default)]
struct RuleBuilder {
    id: String,
    subsection: String,
    heading: String,
    /// The rule-level body in CR order, so an example stays under the line it
    /// belongs to.
    paragraphs: Vec<Paragraph>,
    leaves: Vec<LeafBuilder>,
}

/// One paragraph of a rule-level body.
#[derive(Debug)]
enum Paragraph {
    /// The rule's own line or a sub-rule's line, id first.
    Line(String),
    /// An example, without its `Example:` label.
    Example(String),
}

impl Paragraph {
    fn text_mut(&mut self) -> &mut String {
        match self {
            Self::Line(t) | Self::Example(t) => t,
        }
    }
}

/// The rule-level body: one paragraph per line, examples labelled.
fn body(paragraphs: &[Paragraph]) -> String {
    paragraphs
        .iter()
        .map(|p| match p {
            Paragraph::Line(t) => t.clone(),
            Paragraph::Example(t) => format!("{EXAMPLE_PREFIX}{t}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Default)]
struct LeafBuilder {
    id: String,
    line: String,
    examples: Vec<String>,
}

/// Where the most recent text went, so continuation lines can follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Last {
    #[default]
    None,
    RuleLine,
    LeafLine,
    /// An example of the rule itself (not of a leaf).
    RuleExample,
    LeafExample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Toc,
    Rules,
    Glossary,
    Done,
}

fn heading_of(text: &str, section_title: &str) -> String {
    if text.is_empty() {
        return section_title.to_owned();
    }
    let end = text
        .find(". ")
        .or_else(|| text.strip_suffix('.').map(str::len))
        .unwrap_or(text.len());
    let head = text.get(..end).unwrap_or(text).trim();
    let head = if head.is_empty() || head.chars().count() > MAX_HEADING_CHARS {
        section_title
    } else {
        head
    };
    head.to_owned()
}

/// Parser state machine over the CR's lines.
#[derive(Debug, Default)]
struct Parser {
    mode: Mode,
    effective_line: Option<String>,
    part_title: String,
    /// The number of the section whose title is `section_title`.
    section_id: String,
    section_title: String,
    /// Every section a rule was read under, in order.
    sections: Vec<Section>,
    rules: Vec<RuleBuilder>,
    current: Option<RuleBuilder>,
    last: Last,
    glossary_blocks: Vec<Vec<String>>,
    glossary_open: bool,
}

impl Parser {
    fn feed(&mut self, raw: &str) {
        let line = classify(raw);
        match self.mode {
            Mode::Done => {}
            Mode::Toc => self.toc(line),
            Mode::Rules => self.rule_line(line),
            Mode::Glossary => self.glossary_line(raw, line),
        }
    }

    fn toc(&mut self, line: Line<'_>) {
        match line {
            Line::Other(t) if t.starts_with("These rules are effective as of") => {
                self.effective_line = Some(t.to_owned());
            }
            Line::Part { title } => title.clone_into(&mut self.part_title),
            Line::Section { id, title } => self.start_section(id, title),
            Line::Rule { id, text } => {
                self.mode = Mode::Rules;
                self.start_rule(id, text);
            }
            _ => {}
        }
    }

    fn start_section(&mut self, id: &str, title: &str) {
        id.clone_into(&mut self.section_id);
        title.clone_into(&mut self.section_title);
    }

    fn start_rule(&mut self, id: &str, text: &str) {
        // A section is recorded when its first rule is read, so the table of
        // contents (titles with no rules under them) records none, and the
        // part is the one the rule sits in.
        if id.get(..3) == Some(self.section_id.as_str())
            && !self.sections.iter().any(|s| s.id == self.section_id)
        {
            self.sections.push(Section {
                id: self.section_id.clone(),
                title: self.section_title.clone(),
                part: self.part_title.clone(),
            });
        }
        self.rules.extend(self.current.take());
        self.current = Some(new_rule(id, text, &self.section_title));
        self.last = Last::RuleLine;
    }

    fn rule_line(&mut self, line: Line<'_>) {
        match line {
            Line::Blank => {}
            Line::Part { title } => title.clone_into(&mut self.part_title),
            Line::Section { id, title } => self.start_section(id, title),
            Line::Rule { id, text } => self.start_rule(id, text),
            Line::Leaf { id, parent, text } => {
                if self.current.as_ref().is_none_or(|c| c.id != parent) {
                    // A leaf whose parent rule line is missing: synthesise the parent.
                    self.start_rule(parent, "");
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.paragraphs.push(Paragraph::Line(format!("{id} {text}")));
                    cur.leaves.push(LeafBuilder {
                        id: id.to_owned(),
                        line: format!("{id} {text}"),
                        examples: Vec::new(),
                    });
                }
                self.last = Last::LeafLine;
            }
            Line::Example(t) => {
                let Some(cur) = self.current.as_mut() else {
                    return;
                };
                cur.paragraphs.push(Paragraph::Example(t.to_owned()));
                if matches!(self.last, Last::LeafLine | Last::LeafExample) {
                    if let Some(leaf) = cur.leaves.last_mut() {
                        leaf.examples.push(t.to_owned());
                    }
                    self.last = Last::LeafExample;
                } else {
                    self.last = Last::RuleExample;
                }
            }
            Line::Continuation(t) => {
                if let Some(cur) = self.current.as_mut() {
                    append_continuation(cur, self.last, t);
                }
            }
            // Any other prose inside the rules body is dropped; warn so format
            // drift (a rule line the classifier no longer recognises) is visible.
            Line::Other(t) => {
                if t == "Glossary" {
                    self.rules.extend(self.current.take());
                    self.mode = Mode::Glossary;
                } else {
                    tracing::warn!(line = t, "unrecognised line inside the rules body; dropped");
                }
            }
        }
    }

    fn glossary_line(&mut self, raw: &str, line: Line<'_>) {
        match line {
            Line::Blank => self.glossary_open = false,
            Line::Other("Credits") if !self.glossary_open => self.mode = Mode::Done,
            Line::Continuation(t) => {
                if let Some(l) = self.glossary_blocks.last_mut().and_then(|b| b.last_mut()) {
                    l.push(' ');
                    l.push_str(t);
                }
            }
            Line::Part { .. }
            | Line::Section { .. }
            | Line::Rule { .. }
            | Line::Leaf { .. }
            | Line::Example(_)
            | Line::Other(_) => {
                // Glossary text is free prose; use the raw line rather than the classification.
                let t = raw.trim_end_matches(['\r', '\n']).trim();
                if self.glossary_open {
                    if let Some(block) = self.glossary_blocks.last_mut() {
                        block.push(t.to_owned());
                    }
                } else {
                    self.glossary_blocks.push(vec![t.to_owned()]);
                    self.glossary_open = true;
                }
            }
        }
    }
}

/// Parse a CR text. `source` (file name or URL) is the preferred source of the version.
///
/// # Errors
/// If no CR version can be determined, or a parsed id is malformed.
fn parse(text: &str, source: &str) -> anyhow::Result<ParsedCr> {
    let mut p = Parser::default();
    for raw in text.lines() {
        p.feed(raw);
    }
    p.rules.extend(p.current.take());
    // The file ends with its Credits, after the glossary. A text that stops
    // before them was cut short (a download or a write that died), and
    // loading it would delete every rule it does not reach.
    anyhow::ensure!(
        p.mode == Mode::Done,
        "the CR text from {source} ends before its Credits section; it is incomplete (a truncated download?)"
    );

    let cr_version = version_from_source(source)
        .or_else(|| {
            p.effective_line
                .as_deref()
                .and_then(version_from_effective_line)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot determine CR version from {source:?} or an 'effective as of' line"
            )
        })?;
    let cr_version =
        CrVersion::try_new(cr_version).map_err(|e| anyhow::anyhow!("bad CR version: {e}"))?;

    let mut rules = Vec::with_capacity(p.rules.len() * 3);
    for r in std::mem::take(&mut p.rules) {
        let id = rule_id(&r.id)?;
        let subsection = rule_id(&r.subsection)?;
        rules.push(RuleChunk {
            id: id.clone(),
            parent_id: None,
            subsection: subsection.clone(),
            heading: r.heading.clone(),
            body: body(&r.paragraphs),
            examples: Vec::new(),
            cr_version: cr_version.clone(),
        });
        for leaf in r.leaves {
            rules.push(RuleChunk {
                id: rule_id(&leaf.id)?,
                parent_id: Some(id.clone()),
                subsection: subsection.clone(),
                heading: r.heading.clone(),
                body: leaf.line,
                examples: leaf.examples,
                cr_version: cr_version.clone(),
            });
        }
    }

    let mut seen = std::collections::HashSet::new();
    let glossary = p
        .glossary_blocks
        .into_iter()
        .filter_map(|block| {
            let mut it = block.into_iter();
            let term = it.next()?.trim().to_owned();
            let text = it.collect::<Vec<_>>().join("\n");
            (!term.is_empty() && !text.trim().is_empty() && seen.insert(term.to_lowercase()))
                .then_some(GlossaryEntry { term, text })
        })
        .collect();

    Ok(ParsedCr {
        cr_version,
        rules,
        glossary,
        sections: p.sections,
    })
}

fn new_rule(id: &str, text: &str, section_title: &str) -> RuleBuilder {
    let line = if text.is_empty() {
        format!("{id}.")
    } else {
        format!("{id}. {text}")
    };
    RuleBuilder {
        id: id.to_owned(),
        subsection: id.get(..3).unwrap_or(id).to_owned(),
        heading: heading_of(text, section_title),
        paragraphs: vec![Paragraph::Line(line)],
        leaves: Vec::new(),
    }
}

fn append_continuation(cur: &mut RuleBuilder, last: Last, t: &str) {
    fn extend(s: &mut String, t: &str) {
        s.push(' ');
        s.push_str(t);
    }
    if last == Last::None {
        return;
    }
    // Whatever went last, the rule-level body's last paragraph is it.
    if let Some(p) = cur.paragraphs.last_mut() {
        extend(p.text_mut(), t);
    }
    match last {
        Last::None | Last::RuleLine | Last::RuleExample => {}
        Last::LeafLine => {
            if let Some(leaf) = cur.leaves.last_mut() {
                extend(&mut leaf.line, t);
            }
        }
        Last::LeafExample => {
            if let Some(e) = cur.leaves.last_mut().and_then(|l| l.examples.last_mut()) {
                extend(e, t);
            }
        }
    }
}

fn rule_id(s: &str) -> anyhow::Result<RuleId> {
    RuleId::try_new(s.to_owned()).map_err(|e| anyhow::anyhow!("bad rule id {s:?}: {e}"))
}

/// First run of exactly eight ASCII digits in the (percent-decoded) last path segment of `source`.
#[must_use]
fn version_from_source(source: &str) -> Option<String> {
    let last = percent_decode(source.rsplit(['/', '\\']).next().unwrap_or(source));
    let mut run = String::new();
    for c in last.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            if run.len() == 8 {
                return Some(run);
            }
            run.clear();
        }
    }
    None
}

/// `These rules are effective as of August 19, 2026.` -> `20260819`.
#[must_use]
fn version_from_effective_line(line: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let rest = line.split("as of").nth(1)?;
    let words: Vec<&str> = rest
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let (mi, month) = words.iter().enumerate().find_map(|(i, w)| {
        MONTHS
            .iter()
            .position(|m| m.eq_ignore_ascii_case(w))
            .map(|p| (i, p + 1))
    })?;
    let day: u32 = words.get(mi + 1)?.parse().ok()?;
    let year: u32 = words.get(mi + 2)?.parse().ok()?;
    ((1..=31).contains(&day) && (1993..=9999).contains(&year))
        .then(|| format!("{year:04}{month:02}{day:02}"))
}

/// Upsert rules and glossary in one transaction; rows from other CR versions are removed
/// afterwards (rules that no longer exist). Embeddings are reset when the text changed.
///
/// Before the upsert, the rules currently stored are compared with the new
/// release ([`renumber_map`]) and every call is rewritten to the new ids in the
/// same transaction, so a renumbered rule keeps the calls that cite it.
/// Retired calls too: a call retired for an unrelated reason (a withdrawn
/// ruling) must still follow the numbering, or it could never be restored.
///
/// The section titles are replaced with the release's, and the `categories`
/// table is written from them ([`sync_categories`]).
async fn store(pool: &PgPool, parsed: &ParsedCr) -> anyhow::Result<Taxonomy> {
    anyhow::ensure!(
        !parsed.sections.is_empty(),
        "no section titles parsed from the CR; the taxonomy's titles could not be looked up"
    );
    let version = parsed.cr_version.as_ref().to_owned();
    let mut tx = pool.begin().await?;
    // Serializes with the retirement pass, which rewrites the same rows.
    sqlx::query!(
        "SELECT pg_advisory_xact_lock($1)",
        crate::db::CALLS_REWRITE_LOCK
    )
    .execute(&mut *tx)
    .await
    .context("locking calls for renumbering")?;

    let renumbered = renumber_map_from_db(&mut tx, parsed).await?;

    for batch in parsed.rules.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO rules (id, parent_id, subsection, heading, body, examples, cr_version) ",
        );
        qb.push_values(batch, |mut b, r| {
            b.push_bind(r.id.as_ref().to_owned())
                .push_bind(r.parent_id.as_ref().map(|p| p.as_ref().to_owned()))
                .push_bind(r.subsection.as_ref().to_owned())
                .push_bind(r.heading.clone())
                .push_bind(r.body.clone())
                .push_bind(r.examples.clone())
                .push_bind(version.clone());
        });
        qb.push(
            " ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id, subsection = EXCLUDED.subsection, \
             heading = EXCLUDED.heading, body = EXCLUDED.body, examples = EXCLUDED.examples, cr_version = EXCLUDED.cr_version, \
             embedding = CASE WHEN rules.heading IS DISTINCT FROM EXCLUDED.heading OR rules.body IS DISTINCT FROM EXCLUDED.body \
             OR rules.examples IS DISTINCT FROM EXCLUDED.examples THEN NULL ELSE rules.embedding END",
        );
        qb.build()
            .execute(&mut *tx)
            .await
            .context("upserting rules")?;
    }
    let stale_rules = sqlx::query!("DELETE FROM rules WHERE cr_version <> $1", version)
        .execute(&mut *tx)
        .await
        .context("deleting stale rules")?
        .rows_affected();

    for batch in parsed.glossary.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO glossary (term, text, cr_version) ");
        qb.push_values(batch, |mut b, g| {
            b.push_bind(g.term.clone())
                .push_bind(g.text.clone())
                .push_bind(version.clone());
        });
        qb.push(
            " ON CONFLICT (term) DO UPDATE SET text = EXCLUDED.text, cr_version = EXCLUDED.cr_version, \
             embedding = CASE WHEN glossary.text IS DISTINCT FROM EXCLUDED.text THEN NULL ELSE glossary.embedding END",
        );
        qb.build()
            .execute(&mut *tx)
            .await
            .context("upserting glossary")?;
    }
    let stale_glossary = sqlx::query!("DELETE FROM glossary WHERE cr_version <> $1", version)
        .execute(&mut *tx)
        .await
        .context("deleting stale glossary")?
        .rows_affected();

    let previous_titles = replace_sections(&mut tx, parsed).await?;
    let taxonomy = sync_categories(&mut tx, &previous_titles).await?;
    let relocated_calls = relocate_calls(&mut tx, &renumbered).await?;

    tx.commit().await?;
    taxonomy.log();
    tracing::info!(
        rules = parsed.rules.len(),
        glossary = parsed.glossary.len(),
        stale_rules,
        stale_glossary,
        renumbered = renumbered.len(),
        relocated_calls,
        "stored comprehensive rules"
    );
    Ok(taxonomy)
}

/// Replace `rule_sections` with the release's sections. Returns the titles the
/// table held before, so the caller can tell which sections the release added
/// (empty when it held none).
async fn replace_sections(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    parsed: &ParsedCr,
) -> anyhow::Result<Vec<String>> {
    let previous = sqlx::query_scalar!("SELECT title FROM rule_sections")
        .fetch_all(&mut **tx)
        .await
        .context("reading the stored section titles")?;
    sqlx::query!("DELETE FROM rule_sections")
        .execute(&mut **tx)
        .await
        .context("clearing the section titles")?;
    let version = parsed.cr_version.as_ref();
    for batch in parsed.sections.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO rule_sections (id, title, part, cr_version) ");
        qb.push_values(batch, |mut b, s| {
            b.push_bind(s.id.clone())
                .push_bind(s.title.clone())
                .push_bind(s.part.clone())
                .push_bind(version);
        });
        qb.build()
            .execute(&mut **tx)
            .await
            .context("storing the section titles")?;
    }
    Ok(previous)
}

/// Old id → new id for rules the new release renumbered, from the rows
/// currently stored. Empty when the stored release *is* this release (a
/// re-parse), when nothing is stored yet, or when nothing moved.
async fn renumber_map_from_db(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    parsed: &ParsedCr,
) -> anyhow::Result<BTreeMap<RuleId, RuleId>> {
    let rows = sqlx::query!("SELECT id, parent_id, body, examples, cr_version FROM rules")
        .fetch_all(&mut **tx)
        .await
        .context("reading stored rules for renumbering")?;
    if rows
        .iter()
        .any(|r| r.cr_version == parsed.cr_version.as_ref())
    {
        return Ok(BTreeMap::new());
    }
    let old: Vec<StoredRule> = rows
        .into_iter()
        .map(|r| {
            Ok(StoredRule {
                id: rule_id(&r.id)?,
                parent_id: r.parent_id.as_deref().map(rule_id).transpose()?,
                body: r.body,
                examples: r.examples,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let map = renumber_map(&old, &parsed.rules);
    for (from, to) in &map {
        tracing::info!(%from, %to, "rule renumbered");
    }
    Ok(map)
}

/// Rewrite every call to `map`; returns how many changed.
async fn relocate_calls(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    map: &BTreeMap<RuleId, RuleId>,
) -> anyhow::Result<u64> {
    if map.is_empty() {
        return Ok(0);
    }
    let calls = sqlx::query!("SELECT id, citations, answer FROM calls")
        .fetch_all(&mut **tx)
        .await
        .context("reading calls for relocation")?;
    let mut changed = 0u64;
    for c in calls {
        if let Some((citations, answer)) = rewrite_call(&c.citations, &c.answer, map) {
            sqlx::query!(
                "UPDATE calls SET citations = $2, answer = $3 WHERE id = $1",
                c.id,
                citations,
                answer
            )
            .execute(&mut **tx)
            .await
            .context("relocating call")?;
            tracing::info!(call = %c.id, "call relocated to renumbered rules");
            changed += 1;
        }
    }
    Ok(changed)
}

/// The titles of the CR now in `rules` and `rule_sections`.
async fn stored_outline(tx: &mut sqlx::Transaction<'_, Postgres>) -> anyhow::Result<Outline> {
    let sections = sqlx::query!("SELECT id, title, part FROM rule_sections ORDER BY id")
        .fetch_all(&mut **tx)
        .await
        .context("reading the section titles")?
        .into_iter()
        .map(|r| Section {
            id: r.id,
            title: r.title,
            part: r.part,
        })
        .collect();
    let rules =
        sqlx::query!(r#"SELECT id, heading, parent_id IS NOT NULL AS "sub_rule!" FROM rules"#)
            .fetch_all(&mut **tx)
            .await
            .context("reading the rule titles")?
            .into_iter()
            .map(|r| (r.id, r.heading, r.sub_rule));
    Ok(Outline::new(sections, rules))
}

/// Write the `categories` table from `judge_core::Category` (generated from
/// `data/categories.yaml`): each category's titles are looked up in the CR now
/// stored ([`Outline::resolve_taxonomy`]) and the rule numbers they name are
/// what the retriever's category map reads. Stale rows are removed.
///
/// An entry that names nothing is left out of its category, whose other
/// entries still resolve, and is kept in `categories.unresolved`, so the next
/// write can tell an entry that has just stopped resolving
/// ([`Taxonomy::stopped`]) from one already reported.
///
/// `previous_titles` are the section titles of the release this one replaced
/// (none when no release was replaced): a section no category names whose title
/// is not among them is one the release added ([`Taxonomy::added`]).
///
/// # Errors
/// On a database failure, or when no section titles are stored: every entry
/// would then name nothing, and the table keeps what it had.
async fn sync_categories(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    previous_titles: &[String],
) -> anyhow::Result<Taxonomy> {
    let outline = stored_outline(tx).await?;
    anyhow::ensure!(
        !outline.is_empty(),
        "no section titles are stored, so the taxonomy's titles cannot be looked up; the \
         categories table is left as it was"
    );
    let resolution = outline.resolve_taxonomy();
    let reported: std::collections::BTreeSet<(String, String)> =
        sqlx::query!("SELECT id, unresolved FROM categories")
            .fetch_all(&mut **tx)
            .await
            .context("reading the entries already reported")?
            .into_iter()
            .flat_map(|r| {
                let id = r.id;
                r.unresolved.into_iter().map(move |u| (id.clone(), u))
            })
            .collect();

    let ids: Vec<&str> = Category::ALL.iter().map(|c| c.id()).collect();
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("INSERT INTO categories (id, label, subsections, unresolved) ");
    qb.push_values(&resolution.categories, |mut b, (c, resolved)| {
        let unresolved: Vec<String> = resolution
            .unresolved
            .iter()
            .filter(|u| u.category == c.id())
            .map(|u| u.reference.clone())
            .collect();
        b.push_bind(c.id())
            .push_bind(c.label())
            .push_bind(resolved.clone())
            .push_bind(unresolved);
    });
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET label = EXCLUDED.label, \
         subsections = EXCLUDED.subsections, unresolved = EXCLUDED.unresolved",
    );
    qb.build()
        .execute(&mut **tx)
        .await
        .context("upserting categories")?;
    let stale = sqlx::query!(
        "DELETE FROM categories WHERE id <> ALL($1)",
        &ids as &[&str]
    )
    .execute(&mut **tx)
    .await
    .context("deleting stale categories")?
    .rows_affected();
    tracing::info!(
        categories = ids.len(),
        stale,
        unresolved = resolution.unresolved.len(),
        "synced categories"
    );

    let stopped = resolution
        .unresolved
        .iter()
        .filter(|u| !reported.contains(&(u.category.clone(), u.reference.clone())))
        .cloned()
        .collect();
    let added = if previous_titles.is_empty() {
        Vec::new()
    } else {
        resolution
            .uncovered
            .iter()
            .filter(|s| {
                !previous_titles
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(&s.title))
            })
            .cloned()
            .collect()
    };
    Ok(Taxonomy {
        unresolved: resolution.unresolved,
        stopped,
        uncovered: resolution.uncovered,
        added,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../../tests/fixtures/cr_sample.txt");
    const SOURCE: &str = "https://media.wizards.com/2026/downloads/MagicCompRules%2020260819.txt";

    type R = anyhow::Result<()>;

    fn parsed() -> anyhow::Result<ParsedCr> {
        parse(SAMPLE, SOURCE)
    }

    fn find<'a>(p: &'a ParsedCr, id: &str) -> anyhow::Result<&'a RuleChunk> {
        p.rules
            .iter()
            .find(|r| r.id.as_ref() == id)
            .ok_or_else(|| anyhow::anyhow!("rule {id} not found"))
    }

    #[test]
    fn find_cr_txt_url_picks_the_txt_with_the_highest_version() {
        let html = r#"<a href="https://media.wizards.com/2026/downloads/MagicCompRules 20260807.pdf">PDF</a>
            <a href="https://media.wizards.com/2026/downloads/MagicCompRules 20260807.docx">Word</a>
            <a href="https://media.wizards.com/2025/downloads/MagicCompRules 20250801.txt">old</a>
            <a class="x" href='https://media.wizards.com/2026/downloads/MagicCompRules 20260819.txt'>TXT</a>
            <a href="https://media.wizards.com/other.txt">unrelated</a>"#;
        assert_eq!(
            find_cr_txt_url(html).as_deref(),
            Some("https://media.wizards.com/2026/downloads/MagicCompRules%2020260819.txt")
        );
    }

    #[test]
    fn find_cr_txt_url_needs_a_txt_link() {
        assert_eq!(
            find_cr_txt_url("<a href=\"https://x/MagicCompRules 20260819.pdf\">"),
            None
        );
        assert_eq!(find_cr_txt_url(""), None);
        // A versionless link still counts; run_latest then loads it unconditionally.
        assert_eq!(
            find_cr_txt_url("<a href=\"/MagicCompRules.txt\">"),
            Some("/MagicCompRules.txt".to_owned())
        );
    }

    #[test]
    fn fixture_is_crlf_and_has_unicode() {
        assert!(
            SAMPLE.contains("\r\n"),
            "fixture should keep Windows line endings"
        );
        assert!(
            SAMPLE.contains('\u{2019}') || SAMPLE.contains('\u{201c}'),
            "fixture should contain Unicode quotes"
        );
    }

    #[test]
    fn toc_lines_produce_no_rows() -> R {
        let p = parsed()?;
        // The TOC lists every section; only 100, 613, 702 and 707 have bodies in the sample.
        let subsections: std::collections::BTreeSet<&str> =
            p.rules.iter().map(|r| r.subsection.as_ref()).collect();
        assert_eq!(
            subsections.into_iter().collect::<Vec<_>>(),
            vec!["100", "613", "702", "707"]
        );
        assert!(
            p.rules.iter().all(|r| r.id.as_ref().contains('.')),
            "no section-only rows"
        );
        assert!(!p.rules.iter().any(|r| r.body.is_empty()));
        Ok(())
    }

    #[test]
    fn sections_with_rules_keep_their_title_and_part() -> R {
        let p = parsed()?;
        let got: Vec<(&str, &str, &str)> = p
            .sections
            .iter()
            .map(|s| (s.id.as_str(), s.title.as_str(), s.part.as_str()))
            .collect();
        // The table of contents lists every section and part; only those a
        // rule was read under are kept, each under the part it sits in.
        assert_eq!(
            got,
            vec![
                ("100", "General", "Game Concepts"),
                (
                    "613",
                    "Interaction of Continuous Effects",
                    "Spells, Abilities, and Effects"
                ),
                ("702", "Keyword Abilities", "Additional Rules"),
                ("707", "Copying Objects", "Additional Rules"),
            ]
        );
        // A glossary definition numbered `1. …` is not a part.
        assert!(
            p.glossary
                .iter()
                .any(|g| g.text.lines().any(|l| l.starts_with("1. "))),
            "the fixture has a numbered glossary definition"
        );
        Ok(())
    }

    #[test]
    fn rule_613_1_folds_sub_rules_and_has_leaves() -> R {
        let p = parsed()?;
        let r = find(&p, "613.1")?;
        assert!(r.parent_id.is_none());
        assert_eq!(r.subsection.as_ref(), "613");
        assert!(r.body.starts_with("613.1. The values of an object"));
        for letter in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            let leaf_id = format!("613.1{letter}");
            assert!(
                r.body.contains(&format!("\n{leaf_id} Layer")),
                "body lacks {leaf_id}"
            );
            let leaf = find(&p, &leaf_id)?;
            assert_eq!(leaf.parent_id.as_ref().map(AsRef::as_ref), Some("613.1"));
            assert_eq!(leaf.subsection.as_ref(), "613");
            assert!(leaf.body.starts_with(&format!("{leaf_id} Layer")));
            assert!(!leaf.body.contains('\n'));
            assert_eq!(leaf.heading, r.heading);
        }
        assert!(!r.body.contains("613.2"));
        // The first sentence is prose (> MAX_HEADING_CHARS), so the section title is the heading.
        assert_eq!(r.heading, "Interaction of Continuous Effects");
        let n613 = p
            .rules
            .iter()
            .filter(|x| x.parent_id.is_none() && x.subsection.as_ref() == "613")
            .count();
        assert!(n613 >= 8, "{n613}");
        Ok(())
    }

    #[test]
    fn examples_attach_to_the_right_rule() -> R {
        let p = parsed()?;
        let r = find(&p, "613.4")?;
        let leaf = find(&p, "613.4d")?;
        assert!(!leaf.examples.is_empty());
        assert!(
            leaf.examples
                .iter()
                .all(|e| e.starts_with("A 1/3 creature"))
        );
        // The rule-level row lists none: its examples are in the body, each
        // directly after the sub-rule it belongs to.
        assert!(r.examples.is_empty());
        let lines: Vec<&str> = r.body.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("613.4d "))
            .ok_or_else(|| anyhow::anyhow!("613.4d line"))?;
        assert!(
            lines
                .get(at + 1)
                .is_some_and(|l| l.starts_with("Example: A 1/3 creature"))
        );
        // 613.4's examples belong to it alone; 613.3 and 613.5 don't get them.
        assert!(!find(&p, "613.3")?.body.contains("Example: A 1/3 creature"));
        assert!(
            find(&p, "613.5")?
                .body
                .contains("Example: Honor of the Pure")
        );
        assert!(!r.body.contains("Honor of the Pure"));
        // 707.2 has examples directly on the rule, before any leaf.
        let r707 = find(&p, "707.2")?;
        let staff = r707
            .body
            .find("Example: Chimeric Staff")
            .ok_or_else(|| anyhow::anyhow!("707.2's example"))?;
        assert!(
            staff
                < r707
                    .body
                    .find("707.2a ")
                    .ok_or_else(|| anyhow::anyhow!("707.2a line"))?
        );
        assert!(find(&p, "707.2a")?.examples.is_empty());
        assert!(r707.contains_quote("Chimeric Staff"));
        Ok(())
    }

    /// 903.3's example follows the rule's own line; printed after the last
    /// sub-rule it read as part of 903.3e, and was cited as 903.3e.
    #[test]
    fn an_example_stays_under_the_line_it_follows() -> R {
        let text = "These rules are effective as of May 1, 2026.\n\n903. Commander\n\n903.3. Each deck has a commander.\nExample: A face-down commander\n     is still a commander.\n\n903.3a Some cards can be your commander.\n\n903.3b A melded commander is a commander.\nExample: Melded.\n\n903.3c A merged commander is a commander.\n\nGlossary\n\nTerm\nDef.\n\nCredits\n";
        let p = parse(text, "x.txt")?;
        let r = find(&p, "903.3")?;
        assert_eq!(
            r.body,
            "903.3. Each deck has a commander.\n\
             Example: A face-down commander is still a commander.\n\
             903.3a Some cards can be your commander.\n\
             903.3b A melded commander is a commander.\n\
             Example: Melded.\n\
             903.3c A merged commander is a commander."
        );
        assert!(r.examples.is_empty());
        assert!(find(&p, "903.3a")?.examples.is_empty());
        assert_eq!(find(&p, "903.3b")?.examples, vec!["Melded.".to_owned()]);
        assert!(find(&p, "903.3c")?.examples.is_empty());
        // Shown the same way wherever it is shown.
        assert_eq!(r.text(), r.body);
        Ok(())
    }

    #[test]
    fn heading_for_702_19_is_trample() -> R {
        let p = parsed()?;
        let r = find(&p, "702.19")?;
        assert_eq!(r.heading, "Trample");
        assert_eq!(r.subsection.as_ref(), "702");
        assert!(
            r.body
                .starts_with("702.19. Trample\n702.19a Trample is a static ability")
        );
        let leaves: Vec<&str> = p
            .rules
            .iter()
            .filter(|x| {
                x.parent_id
                    .as_ref()
                    .is_some_and(|pid| pid.as_ref() == "702.19")
            })
            .map(|x| x.id.as_ref())
            .collect();
        assert_eq!(
            leaves,
            [
                "702.19a", "702.19b", "702.19c", "702.19d", "702.19e", "702.19f", "702.19g"
            ]
        );
        assert_eq!(find(&p, "702.19b")?.heading, "Trample");
        Ok(())
    }

    #[test]
    fn glossary_parses() -> R {
        let p = parsed()?;
        assert!(p.glossary.len() >= 5, "{}", p.glossary.len());
        let terms: Vec<&str> = p.glossary.iter().map(|g| g.term.as_str()).collect();
        assert!(terms.contains(&"Abandon"));
        assert!(terms.contains(&"Zone-Change Triggers"));
        assert!(!terms.contains(&"Credits"));
        let ability = p.glossary.iter().find(|g| g.term == "Ability");
        assert!(ability.is_some_and(
            |g| g.text.lines().count() == 3 && g.text.starts_with("1. Text on an object")
        ));
        assert!(p.glossary.iter().all(|g| !g.text.trim().is_empty()));
        Ok(())
    }

    #[test]
    fn cr_version_extraction() -> R {
        assert_eq!(parsed()?.cr_version.as_ref(), "20260819");
        assert_eq!(version_from_source(SOURCE).as_deref(), Some("20260819"));
        assert_eq!(
            version_from_source("/x/MagicCompRules 20250801.txt").as_deref(),
            Some("20250801")
        );
        assert_eq!(version_from_source("/2026/rules.txt"), None);
        assert_eq!(
            version_from_effective_line("These rules are effective as of August 7, 2026.")
                .as_deref(),
            Some("20260807")
        );
        assert_eq!(
            version_from_effective_line("These rules are effective as of Foo 7, 2026."),
            None
        );
        // Fallback to the effective-as-of line when the name carries no date.
        let p = parse(SAMPLE, "rules.txt")?;
        // Cut anywhere before the Credits, the same text is refused.
        let cut = SAMPLE.find("Glossary\n").unwrap_or(SAMPLE.len() / 2) + 200;
        let short = SAMPLE.get(..cut).unwrap_or_default();
        assert!(
            parse(short, "rules.txt")
                .err()
                .is_some_and(|e| e.to_string().contains("ends before its Credits")),
            "a truncated CR is refused"
        );
        assert_eq!(p.cr_version.as_ref(), "20260807");
        Ok(())
    }

    #[test]
    fn continuation_lines_and_lf_input() -> R {
        let text = "These rules are effective as of May 1, 2026.\n\n100. General\n\n100.1. First. More.\n\n100.1a Sub\n     continued here\nExample: ex one\n     ex continued\n\nGlossary\n\nTerm\nDef.\n\nCredits\n";
        let p = parse(text, "x.txt")?;
        assert_eq!(p.cr_version.as_ref(), "20260501");
        let leaf = find(&p, "100.1a")?;
        assert_eq!(leaf.body, "100.1a Sub continued here");
        assert_eq!(leaf.examples, vec!["ex one ex continued".to_owned()]);
        let r = find(&p, "100.1")?;
        assert_eq!(r.heading, "First");
        assert_eq!(
            r.body,
            "100.1. First. More.\n100.1a Sub continued here\nExample: ex one ex continued"
        );
        assert!(r.examples.is_empty());
        assert_eq!(
            p.glossary,
            vec![GlossaryEntry {
                term: "Term".into(),
                text: "Def.".into()
            }]
        );
        Ok(())
    }

    #[test]
    fn rule_line_forms_seen_in_the_live_cr() -> R {
        let text = "These rules are effective as of May 1, 2026.\n\n119. Life\n\n119.1. Each player begins the game with a starting life total of 20 unless a variant says otherwise.\n119.1c Text c.\n119.1d. In a two-player Brawl game, each player’s starting life total is 25.\n\n606. Loyalty Abilities\n\n606.4. Text four.\n606.5 If the total cost to activate a loyalty ability contains multiple costs, they are combined.\nExample: Combined.\n606.6. Text six.\n\n704. State-Based Actions\n\n704.5. The state-based actions are as follows:\n704.5z If a permanent has more than one Role.\n704.5aa If a player controls a permanent with start your engines! and that player has no speed, that player’s speed becomes 1.\n\nGlossary\n\nTerm\nDef.\n\nCredits\n";
        let p = parse(text, "x.txt")?;
        let d = find(&p, "119.1d")?;
        assert_eq!(d.parent_id.as_ref().map(AsRef::as_ref), Some("119.1"));
        assert!(d.body.starts_with("119.1d In a two-player Brawl game"));
        let r = find(&p, "606.5")?;
        assert!(r.parent_id.is_none());
        assert!(r.body.starts_with("606.5. If the total cost"));
        assert!(r.body.ends_with("combined.\nExample: Combined."));
        assert!(r.examples.is_empty());
        assert!(find(&p, "606.6")?.body.starts_with("606.6. Text six."));
        let aa = find(&p, "704.5aa")?;
        assert_eq!(aa.parent_id.as_ref().map(AsRef::as_ref), Some("704.5"));
        assert!(aa.body.starts_with("704.5aa If a player controls"));
        assert!(find(&p, "704.5")?.body.contains("\n704.5aa If a player"));
        // Headings: short first sentences stay; long ones fall back to the section title.
        assert_eq!(find(&p, "606.4")?.heading, "Text four");
        assert_eq!(find(&p, "119.1")?.heading, "Life");
        Ok(())
    }

    #[test]
    fn cache_file_name_decodes() {
        assert_eq!(cache_file_name(SOURCE), "MagicCompRules 20260819.txt");
        assert_eq!(cache_file_name("https://x/a%2Fb.txt?x=1"), "a_b.txt");
    }

    /// End to end through Postgres: a release that inserts a rule and shifts the
    /// next one (and its lettered sub-rules) along moves every call citing it —
    /// citation id, quote and answer text together, retired calls included. The
    /// retirement pass run afterwards keeps the relocated calls live.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn renumbering_relocates_live_calls(pool: sqlx::PgPool) -> anyhow::Result<()> {
        use crate::ingest::renumber::rewrite_ids;
        use std::collections::BTreeMap;

        let old = parsed()?;
        store(&pool, &old).await?;
        let moved = old
            .rules
            .iter()
            .find(|r| r.id.as_ref() == "702.19b")
            .ok_or_else(|| anyhow::anyhow!("fixture lacks 702.19b"))?;
        let quote = moved.body.lines().next().unwrap_or_default().to_owned();
        anyhow::ensure!(
            quote.starts_with("702.19b"),
            "leaf bodies start with their id: {quote:?}"
        );
        let cite = |id: &str, q: &str| serde_json::json!([{"kind": "rule", "id": id, "quote": q}]);
        let insert = |retired: bool| {
            sqlx::query_scalar::<_, uuid::Uuid>(
                "INSERT INTO calls (thread_id, interface, question, answer, category, source, cr_version, citations, retired_at, retired_reason) \
                 VALUES ('t', 'discord', 'q', 'See 702.19 and 702.19b. Unrelated 613.1 and 613.1a stay.', 'layers', 'cr', '20260819', $1, \
                         CASE WHEN $2 THEN now() END, CASE WHEN $2 THEN 'test' END) RETURNING id",
            )
            .bind(cite("702.19b", &quote))
            .bind(retired)
            .fetch_one(&pool)
        };
        let live = insert(false).await?;
        let retired = insert(true).await?;

        // The next release: a new 702.19, and the old 702.19 (with its leaves) becomes 702.20.
        let mut map: BTreeMap<RuleId, RuleId> = BTreeMap::new();
        for r in &old.rules {
            if let Some(rest) = r.id.as_ref().strip_prefix("702.19") {
                map.insert(r.id.clone(), rule_id(&format!("702.20{rest}"))?);
            }
        }
        let version =
            CrVersion::try_new("20260919".to_owned()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut new = ParsedCr {
            cr_version: version.clone(),
            rules: Vec::new(),
            glossary: old.glossary.clone(),
            sections: old.sections.clone(),
        };
        for r in &old.rules {
            let mut r = r.clone();
            r.id = map.get(&r.id).cloned().unwrap_or(r.id);
            r.parent_id = r.parent_id.map(|p| map.get(&p).cloned().unwrap_or(p));
            r.body = rewrite_ids(&r.body, &map).unwrap_or(r.body);
            r.cr_version = version.clone();
            new.rules.push(r);
        }
        new.rules.push(RuleChunk {
            id: rule_id("702.19")?,
            parent_id: None,
            subsection: rule_id("702")?,
            heading: "Brand New".into(),
            body: "702.19. A keyword that did not exist before.".into(),
            examples: vec![],
            cr_version: version.clone(),
        });
        store(&pool, &new).await?;

        let row = |id: uuid::Uuid| {
            sqlx::query_as::<_, (serde_json::Value, String)>(
                "SELECT citations, answer FROM calls WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
        };
        let (c, a) = row(live).await?;
        let moved_quote = quote.replacen("702.19b", "702.20b", 1);
        assert_eq!(
            c,
            cite("702.20b", &moved_quote),
            "live call follows the renumbering"
        );
        assert_eq!(
            a,
            "See 702.20 and 702.20b. Unrelated 613.1 and 613.1a stay."
        );
        let (c, a) = row(retired).await?;
        assert_eq!(
            c,
            cite("702.20b", &moved_quote),
            "a retired call follows the numbering too"
        );
        assert_eq!(
            a,
            "See 702.20 and 702.20b. Unrelated 613.1 and 613.1a stay."
        );

        // Both citations validate against the new rows: the live call stays live
        // and the retired one (retired for a reason that no longer holds) comes back.
        let s = crate::db::retire_unsupported(&pool).await?;
        assert_eq!(
            (s.checked, s.retired, s.restored, s.still_retired),
            (2, 0, 1, 0),
            "{s:?}"
        );

        // A re-parse of the same release computes no map and rewrites nothing.
        store(&pool, &new).await?;
        assert_eq!(row(live).await?.0, cite("702.20b", &moved_quote));
        Ok(())
    }

    /// `categories.subsections` and `.unresolved` for `id`.
    async fn category(pool: &PgPool, id: &str) -> anyhow::Result<(Vec<String>, Vec<String>)> {
        Ok(sqlx::query_as::<_, (Vec<String>, Vec<String>)>(
            "SELECT subsections, unresolved FROM categories WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await?)
    }

    /// `parsed` as a later release: every id of section `from` moved to `to`
    /// (cross-references left alone; these tests read the taxonomy, not calls).
    fn renumbered(old: &ParsedCr, version: &str, from: &str, to: &str) -> anyhow::Result<ParsedCr> {
        let version = CrVersion::try_new(version.to_owned()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let moved = |id: &str| match id.strip_prefix(from) {
            Some(rest) => format!("{to}{rest}"),
            None => id.to_owned(),
        };
        let mut new = old.clone();
        new.cr_version = version.clone();
        for r in &mut new.rules {
            r.id = rule_id(&moved(r.id.as_ref()))?;
            r.parent_id = r
                .parent_id
                .as_ref()
                .map(|p| rule_id(&moved(p.as_ref())))
                .transpose()?;
            r.subsection = rule_id(&moved(r.subsection.as_ref()))?;
            r.body = r.body.replace(&format!("{from}."), &format!("{to}."));
            r.cr_version = version.clone();
        }
        for s in &mut new.sections {
            s.id = moved(&s.id);
        }
        Ok(new)
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_taxonomy_follows_titles_and_reports_once(pool: PgPool) -> R {
        let old = parsed()?;
        let first = store(&pool, &old).await?;

        // The fixture has four sections: an entry naming one of them resolves
        // to its number, and the rest of a category's entries are reported
        // while the category keeps what resolved.
        assert_eq!(
            category(&pool, "layers").await?,
            (vec!["613".to_owned()], vec![])
        );
        assert_eq!(
            category(&pool, "copying").await?,
            (vec!["707".to_owned()], vec!["Tokens".to_owned()])
        );
        // The fixture's 702 has Trample alone: the section is there and the
        // rules named inside it are not.
        assert_eq!(
            category(&pool, "keyword_abilities").await?,
            (
                vec![],
                vec![
                    "Keyword Abilities > .1".to_owned(),
                    "Keyword Abilities > Deathtouch".to_owned()
                ]
            )
        );
        let (subsections, unresolved) = category(&pool, "format_legality").await?;
        assert_eq!(subsections, vec!["100"]);
        assert_eq!(unresolved, vec!["Commander > .5"]);

        // Everything unresolved is news the first time, and no section is
        // "added": nothing was stored before.
        assert!(!first.unresolved.is_empty());
        assert_eq!(first.stopped, first.unresolved);
        assert!(first.added.is_empty(), "{:?}", first.added);
        let uncovered = |t: &Taxonomy| {
            t.uncovered
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(uncovered(&first), vec!["702 Keyword Abilities"]);

        // The same release again: the same entries are unresolved, none newly.
        let again = store(&pool, &old).await?;
        assert_eq!(again.unresolved, first.unresolved);
        assert!(again.stopped.is_empty(), "{:?}", again.stopped);

        // The next release inserts a section at 613, moving the layers to 614,
        // and retitles Copying Objects.
        let mut new = renumbered(&old, "20260919", "613", "614")?;
        new.sections.push(Section {
            id: "613".into(),
            title: "Brand New Cards".into(),
            part: "Spells, Abilities, and Effects".into(),
        });
        new.rules.push(RuleChunk {
            id: rule_id("613.1")?,
            parent_id: None,
            subsection: rule_id("613")?,
            heading: "Brand New Cards".into(),
            body: "613.1. A card layout that did not exist before.".into(),
            examples: vec![],
            cr_version: new.cr_version.clone(),
        });
        for s in &mut new.sections {
            if s.title == "Copying Objects" {
                s.title = "Copies".into();
            }
        }
        let next = store(&pool, &new).await?;

        // The title follows its section; the number it had is another section's.
        assert_eq!(
            category(&pool, "layers").await?,
            (vec!["614".to_owned()], vec![])
        );
        // The retitled section stops resolving: left out, never pointed elsewhere.
        assert_eq!(
            category(&pool, "copying").await?,
            (
                vec![],
                vec!["Copying Objects".to_owned(), "Tokens".to_owned()]
            )
        );
        let stopped: Vec<String> = next.stopped.iter().map(ToString::to_string).collect();
        assert_eq!(
            stopped,
            vec!["copying: Copying Objects (no section has this title)"]
        );
        // The inserted section and the retitled one are sections the release
        // added that no category names.
        let added: Vec<String> = next.added.iter().map(ToString::to_string).collect();
        assert_eq!(added, vec!["613 Brand New Cards", "707 Copies"]);
        assert_eq!(
            uncovered(&next),
            vec!["613 Brand New Cards", "702 Keyword Abilities", "707 Copies"]
        );

        // The record reads back, and one written before this field existed
        // reads as nothing to report.
        let outcome = Outcome::Updated {
            version: "20260919".into(),
            url: "https://example.test/cr.txt".into(),
            taxonomy: next.clone(),
        };
        let json = serde_json::to_value(&outcome)?;
        assert_eq!(serde_json::from_value::<Outcome>(json)?, outcome);
        let older: Outcome =
            serde_json::from_value(serde_json::json!({"cr": "unchanged", "version": "20260819"}))?;
        assert_eq!(older.taxonomy(), &Taxonomy::default());
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_release_with_no_section_titles_changes_nothing(pool: PgPool) -> R {
        let old = parsed()?;
        store(&pool, &old).await?;
        let before = category(&pool, "layers").await?;

        let mut untitled = renumbered(&old, "20260919", "613", "614")?;
        untitled.sections.clear();
        let Err(e) = store(&pool, &untitled).await else {
            anyhow::bail!("a release with no section titles was stored");
        };
        assert!(format!("{e:#}").contains("no section titles"), "{e:#}");
        // Neither the rules nor the categories moved.
        assert_eq!(category(&pool, "layers").await?, before);
        let stored: Option<String> = sqlx::query_scalar("SELECT max(cr_version) FROM rules")
            .fetch_one(&pool)
            .await?;
        assert_eq!(stored.as_deref(), Some("20260819"));

        // A database migrated from a release that kept no titles: the write
        // refuses rather than emptying every category.
        sqlx::query("DELETE FROM rule_sections")
            .execute(&pool)
            .await?;
        let mut tx = pool.begin().await?;
        let Err(e) = sync_categories(&mut tx, &[]).await else {
            anyhow::bail!("the categories were written with no titles to look up");
        };
        assert!(format!("{e:#}").contains("no section titles"), "{e:#}");
        drop(tx);
        assert_eq!(category(&pool, "layers").await?, before);
        Ok(())
    }
}
