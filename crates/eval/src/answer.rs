//! `eval answer`: run the full `judge()` pipeline over the gold set, score
//! citations and source, account for model spend, and write a run file
//! that `eval show` renders for human review.

use std::{path::PathBuf, time::Instant};

use anyhow::Context as _;
use judge_bot::config::Config;
use judge_core::{Citation, JudgeError, Question, Verdict, judge};
use judge_llm::LlmError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{
    gold::{Gold, GoldQuestion},
    score::{self, CiteCounts, Recall},
};

/// Parsed `answer` flags.
#[derive(Debug)]
pub struct Options {
    /// Gold file.
    pub gold: PathBuf,
    /// Run at most this many questions (after `--ids` filtering).
    pub limit: usize,
    /// Only these gold ids, in gold order.
    pub ids: Vec<String>,
    /// Spend cap for this run.
    pub max_usd: f64,
    /// Where the JSON goes.
    pub out: PathBuf,
    /// Run label (also the default file stem).
    pub label: String,
    /// Replace the LLM extractor with the gold file's cards/categories/source.
    pub gold_extraction: bool,
    /// A `judge.toml` to run with (`--config`), ahead of `JUDGE_CONFIG`.
    pub config: Option<PathBuf>,
}

impl Options {
    /// Parse `answer` flags. `--label` is required unless `--out` is given.
    ///
    /// # Errors
    /// On an unknown flag or a missing value.
    pub fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let (mut gold, mut limit, mut ids, mut max_usd, mut out, mut label) =
            (None, None::<usize>, Vec::<String>::new(), 2.00f64, None::<PathBuf>, None::<String>);
        let mut gold_extraction = false;
        let mut config = None;
        while let Some(a) = args.next() {
            let mut val = || args.next().ok_or_else(|| anyhow::anyhow!("{a} needs a value"));
            match a.as_str() {
                "--gold" => gold = Some(PathBuf::from(val()?)),
                "--config" => config = Some(PathBuf::from(val()?)),
                "--limit" => limit = Some(val()?.parse().context("--limit must be an integer")?),
                "--ids" => ids = val()?.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
                "--max-usd" => max_usd = val()?.parse().context("--max-usd must be a number")?,
                "--out" => out = Some(PathBuf::from(val()?)),
                "--label" => label = Some(val()?),
                "--gold-extraction" => gold_extraction = true,
                other => anyhow::bail!("unknown flag {other}"),
            }
        }
        let label = match (&label, &out) {
            (Some(l), _) => l.clone(),
            (None, Some(o)) => o.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            (None, None) => anyhow::bail!("--label <name> is required (or pass --out <path>)"),
        };
        let out = out.unwrap_or_else(|| PathBuf::from("eval/runs").join(format!("{label}.json")));
        // Explicit ids are all run unless --limit says otherwise; a bare run defaults to two questions.
        let limit = limit.unwrap_or(if ids.is_empty() { 2 } else { ids.len() });
        if limit == 0 {
            anyhow::bail!("--limit must be at least 1");
        }
        if !(max_usd.is_finite() && max_usd >= 0.0) {
            anyhow::bail!("--max-usd must be a finite non-negative number");
        }
        Ok(Self { gold: gold.unwrap_or_else(crate::gold::default_path), limit, ids, max_usd, out, label, gold_extraction, config })
    }
}

/// The bot's output for one question, as recorded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// A validated verdict.
    Verdict {
        /// Answer text.
        answer: String,
        /// Self-reported confidence.
        confidence: String,
        /// Citations as `Display`ed (`rule 702.19b: "..."`).
        citations: Vec<String>,
        /// Category assigned.
        category: String,
        /// Source assigned (`cr`, `commander`, ...).
        source: String,
        /// CR version validated against.
        cr_version: String,
    },
    /// `judge()` failed.
    Error {
        /// Variant name.
        variant: String,
        /// `Display` text.
        message: String,
    },
}

