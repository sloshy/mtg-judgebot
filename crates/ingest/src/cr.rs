//! Comprehensive Rules parser: txt (local path or URL) -> `rules` chunks + `glossary`.
//!
//! Chunking (ARCHITECTURE.md §2), at *rule* granularity:
//!
//! * One row per rule `X.Y` (e.g. `702.19`): `body` = the rule's own line followed by
//!   every lettered sub-rule (`702.19a`, `702.19b`, …) in order, one per line;
//!   `examples` = every `Example:` paragraph under the rule or any of its sub-rules;
//!   `parent_id = NULL`; `subsection = "702"`; `heading` = the rule's first-line text
//!   up to its first sentence-ending period (so `702.19. Trample` → `Trample`), or the
//!   section title (`Interaction of Continuous Effects`) when that first sentence is
//!   prose longer than [`MAX_HEADING_CHARS`].
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
use judge_core::{Category, CrVersion, GlossaryEntry, RuleChunk, RuleId};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::renumber::{StoredRule, renumber_map, rewrite_call};

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
const USER_AGENT: &str = "mtg-judgebot-ingest/0.1 (+https://github.com/sloshy/mtg-judgebot)";

/// Parse and load the CR from `source` (path or `http(s)://` URL, cached under `cache_dir`).
///
/// # Errors
/// On download, parse or database failure.
pub async fn run(pool: &PgPool, source: &str, cache_dir: &Path) -> anyhow::Result<()> {
    let text = fetch(source, cache_dir).await?;
    let parsed = parse(&text, source)?;
    tracing::info!(
        cr_version = %parsed.cr_version,
        rules = parsed.rules.iter().filter(|r| r.parent_id.is_none()).count(),
        leaves = parsed.rules.iter().filter(|r| r.parent_id.is_some()).count(),
        glossary = parsed.glossary.len(),
        "parsed comprehensive rules"
    );
    if parsed.rules.is_empty() {
        anyhow::bail!("no rules parsed from {source}");
    }
    store(pool, &parsed).await
}

/// What [`run_latest`] found on the rules page and did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The published release is the one already in `rules`; nothing was downloaded.
    Unchanged { version: String },
    /// A different release is published; it was fetched, parsed and stored.
    Updated { version: String, url: String },
}

/// Load the CR release currently linked from [`RULES_PAGE_URL`], unless `rules`
/// already holds that version.
///
/// A link whose file name carries no version is loaded unconditionally: [`store`] is
/// idempotent, so the only cost of being unable to tell is a download.
///
/// # Errors
/// If the rules page is unreachable or links no `MagicCompRules*.txt`, or on any
/// failure of [`run`].
pub async fn run_latest(pool: &PgPool, cache_dir: &Path) -> anyhow::Result<Outcome> {
    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
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
        .fetch_one(pool)
        .await
        .context("reading the stored CR version")?;
    tracing::info!(url, published = ?published, stored = ?current, "current comprehensive rules release");
    match (published, current) {
        (Some(version), Some(stored)) if version == stored => {
            tracing::info!(%version, "comprehensive rules unchanged; nothing to do");
            Ok(Outcome::Unchanged { version })
        }
        (published, _) => {
            run(pool, &url, cache_dir).await?;
            let version = published.unwrap_or_else(|| "unknown".to_owned());
            Ok(Outcome::Updated { version, url })
        }
    }
}