/// One scored question.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    /// Gold id.
    pub id: String,
    /// The question text.
    pub question: String,
    /// Gold reference answer.
    pub expected_answer: String,
    /// Gold source label.
    pub expected_source: String,
    /// Gold rule ids.
    pub expected_rule_ids: Vec<String>,
    /// What the bot did.
    pub outcome: Outcome,
    /// Rule ids the validated citations reference (empty on error).
    pub cited_rule_ids: Vec<String>,
    /// Citation recall against `expected_rule_ids`.
    pub recall: Recall,
    /// Whether at least one `expected_rule_ids` entry was cited (`recall.hit` non-empty).
    #[serde(default)]
    pub any_expected_cited: bool,
    /// Rule and ruling citation counts (older run files lack them: zero).
    #[serde(default, flatten)]
    pub cites: CiteCounts,
    /// Whether the produced source (or out-of-scope error) matches the gold source.
    pub source_ok: bool,
    /// `true` for a verdict, or for an `OutOfScope` error on an unanswerable gold item.
    pub correct_shape: bool,
    /// Wall time in milliseconds.
    pub elapsed_ms: u128,
    /// Anthropic calls this question made.
    pub calls: u64,
    /// Estimated USD this question cost.
    pub usd: f64,
}

/// Which model answered each stage of a run, as `provider/model`, so
/// `show`/`rescore` can compare two providers on the same gold set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunModels {
    /// The extraction model (`gold` when `--gold-extraction` replaced it).
    pub extract: String,
    /// The synthesis model.
    pub synth: String,
}

/// The run file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Run {
    /// `--label`.
    pub label: String,
    /// Gold file path.
    pub gold: String,
    /// Cap the client was given.
    pub max_usd: f64,
    /// Which models ran; absent in run files from before providers were configurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<RunModels>,
    /// Rows in gold order.
    pub rows: Vec<Row>,
    /// Sum of `usd`.
    pub total_usd: f64,
    /// Sum of `calls`.
    pub total_calls: u64,
}

impl Run {
    /// `extract=<provider/model> synth=<provider/model>` for the table and
    /// `show` headers, so two providers' runs compare without opening the
    /// JSON; `models=unknown` for a run file from before that was recorded.
    #[must_use]
    pub fn models_line(&self) -> String {
        self.models.as_ref().map_or_else(|| "models=unknown".to_owned(), |m| format!("extract={} synth={}", m.extract, m.synth))
    }

    /// `(questions with ≥1 expected id cited, questions expecting any id)`.
    #[must_use]
    pub fn any_expected_cited(&self) -> (usize, usize) {
        let expecting = self.rows.iter().filter(|r| r.recall.expects_any()).count();
        let cited = self.rows.iter().filter(|r| r.any_expected_cited).count();
        (cited, expecting)
    }

    /// Aggregate citation recall over answerable questions.
    #[must_use]
    pub fn recall(&self) -> Option<f64> {
        let hit: usize = self.rows.iter().map(|r| r.recall.hit.len()).sum();
        let total: usize = self.rows.iter().map(|r| r.recall.hit.len() + r.recall.missed.len()).sum();
        if total == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let f = hit as f64 / total as f64;
        Some(f)
    }
}

/// Pick the questions to run.
fn select<'a>(gold: &'a Gold, opts: &Options) -> Vec<&'a GoldQuestion> {
    gold.questions
        .iter()
        .filter(|q| opts.ids.is_empty() || opts.ids.contains(&q.id))
        .take(opts.limit)
        .collect()
}

/// Run and score; prints the table and writes `opts.out`.
///
/// # Errors
/// DB / client construction, or writing the run file. Per-question failures are recorded, not raised.
pub async fn run(pool: PgPool, opts: &Options) -> anyhow::Result<Run> {
    let gold = crate::gold::load(&opts.gold)?;
    if let Some(known) = opts.ids.iter().find(|id| !gold.questions.iter().any(|q| &q.id == *id)) {
        anyhow::bail!("--ids: no gold question with id {known:?}");
    }
    let config = Config::load_from(opts.config.as_deref())?;
    tracing::info!("{}", config.summary());
    let models = config.models()?;
    models.meter().set_max_spend_usd(opts.max_usd)?;
    let meter = models.meter().clone();
    let embedder = config.embedder()?;
    if embedder.is_none() {
        tracing::warn!("no embedder; retrieval runs without vector search");
    }
    let run_models = RunModels {
        extract: if opts.gold_extraction { "gold".to_owned() } else { config.extract().map(judge_bot::config::Stage::label).unwrap_or_default() },
        synth: config.synth().map(judge_bot::config::Stage::label).unwrap_or_default(),
    };
    let deps = crate::deps::build(pool, &models, embedder, &config.deps_config(), &gold, opts.gold_extraction);
    let selected = select(&gold, opts);

    let mut rows = Vec::with_capacity(selected.len());
    for q in selected {
        let (usd0, calls0) = (meter.spent_usd(), meter.calls());
        let question = Question { thread_id: q.id.clone(), text: q.question.clone() };
        let started = Instant::now();
        let result = judge(&deps, &question, &[]).await;
        let elapsed_ms = started.elapsed().as_millis();
        let row = score_row(q, &result, elapsed_ms, meter.calls() - calls0, meter.spent_usd() - usd0);
        tracing::info!(id = %row.id, ok = row.correct_shape, usd = format_args!("{:.4}", row.usd), "scored");
        rows.push(row);
        if let Err(JudgeError::Upstream(e)) = &result
            && e.downcast_ref::<LlmError>().is_some_and(|c| matches!(c, LlmError::SpendCapExceeded { .. }))
        {
            tracing::warn!("spend cap reached; stopping the run");
            break;
        }
    }
    let run = Run {
        label: opts.label.clone(),
        gold: opts.gold.display().to_string(),
        max_usd: opts.max_usd,
        models: Some(run_models),
        total_usd: rows.iter().map(|r| r.usd).sum(),
        total_calls: rows.iter().map(|r| r.calls).sum(),
        rows,
    };
    if let Some(dir) = opts.out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&opts.out, serde_json::to_string_pretty(&run)?).with_context(|| format!("writing {}", opts.out.display()))?;
    print!("{}", table(&run));
    println!("wrote {}", opts.out.display());
    Ok(run)
}

fn score_row(
    q: &GoldQuestion,
    result: &Result<Verdict<judge_core::Validated>, JudgeError>,
    elapsed_ms: u128,
    calls: u64,
    usd: f64,
) -> Row {
    let expected_rule_ids: Vec<String> = q.expected_rule_ids.iter().map(crate::gold::YamlScalar::as_text).collect();
    let (outcome, cited, counts, source_ok, correct_shape) = match result {
        Ok(v) => {
            let cited = score::cited_rule_ids(v.citations());
            let counts = score::cite_counts(v.citations());
            let source = serde_json::to_value(v.source()).ok().and_then(|s| s.as_str().map(str::to_owned)).unwrap_or_default();
            let outcome = Outcome::Verdict {
                answer: v.answer().to_owned(),
                confidence: format!("{:?}", v.confidence()).to_ascii_lowercase(),
                citations: v.citations().iter().map(Citation::to_string).collect(),
                category: v.category().to_string(),
                source,
                cr_version: v.cr_version().to_string(),
            };
            (outcome, cited, counts, score::source_matches(&q.source, v.source()), q.is_answerable())
        }
        Err(e) => {
            let variant = match e {
                JudgeError::AmbiguousCards(_) => "AmbiguousCards",
                JudgeError::CardsNotFound(_) => "CardsNotFound",
                JudgeError::OutOfScope(_) => "OutOfScope",
                JudgeError::BadCitation(_) => "BadCitation",
                JudgeError::MalformedCitation(_) => "MalformedCitation",
                JudgeError::EmptyVerdict(_) => "EmptyVerdict",
                JudgeError::LlmRefused => "LlmRefused",
                JudgeError::Upstream(_) => "Upstream",
            };
            let (source_ok, shape) = match e {
                JudgeError::OutOfScope(s) => (score::source_matches(&q.source, *s), !q.is_answerable()),
                _ => (false, false),
            };
            (Outcome::Error { variant: variant.into(), message: format!("{e:#}") }, Vec::new(), CiteCounts::default(), source_ok, shape)
        }
    };
    let recall = if q.is_answerable() { score::recall_with(&cited, &expected_rule_ids, &q.equivalents()) } else { Recall { hit: vec![], missed: vec![] } };
    let any_expected_cited = recall.any_hit();
    Row {
        id: q.id.clone(),
        question: q.question.clone(),
        expected_answer: q.expected_answer.clone(),
        expected_source: q.source.clone(),
        expected_rule_ids,
        outcome,
        cited_rule_ids: cited,
        recall,
        any_expected_cited,
        cites: counts,
        source_ok,
        correct_shape,
        elapsed_ms,
        calls,
        usd,
    }
}