/// The download link for the current CR text on the rules page: the `href` whose
/// file name mentions `MagicCompRules` and ends in `.txt`. When several qualify the
/// highest version wins (ties: first seen). Spaces are percent-encoded so the result
/// is a valid URL; Wizards links `MagicCompRules 20260819.txt` with a literal space.
#[must_use]
pub fn find_cr_txt_url(html: &str) -> Option<String> {
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
    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
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
    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    Ok(text)
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
pub struct ParsedCr {
    /// CR effective date, `YYYYMMDD`.
    pub cr_version: CrVersion,
    /// Rule chunks and leaves, in document order (each rule chunk before its leaves).
    pub rules: Vec<RuleChunk>,
    /// Glossary entries in document order (terms unique).
    pub glossary: Vec<GlossaryEntry>,
}

/// What one line of the CR is.
#[derive(Debug, Clone, Copy)]
enum Line<'a> {
    Blank,
    /// Indented continuation of the previous line.
    Continuation(&'a str),
    Example(&'a str),
    /// `702. Keyword Abilities`
    Section {
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

/// `702. Title` | `702.19. text` | `702.19b text`, ASCII digits only.
fn classify_numbered(line: &str) -> Option<Line<'_>> {
    line.get(..3)
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?;
    let after = line.get(3..)?;
    let after = after.strip_prefix('.')?;
    if let Some(title) = after.strip_prefix(' ') {
        return Some(Line::Section {
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
    lines: Vec<String>,
    examples: Vec<String>,
    leaves: Vec<LeafBuilder>,
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
    section_title: String,
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
            Line::Section { title } => title.clone_into(&mut self.section_title),
            Line::Rule { id, text } => {
                self.mode = Mode::Rules;
                self.start_rule(id, text);
            }
            _ => {}
        }
    }

    fn start_rule(&mut self, id: &str, text: &str) {
        self.rules.extend(self.current.take());
        self.current = Some(new_rule(id, text, &self.section_title));
        self.last = Last::RuleLine;
    }

    fn rule_line(&mut self, line: Line<'_>) {
        match line {
            Line::Blank => {}
            Line::Section { title } => title.clone_into(&mut self.section_title),
            Line::Rule { id, text } => self.start_rule(id, text),
            Line::Leaf { id, parent, text } => {
                if self.current.as_ref().is_none_or(|c| c.id != parent) {
                    // A leaf whose parent rule line is missing: synthesise the parent.
                    self.start_rule(parent, "");
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.lines.push(format!("{id} {text}"));
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
                cur.examples.push(t.to_owned());
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
            Line::Section { .. }
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
pub fn parse(text: &str, source: &str) -> anyhow::Result<ParsedCr> {
    let mut p = Parser::default();
    for raw in text.lines() {
        p.feed(raw);
    }
    p.rules.extend(p.current.take());

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
    for r in p.rules {
        let id = rule_id(&r.id)?;
        let subsection = rule_id(&r.subsection)?;
        rules.push(RuleChunk {
            id: id.clone(),
            parent_id: None,
            subsection: subsection.clone(),
            heading: r.heading.clone(),
            body: r.lines.join("\n"),
            examples: r.examples,
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
        lines: vec![line],
        examples: Vec::new(),
        leaves: Vec::new(),
    }
}

fn append_continuation(cur: &mut RuleBuilder, last: Last, t: &str) {
    fn extend(s: &mut String, t: &str) {
        s.push(' ');
        s.push_str(t);
    }
    match last {
        Last::None => {}
        Last::RuleLine | Last::LeafLine => {
            if let Some(l) = cur.lines.last_mut() {
                extend(l, t);
            }
            if let Some(leaf) = cur.leaves.last_mut().filter(|_| last == Last::LeafLine) {
                extend(&mut leaf.line, t);
            }
        }
        Last::RuleExample | Last::LeafExample => {
            if let Some(e) = cur.examples.last_mut() {
                extend(e, t);
            }
            if let Some(e) = cur
                .leaves
                .last_mut()
                .and_then(|l| l.examples.last_mut())
                .filter(|_| last == Last::LeafExample)
            {
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
pub fn version_from_source(source: &str) -> Option<String> {
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
pub fn version_from_effective_line(line: &str) -> Option<String> {
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
async fn store(pool: &PgPool, parsed: &ParsedCr) -> anyhow::Result<()> {
    let version = parsed.cr_version.as_ref().to_owned();
    let mut tx = pool.begin().await?;
    // Serializes with the retirement pass, which rewrites the same rows.
    sqlx::query!(
        "SELECT pg_advisory_xact_lock($1)",
        judge_bot::db::CALLS_REWRITE_LOCK
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

    sync_categories(&mut tx).await?;
    let relocated_calls = relocate_calls(&mut tx, &renumbered).await?;

    tx.commit().await?;
    tracing::info!(
        rules = parsed.rules.len(),
        glossary = parsed.glossary.len(),
        stale_rules,
        stale_glossary,
        renumbered = renumbered.len(),
        relocated_calls,
        "stored comprehensive rules"
    );
    Ok(())
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

/// Mirror `judge_core::Category` (generated from `data/categories.yaml`) into the
/// `categories` table, so the retriever's category map reads the same
/// subsection lists the code was built with; stale rows are removed.
async fn sync_categories(tx: &mut sqlx::Transaction<'_, Postgres>) -> anyhow::Result<()> {
    let ids: Vec<&str> = Category::ALL.iter().map(|c| c.id()).collect();
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("INSERT INTO categories (id, label, subsections) ");
    qb.push_values(Category::ALL, |mut b, c| {
        let subs: Vec<String> = c.subsections().iter().map(|s| (*s).to_owned()).collect();
        b.push_bind(c.id()).push_bind(c.label()).push_bind(subs);
    });
    qb.push(" ON CONFLICT (id) DO UPDATE SET label = EXCLUDED.label, subsections = EXCLUDED.subsections");
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
    tracing::info!(categories = ids.len(), stale, "synced categories");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/cr_sample.txt");
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
        assert!(leaf.examples.iter().all(|e| r.examples.contains(e)));
        assert!(r.examples.len() >= leaf.examples.len());
        // 613.4's examples belong to it alone; 613.3 and 613.5 don't get them.
        assert!(
            !find(&p, "613.3")?
                .examples
                .iter()
                .any(|e| e.starts_with("A 1/3 creature"))
        );
        let r5 = find(&p, "613.5")?;
        assert!(
            r5.examples
                .iter()
                .any(|e| e.starts_with("Honor of the Pure"))
        );
        assert!(
            !r.examples
                .iter()
                .any(|e| e.starts_with("Honor of the Pure"))
        );
        // 707.2 has examples directly on the rule (before any leaf).
        let r707 = find(&p, "707.2")?;
        assert!(
            r707.examples
                .iter()
                .any(|e| e.starts_with("Chimeric Staff"))
        );
        assert!(find(&p, "707.2a")?.examples.is_empty());
        assert!(r707.contains_quote("Chimeric Staff"));
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
        assert_eq!(r.body, "100.1. First. More.\n100.1a Sub continued here");
        assert_eq!(r.examples, leaf.examples);
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
        assert_eq!(r.examples, vec!["Combined.".to_owned()]);
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
        use crate::renumber::rewrite_ids;
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
                "INSERT INTO calls (thread_id, question, answer, category, source, cr_version, citations, retired_at, retired_reason) \
                 VALUES ('t', 'q', 'See 702.19 and 702.19b. Unrelated 613.1 and 613.1a stay.', 'layers', 'cr', '20260819', $1, \
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
        let s = judge_bot::db::retire_unsupported(&pool).await?;
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
}