/// Milliseconds → seconds for display.
fn secs(ms: u128) -> f64 {
    // Display only; a run never approaches 2^53 ms.
    #[allow(clippy::cast_precision_loss)]
    let s = ms as f64 / 1000.0;
    s
}

/// Compact per-question table plus totals.
#[must_use]
pub fn table(run: &Run) -> String {
    use std::fmt::Write as _;
    let width = run.rows.iter().map(|r| r.id.len()).max().unwrap_or(8).max(8);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:<width$}  {:<12}  {:>6}  {:<3}  {:>5}  {:>5}  {:>5}  {:<3}  {:<3}  {:>7}  {:>5}  {:>8}",
        "question", "outcome", "recall", "any", "rules", "rlngs", "orcl", "src", "ok", "secs", "calls", "usd"
    );
    for r in &run.rows {
        let outcome = match &r.outcome {
            Outcome::Verdict { confidence, .. } => format!("verdict/{confidence}"),
            Outcome::Error { variant, .. } => variant.clone(),
        };
        let recall = format!("{}/{}", r.recall.hit.len(), r.recall.hit.len() + r.recall.missed.len());
        let _ = writeln!(
            s,
            "{:<width$}  {:<12}  {:>6}  {:<3}  {:>5}  {:>5}  {:>5}  {:<3}  {:<3}  {:>7.1}  {:>5}  {:>8.4}",
            r.id,
            outcome,
            recall,
            if r.any_expected_cited { "yes" } else { "no" },
            r.cites.n_rule_cites,
            r.cites.n_ruling_cites,
            r.cites.n_oracle_cites,
            if r.source_ok { "yes" } else { "no" },
            if r.correct_shape { "yes" } else { "NO" },
            secs(r.elapsed_ms),
            r.calls,
            r.usd
        );
    }
    let ok = run.rows.iter().filter(|r| r.correct_shape).count();
    let src = run.rows.iter().filter(|r| r.source_ok).count();
    let (any, expecting) = run.any_expected_cited();
    let n_rules: usize = run.rows.iter().map(|r| r.cites.n_rule_cites).sum();
    let n_rulings: usize = run.rows.iter().map(|r| r.cites.n_ruling_cites).sum();
    let n_oracle: usize = run.rows.iter().map(|r| r.cites.n_oracle_cites).sum();
    let _ = writeln!(
        s,
        "\n{} questions: {ok} correct shape, {src} source match, citation recall {}, {} calls, TOTAL ${:.4} (cap ${:.2}); {}",
        run.rows.len(),
        run.recall().map_or_else(|| "n/a".to_owned(), |f| format!("{:.1}%", f * 100.0)),
        run.total_calls,
        run.total_usd,
        run.max_usd,
        run.models_line()
    );
    let _ = writeln!(s, "questions with ≥1 expected id cited: {any}/{expecting}; {n_rules} rule citations, {n_rulings} ruling citations, {n_oracle} oracle citations");
    s
}

/// `eval show <run.json>`: question, expected answer and bot answer side by side.
///
/// # Errors
/// If the file cannot be read or parsed.
/// Re-score a stored run against the CURRENT gold file (equivalence lists and
/// edited expectations apply retroactively; nothing is re-asked of the model).
/// Recomputes each row's recall from its stored `cited_rule_ids` and rewrites
/// the derived columns; the run file itself is not modified.
pub fn rescore(path: &std::path::Path, gold_path: &std::path::Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut run: Run = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let gold = crate::gold::load(gold_path)?;
    for row in &mut run.rows {
        let Some(q) = gold.questions.iter().find(|q| q.id == row.id) else {
            tracing::warn!(id = %row.id, "row not in gold file; keeping stored recall");
            continue;
        };
        if q.is_answerable() {
            row.recall = score::recall_with(&row.cited_rule_ids, &q.expected_rule_ids.iter().map(crate::gold::YamlScalar::as_text).collect::<Vec<_>>(), &q.equivalents());
            row.any_expected_cited = row.recall.any_hit();
        }
    }
    Ok(table(&run))
}

pub fn show(path: &std::path::Path) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let run: Run = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let mut s = String::new();
    let _ = writeln!(s, "run {} ({} questions, ${:.4}; {})\n", run.label, run.rows.len(), run.total_usd, run.models_line());
    for r in &run.rows {
        let _ = writeln!(s, "{}\n=== {} [{}] ===", "=".repeat(78), r.id, r.expected_source);
        let _ = writeln!(s, "Q: {}\n", r.question.trim());
        let _ = writeln!(s, "--- expected ({}) ---\n{}\n", r.expected_rule_ids.join(" "), r.expected_answer.trim());
        match &r.outcome {
            Outcome::Verdict { answer, confidence, citations, category, source, cr_version } => {
                let _ = writeln!(s, "--- bot ({confidence}, {category}, {source}, CR {cr_version}) ---\n{}\n", answer.trim());
                for c in citations {
                    let _ = writeln!(s, "  * {c}");
                }
            }
            Outcome::Error { variant, message } => {
                let _ = writeln!(s, "--- bot: {variant} ---\n{message}");
            }
        }
        let _ = writeln!(
            s,
            "\nrecall {}/{} (missed: {})  cites {} rule / {} ruling / {} oracle  source {}  {:.1}s  ${:.4}\n",
            r.recall.hit.len(),
            r.recall.hit.len() + r.recall.missed.len(),
            r.recall.missed.join(" "),
            r.cites.n_rule_cites,
            r.cites.n_ruling_cites,
            r.cites.n_oracle_cites,
            if r.source_ok { "ok" } else { "MISMATCH" },
            secs(r.elapsed_ms),
            r.usd
        );
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_and_flags() -> anyhow::Result<()> {
        let o = Options::parse(["--label", "smoke"].iter().map(|s| (*s).to_owned()))?;
        assert_eq!(o.limit, 2);
        assert!((o.max_usd - 2.0).abs() < f64::EPSILON);
        assert_eq!(o.out, PathBuf::from("eval/runs/smoke.json"));
        let o = Options::parse(
            ["--limit", "5", "--ids", "a, b", "--max-usd", "0.5", "--out", "x/y.json"].iter().map(|s| (*s).to_owned()),
        )?;
        assert_eq!((o.limit, o.ids.len(), o.label.as_str()), (5, 2, "y"));
        // --ids without --limit runs every listed id.
        let o = Options::parse(["--label", "l", "--ids", "a,b,c,d,e"].iter().map(|s| (*s).to_owned()))?;
        assert_eq!(o.limit, 5);
        assert!(Options::parse(std::iter::empty()).is_err());
        assert!(Options::parse(["--bogus"].iter().map(|s| (*s).to_owned())).is_err());
        assert!(Options::parse(["--label", "l", "--limit", "0"].iter().map(|s| (*s).to_owned())).is_err());
        assert!(Options::parse(["--label", "l", "--max-usd", "nan"].iter().map(|s| (*s).to_owned())).is_err());
        assert!(Options::parse(["--label", "l", "--max-usd", "-1"].iter().map(|s| (*s).to_owned())).is_err());
        Ok(())
    }

    #[test]
    fn out_of_scope_error_counts_as_correct() {
        let q = GoldQuestion {
            id: "t".into(),
            question: "q".into(),
            cards: vec![],
            nicknames_used: vec![],
            categories: vec![],
            source: "Tournament".into(),
            expected_rule_ids: vec![],
            expected_answer: String::new(),
            equivalent_rule_ids: std::collections::BTreeMap::default(),
        };
        let r = score_row(&q, &Err(JudgeError::OutOfScope(judge_core::Source::Tournament)), 1, 1, 0.01);
        assert!(r.correct_shape && r.source_ok);
        let r = score_row(&q, &Err(JudgeError::OutOfScope(judge_core::Source::OutOfScope)), 1, 1, 0.01);
        assert!(r.correct_shape && !r.source_ok);
        let r = score_row(&q, &Err(JudgeError::LlmRefused), 1, 1, 0.01);
        assert!(!r.correct_shape);
        let run = Run { label: "l".into(), gold: "g".into(), max_usd: 1.0, models: None, rows: vec![r], total_usd: 0.01, total_calls: 1 };
        assert!(table(&run).contains("TOTAL $0.0100"));
        assert!(table(&run).contains("(cap $1.00); models=unknown"), "{}", table(&run));
        assert!(table(&run).contains("questions with ≥1 expected id cited: 0/0"), "{}", table(&run));
        let with_models = Run { models: Some(RunModels { extract: "ollama/qwen3:8b".into(), synth: "anthropic/claude-opus-5".into() }), ..run };
        assert!(table(&with_models).contains("(cap $1.00); extract=ollama/qwen3:8b synth=anthropic/claude-opus-5"), "{}", table(&with_models));
    }

    #[test]
    fn verdict_rows_count_cites_and_any_expected() -> anyhow::Result<()> {
        use judge_core::{AnswerableSource, Category, Citation, Confidence, Context, CrVersion, RuleChunk, RuleId, Verdict};
        let q = GoldQuestion {
            id: "t".into(),
            question: "q".into(),
            cards: vec![],
            nicknames_used: vec![],
            categories: vec![],
            source: "CR".into(),
            expected_rule_ids: vec![crate::gold::YamlScalar::Text("702.15".into()), crate::gold::YamlScalar::Text("1.1".into())],
            expected_answer: String::new(),
            equivalent_rule_ids: std::collections::BTreeMap::default(),
        };
        let ctx = Context {
            rules: vec![RuleChunk {
                id: RuleId::try_new("702.15b".to_owned())?,
                parent_id: None,
                subsection: RuleId::try_new("702".to_owned())?,
                heading: "Lifelink".into(),
                body: "gain that much life".into(),
                examples: vec![],
                cr_version: CrVersion::try_new("20250801".to_owned())?,
            }],
            ..Context::default()
        };
        let v = Verdict::new(
            "Lifelink causes its controller to gain that much life at the same time.".into(),
            Confidence::High,
            vec![Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain that much life".into() }],
            Category::KeywordAbilities,
        )
        .validate(&ctx, AnswerableSource::Cr)?;
        let r = score_row(&q, &Ok(v), 1, 1, 0.01);
        assert!(r.any_expected_cited);
        assert_eq!((r.cites.n_rule_cites, r.cites.n_ruling_cites), (1, 0));
        assert_eq!(r.recall.missed, vec!["1.1".to_owned()]);
        let run = Run { label: "l".into(), gold: "g".into(), max_usd: 1.0, models: None, rows: vec![r], total_usd: 0.01, total_calls: 1 };
        assert_eq!(run.any_expected_cited(), (1, 1));
        let t = table(&run);
        assert!(t.contains("questions with ≥1 expected id cited: 1/1; 1 rule citations, 0 ruling citations, 0 oracle citations"), "{t}");
        // Old run files without the new fields still load.
        let json = serde_json::to_string(&run)?.replace(",\"any_expected_cited\":true", "").replace(",\"n_rule_cites\":1", "");
        let old: Run = serde_json::from_str(&json)?;
        assert!(!old.rows.first().is_some_and(|r| r.any_expected_cited));
        Ok(())
    }

    #[test]
    fn legacy_row_without_cite_fields_loads_with_defaults() -> anyhow::Result<()> {
        // A row as written before `any_expected_cited` / `n_rule_cites` / `n_ruling_cites` existed.
        let json = r#"{
            "id": "g1", "question": "q", "expected_answer": "a", "expected_source": "CR",
            "expected_rule_ids": ["702.15"],
            "outcome": {"kind": "error", "variant": "LlmRefused", "message": "refused"},
            "cited_rule_ids": [], "recall": {"hit": [], "missed": ["702.15"]},
            "source_ok": false, "correct_shape": false, "elapsed_ms": 12, "calls": 1, "usd": 0.01
        }"#;
        let row: Row = serde_json::from_str(json)?;
        assert!(!row.any_expected_cited);
        assert_eq!(row.cites, CiteCounts::default());
        assert_eq!(row.recall.missed, vec!["702.15".to_owned()]);
        Ok(())
    }
}
