//! `eval sweep <plan.toml>`: a model × effort grid over the gold set, run
//! from one plan file, resumable, under one spend cap.
//!
//! A plan names up to three kinds of cell:
//!
//! - `[extract]`: the extraction stage alone. Each question is extracted,
//!   its spans resolved and its material retrieved, with no synthesis. Scored
//!   on the source, the resolved cards, the primary category and the gold rule
//!   ids the synthesis prompt would show (`recall`'s measure, on the model's
//!   own extraction). One small call per question.
//! - `[synth]`: the synthesis stage, on the gold extraction by default
//!   (`answer --gold-extraction`), so the extractor cannot move its score.
//! - `[[pipeline]]`: named end-to-end configurations (`answer`).
//!
//! Every cell runs `repeats` times. Each run writes a `judge.toml` and a run
//! file under `eval/runs/sweep-<label>/`; a run whose file already holds every
//! question is not repeated, so a sweep stopped by the cap or a failure picks
//! up where it stopped. Answers are graded by the plan's `[grader]` model,
//! the same for every cell, so grades compare across cells. The summary
//! (`summary.md` beside the runs) averages each cell over its repeats.
//! Spend is what the meter counts from each response's usage at the price
//! table, the same figure the spend cap uses. Every run and grading pass is
//! appended to `ledger.jsonl`, and `max_usd` is checked against the whole
//! ledger, so it caps the sweep across invocations. A run its own cap cut
//! short is recorded as cut and neither graded, counted nor retried.

use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::Context as _;
use futures::StreamExt as _;
use judge_bot::{
    config::Config,
    synth::{Budget, shown_rules},
};
use judge_core::{Question, judge::collect_resolved};
use judge_llm::LlmError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{
    answer,
    gold::{Gold, GoldQuestion},
    grade,
};

/// The provider tables a generated `judge.toml` starts from when the plan
/// names no `base`: Anthropic direct and Voyage, as the published configs.
const DEFAULT_BASE: &str = r#"
[providers.anthropic]
kind = "anthropic"
endpoint = "direct"
api_key_env = "ANTHROPIC_API_KEY"

[providers.voyage]
kind = "voyage"

[models.embed]
provider = "voyage"
model = "voyage-3.5"
"#;

/// Below this much left of the sweep's cap, no new run starts: a synthesis
/// reserves its worst case before sending, and a run that cannot afford one
/// question records nothing useful.
const MIN_RUN_USD: f64 = 0.50;

/// An effort level as `judge.toml` spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortName {
    /// `low`.
    Low,
    /// `medium`.
    Medium,
    /// `high`.
    High,
    /// `xhigh`.
    Xhigh,
    /// `max`.
    Max,
}

impl EffortName {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// One stage's model, as a plan names it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    /// Model id, e.g. `claude-opus-5-5`.
    pub model: String,
    /// Effort level.
    pub effort: EffortName,
    /// Provider table in the base config; the plan's `provider` when absent.
    #[serde(default)]
    pub provider: Option<String>,
}

/// Models × efforts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grid {
    models: Vec<String>,
    efforts: Vec<EffortName>,
    #[serde(default)]
    provider: Option<String>,
}

/// `[synth]`: a grid, on the gold extraction unless `extract` names a model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SynthGrid {
    #[serde(flatten)]
    grid: Grid,
    /// A fixed extraction model; absent = the gold extraction.
    #[serde(default)]
    extract: Option<StageSpec>,
}

/// `[[pipeline]]`: one named end-to-end configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineSpec {
    name: String,
    extract: StageSpec,
    synth: StageSpec,
}

/// `[grader]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraderSpec {
    model: String,
    #[serde(default)]
    provider: Option<String>,
    /// Cap for one run's grading pass.
    #[serde(default = "default_grade_usd")]
    max_usd: f64,
}

const fn default_grade_usd() -> f64 {
    1.50
}

const fn default_run_usd() -> f64 {
    8.00
}

const fn default_repeats() -> u32 {
    1
}

const fn default_concurrency() -> usize {
    4
}

const fn default_parallel_runs() -> usize {
    3
}

fn default_provider() -> String {
    "anthropic".to_owned()
}

/// A plan file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlan {
    label: String,
    #[serde(default = "default_repeats")]
    repeats: u32,
    max_usd: f64,
    #[serde(default = "default_run_usd")]
    run_max_usd: f64,
    /// Questions in flight at once within one run.
    #[serde(default = "default_concurrency")]
    concurrency: usize,
    /// Runs in flight at once.
    #[serde(default = "default_parallel_runs")]
    parallel_runs: usize,
    #[serde(default)]
    base: Option<PathBuf>,
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default)]
    gold: Option<PathBuf>,
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    grader: Option<GraderSpec>,
    #[serde(default)]
    extract: Option<Grid>,
    #[serde(default)]
    synth: Option<SynthGrid>,
    #[serde(default)]
    pipeline: Vec<PipelineSpec>,
}

/// What kind of run a cell is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Extraction alone.
    Extract,
    /// Synthesis on a fixed extraction.
    Synth,
    /// End to end.
    Pipeline,
}

impl Kind {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "extract" => Ok(Self::Extract),
            "synth" => Ok(Self::Synth),
            "pipeline" => Ok(Self::Pipeline),
            other => anyhow::bail!("--only: unknown kind {other:?} (extract, synth, pipeline)"),
        }
    }
}

/// One configuration the sweep runs `repeats` times.
#[derive(Clone, Debug)]
pub struct Cell {
    kind: Kind,
    /// File stem: `extract-opus-5-5-low`, `pipeline-default`.
    name: String,
    /// The extraction stage; `None` = the gold extraction.
    extract: Option<StageSpec>,
    /// The synthesis stage; for an extract cell, unused but required by the
    /// config loader (the extraction model at `low`).
    synth: StageSpec,
}

impl Cell {
    /// `extract=… synth=…` for the summary.
    fn describe(&self) -> String {
        let stage = |s: &StageSpec| format!("{}@{}", s.model, s.effort.as_str());
        match self.kind {
            Kind::Extract => self.extract.as_ref().map_or_else(String::new, stage),
            Kind::Synth | Kind::Pipeline => format!(
                "extract={} synth={}",
                self.extract
                    .as_ref()
                    .map_or_else(|| "gold".to_owned(), stage),
                stage(&self.synth)
            ),
        }
    }
}

/// A validated plan.
pub struct Plan {
    label: String,
    repeats: u32,
    max_usd: f64,
    run_max_usd: f64,
    concurrency: usize,
    parallel_runs: usize,
    base: toml::Table,
    gold: PathBuf,
    ids: Vec<String>,
    grader: Option<(StageSpec, f64)>,
    cells: Vec<Cell>,
}

/// `claude-opus-5-5` → `opus-5-5`; anything outside `[a-z0-9.-]` → `-`.
fn slug(model: &str) -> String {
    model
        .trim_start_matches("claude-")
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn require(what: &str, list_len: usize) -> anyhow::Result<()> {
    if list_len == 0 {
        anyhow::bail!("{what} must not be empty");
    }
    Ok(())
}

impl Plan {
    /// Read and check a plan file.
    ///
    /// # Errors
    /// Unreadable or unknown keys, empty grids, a non-positive cap, duplicate
    /// cell names, or a plan with no cells.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawPlan =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawPlan) -> anyhow::Result<Self> {
        if raw.label.is_empty() || slug(&raw.label) != raw.label {
            anyhow::bail!(
                "label {:?}: use lowercase letters, digits, '.' and '-'",
                raw.label
            );
        }
        if raw.repeats == 0 {
            anyhow::bail!("repeats must be at least 1");
        }
        if raw.concurrency == 0 || raw.parallel_runs == 0 {
            anyhow::bail!("concurrency and parallel_runs must be at least 1");
        }
        for (key, v) in [("max_usd", raw.max_usd), ("run_max_usd", raw.run_max_usd)] {
            if !(v.is_finite() && v > 0.0) {
                anyhow::bail!("{key} must be a positive number");
            }
        }
        let base_text = match &raw.base {
            Some(p) => std::fs::read_to_string(p)
                .with_context(|| format!("reading base config {}", p.display()))?,
            None => DEFAULT_BASE.to_owned(),
        };
        let base: toml::Table = toml::from_str(&base_text).context("parsing the base config")?;
        let with_provider = |mut s: StageSpec, fallback: &Option<String>| {
            if s.provider.is_none() {
                s.provider = Some(fallback.clone().unwrap_or_else(|| raw.provider.clone()));
            }
            s
        };
        let mut cells = Vec::new();
        if let Some(g) = &raw.extract {
            require("[extract] models", g.models.len())?;
            require("[extract] efforts", g.efforts.len())?;
            for model in &g.models {
                for &effort in &g.efforts {
                    let spec = with_provider(
                        StageSpec {
                            model: model.clone(),
                            effort,
                            provider: None,
                        },
                        &g.provider,
                    );
                    cells.push(Cell {
                        kind: Kind::Extract,
                        name: format!("extract-{}-{}", slug(model), effort.as_str()),
                        synth: StageSpec {
                            effort: EffortName::Low,
                            ..spec.clone()
                        },
                        extract: Some(spec),
                    });
                }
            }
        }
        if let Some(s) = &raw.synth {
            require("[synth] models", s.grid.models.len())?;
            require("[synth] efforts", s.grid.efforts.len())?;
            let extract = s.extract.clone().map(|e| with_provider(e, &None));
            for model in &s.grid.models {
                for &effort in &s.grid.efforts {
                    cells.push(Cell {
                        kind: Kind::Synth,
                        name: format!("synth-{}-{}", slug(model), effort.as_str()),
                        extract: extract.clone(),
                        synth: with_provider(
                            StageSpec {
                                model: model.clone(),
                                effort,
                                provider: None,
                            },
                            &s.grid.provider,
                        ),
                    });
                }
            }
        }
        for p in raw.pipeline {
            if p.name.is_empty() || slug(&p.name) != p.name {
                anyhow::bail!(
                    "pipeline name {:?}: use lowercase letters, digits, '.' and '-'",
                    p.name
                );
            }
            cells.push(Cell {
                kind: Kind::Pipeline,
                name: format!("pipeline-{}", p.name),
                extract: Some(with_provider(p.extract, &None)),
                synth: with_provider(p.synth, &None),
            });
        }
        if cells.is_empty() {
            anyhow::bail!("the plan has no cells: add [extract], [synth] or [[pipeline]]");
        }
        let mut seen = HashSet::new();
        if let Some(dup) = cells.iter().find(|c| !seen.insert(c.name.clone())) {
            anyhow::bail!("two cells are both named {}", dup.name);
        }
        let grader = match raw.grader {
            Some(g) => {
                if !(g.max_usd.is_finite() && g.max_usd > 0.0) {
                    anyhow::bail!("[grader] max_usd must be a positive number");
                }
                let spec = StageSpec {
                    model: g.model,
                    effort: EffortName::Medium,
                    provider: Some(g.provider.unwrap_or_else(|| raw.provider.clone())),
                };
                Some((spec, g.max_usd))
            }
            None => None,
        };
        Ok(Self {
            label: raw.label,
            repeats: raw.repeats,
            max_usd: raw.max_usd,
            run_max_usd: raw.run_max_usd,
            concurrency: raw.concurrency,
            parallel_runs: raw.parallel_runs,
            base,
            gold: raw.gold.unwrap_or_else(crate::gold::default_path),
            ids: raw.ids,
            grader,
            cells,
        })
    }

    fn dir(&self) -> PathBuf {
        PathBuf::from("eval/runs").join(format!("sweep-{}", self.label))
    }

    /// The `judge.toml` for `extract` + `synth` on this plan's base.
    fn config_text(&self, extract: &StageSpec, synth: &StageSpec) -> anyhow::Result<String> {
        let mut doc = self.base.clone();
        let models = doc
            .entry("models")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("base config: `models` is not a table"))?;
        for (stage, spec) in [("extract", extract), ("synth", synth)] {
            let mut t = toml::Table::new();
            t.insert(
                "provider".into(),
                spec.provider
                    .clone()
                    .unwrap_or_else(default_provider)
                    .into(),
            );
            t.insert("model".into(), spec.model.clone().into());
            t.insert("effort".into(), spec.effort.as_str().into());
            models.insert(stage.into(), toml::Value::Table(t));
        }
        Ok(format!(
            "# Written by `judge-eval sweep` for one cell; edits are overwritten.\n{}",
            toml::to_string(&doc)?
        ))
    }
}

/// Parsed `sweep` flags.
pub struct Options {
    plan: PathBuf,
    dry_run: bool,
    summary_only: bool,
    only: Option<Vec<Kind>>,
    repeats: Option<u32>,
    max_usd: Option<f64>,
    concurrency: Option<usize>,
    parallel: Option<usize>,
}

impl Options {
    /// Parse `sweep <plan.toml> [--dry-run] [--summary] [--only k,k] [--repeats N] [--max-usd X]`.
    ///
    /// # Errors
    /// On an unknown flag, a missing value or no plan path.
    pub fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut plan = None;
        let (mut dry_run, mut summary_only, mut only, mut repeats, mut max_usd) =
            (false, false, None, None, None);
        let (mut concurrency, mut parallel) = (None, None);
        while let Some(a) = args.next() {
            let mut val = || {
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
            };
            match a.as_str() {
                "--dry-run" => dry_run = true,
                "--summary" => summary_only = true,
                "--only" => {
                    only = Some(
                        val()?
                            .split(',')
                            .map(|s| Kind::parse(s.trim()))
                            .collect::<anyhow::Result<Vec<_>>>()?,
                    );
                }
                "--repeats" => {
                    let n: u32 = val()?.parse().context("--repeats must be an integer")?;
                    if n == 0 {
                        anyhow::bail!("--repeats must be at least 1");
                    }
                    repeats = Some(n);
                }
                "--concurrency" | "--parallel" => {
                    let n: usize = val()?
                        .parse()
                        .with_context(|| format!("{a} must be an integer"))?;
                    if n == 0 {
                        anyhow::bail!("{a} must be at least 1");
                    }
                    if a == "--concurrency" {
                        concurrency = Some(n);
                    } else {
                        parallel = Some(n);
                    }
                }
                "--max-usd" => {
                    let v: f64 = val()?.parse().context("--max-usd must be a number")?;
                    if !(v.is_finite() && v > 0.0) {
                        anyhow::bail!("--max-usd must be a positive number");
                    }
                    max_usd = Some(v);
                }
                flag if flag.starts_with("--") => anyhow::bail!("unknown flag {flag}"),
                path if plan.is_none() => plan = Some(PathBuf::from(path)),
                extra => anyhow::bail!("unexpected argument {extra}"),
            }
        }
        Ok(Self {
            plan: plan.ok_or_else(|| anyhow::anyhow!("sweep needs a plan file"))?,
            dry_run,
            summary_only,
            only,
            repeats,
            max_usd,
            concurrency,
            parallel,
        })
    }
}

/// One extraction-only question.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractRow {
    /// Gold id.
    pub id: String,
    /// The spans the model wrote.
    pub spans: Vec<String>,
    /// The source it chose (`cr`, …), or the error.
    pub source: String,
    /// Whether the source matches the gold source.
    pub source_ok: bool,
    /// Resolved card names, or why resolution stopped.
    pub cards: Result<Vec<String>, String>,
    /// The resolved cards equal the gold cards.
    pub cards_ok: bool,
    /// The primary category it chose.
    pub primary: String,
    /// The primary category is one the gold labels map to; `None` when no
    /// gold label maps to a category.
    pub primary_ok: Option<bool>,
    /// Gold rule ids (decisive and supporting) the synthesis prompt would
    /// show from this extraction, and how many there are. Zero shown when the
    /// pipeline would stop before retrieval. `(0, 0)` for an unanswerable question.
    pub shown: (usize, usize),
    /// The extraction call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall time in milliseconds.
    pub elapsed_ms: u128,
    /// Model and embedding spend.
    pub usd: f64,
}

/// An extraction-only run file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExtractRun {
    /// File stem plus repeat.
    pub label: String,
    /// `provider/model`.
    pub model: String,
    /// Effort as configured.
    pub effort: String,
    /// Rows in gold order.
    pub rows: Vec<ExtractRow>,
    /// Sum of `usd`.
    pub total_usd: f64,
    /// A question was refused at the spend cap (see [`answer::Run::capped`]).
    #[serde(default)]
    pub capped: bool,
}

/// Run the extraction stage alone over `questions`, `concurrency` at a time.
async fn run_extract(
    pool: PgPool,
    questions: &[&GoldQuestion],
    config_path: &Path,
    cap: f64,
    label: &str,
    concurrency: usize,
) -> anyhow::Result<ExtractRun> {
    let config = Config::load_from(Some(config_path))?;
    tracing::info!("{}", config.summary());
    let models = config.models()?;
    config.probe_auth().await?;
    models.meter().set_max_spend_usd(cap)?;
    let vectors = config.vectors(pool.clone())?;
    if let Some(v) = &vectors {
        v.enabled().await;
    }
    let stage = config
        .extract()
        .ok_or_else(|| anyhow::anyhow!("no extraction stage configured"))?;
    let (model, effort) = (stage.label(), format!("{:?}", stage.effort).to_lowercase());
    let deps = judge_bot::build_deps_with(pool, &models, vectors, &config.deps_config());
    let mut rows = Vec::with_capacity(questions.len());
    let mut capped_run = false;
    crate::concurrent::in_order(
        questions.iter().copied(),
        concurrency,
        |q| extract_one(&deps, q),
        |(row, capped)| {
            tracing::info!(id = %row.id, source = row.source_ok, cards = row.cards_ok, usd = format_args!("{:.4}", row.usd), "extracted");
            rows.push(row);
            if capped {
                capped_run = true;
                tracing::warn!("spend cap reached; stopping the run");
                return Ok(crate::concurrent::Flow::Stop);
            }
            Ok(crate::concurrent::Flow::Continue)
        },
    )
    .await?;
    Ok(ExtractRun {
        label: label.to_owned(),
        model,
        effort,
        total_usd: rows.iter().map(|r| r.usd).sum(),
        capped: capped_run,
        rows,
    })
}

/// One question through extraction, resolution and retrieval; `true` when
/// the extraction was refused at the spend cap. A database failure is the
/// row's error, so the run, and what it spent, carries on.
async fn extract_one(
    deps: &judge_core::Deps,
    q: &GoldQuestion,
) -> anyhow::Result<(ExtractRow, bool)> {
    let tally = judge_core::Tally::new();
    let started = Instant::now();
    match extract_scored(deps, q, &tally, started).await {
        Ok(done) => Ok(done),
        Err(e) => Ok((
            ExtractRow {
                id: q.id.clone(),
                spans: Vec::new(),
                source: String::new(),
                source_ok: false,
                cards: Err("not scored".into()),
                cards_ok: false,
                primary: String::new(),
                primary_ok: None,
                shown: (
                    0,
                    if q.is_answerable() {
                        q.all_ids().len()
                    } else {
                        0
                    },
                ),
                error: Some(format!("{e:#}")),
                elapsed_ms: started.elapsed().as_millis(),
                usd: tally.cost().usd(),
            },
            false,
        )),
    }
}

async fn extract_scored(
    deps: &judge_core::Deps,
    q: &GoldQuestion,
    tally: &judge_core::Tally,
    started: Instant,
) -> anyhow::Result<(ExtractRow, bool)> {
    let question = Question {
        thread_id: q.id.clone(),
        text: q.question.clone(),
    };
    let all_ids = if q.is_answerable() {
        q.all_ids()
    } else {
        Vec::new()
    };
    let e = match deps.extractor.extract(&question, &[], tally).await {
        Ok(e) => e,
        Err(e) => {
            let capped = matches!(&e, judge_core::JudgeError::Upstream(u)
                if u.downcast_ref::<LlmError>().is_some_and(|c| matches!(c, LlmError::SpendCapExceeded { .. })));
            let row = ExtractRow {
                id: q.id.clone(),
                spans: Vec::new(),
                source: String::new(),
                source_ok: false,
                cards: Err("not extracted".into()),
                cards_ok: false,
                primary: String::new(),
                primary_ok: None,
                shown: (0, all_ids.len()),
                error: Some(format!("{e:#}")),
                elapsed_ms: started.elapsed().as_millis(),
                usd: tally.cost().usd(),
            };
            return Ok((row, capped));
        }
    };
    let mut resolutions = Vec::with_capacity(e.card_spans.len());
    for span in &e.card_spans {
        resolutions.push(deps.resolver.resolve(span).await?);
    }
    let cards = collect_resolved(resolutions).map_err(|err| format!("{err:#}"));
    // Gold names resolve to ids (a double-faced card's gold name is its front
    // face), and extra cards the question names are not a miss: every gold
    // card resolved, and no span left the pipeline asking "did you mean?".
    let mut want = HashSet::new();
    for name in &q.cards {
        if let judge_core::Resolution::Resolved { card, .. } = deps.resolver.resolve(name).await? {
            want.insert(card.id);
        }
    }
    let cards_ok = cards.as_ref().is_ok_and(|cards| {
        let got: HashSet<_> = cards.iter().map(|c| c.id).collect();
        want.len() == q.cards.len() && want.is_subset(&got)
    });
    let mapped = crate::categories::map_labels(&q.id, &q.categories);
    let primary_ok = (!mapped.is_empty()).then(|| mapped.contains(&e.primary_category()));
    let hit = match (&cards, e.source.answerable(), all_ids.is_empty()) {
        (Ok(cards), Some(_), false) => {
            let ctx = deps.retriever.retrieve(&question, cards, &e, tally).await?;
            let shown = shown_rules(&ctx, &[], &Budget::default());
            all_ids
                .iter()
                .filter(|id| crate::recall::present(shown.iter().copied(), id))
                .count()
        }
        _ => 0,
    };
    let row = ExtractRow {
        id: q.id.clone(),
        spans: e.card_spans.clone(),
        source: serde_json::to_value(e.source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        source_ok: crate::score::source_matches(&q.source, e.source),
        cards: cards.map(|c| c.into_iter().map(|c| c.name).collect()),
        cards_ok,
        primary: e.primary_category().to_string(),
        primary_ok,
        shown: (hit, all_ids.len()),
        error: None,
        elapsed_ms: started.elapsed().as_millis(),
        usd: tally.cost().usd(),
    };
    Ok((row, false))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// One line of `ledger.jsonl`: what a run or grading pass spent. The sweep's
/// cap is checked against the whole ledger, so it covers every invocation,
/// and spend on a run that was later redone stays counted.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    run: String,
    what: String,
    usd: f64,
    /// The run stopped at its own cap before every question: final, not
    /// retried (raise `run_max_usd` and delete the file to redo it).
    #[serde(default)]
    cut: bool,
}

fn read_ledger(dir: &Path) -> anyhow::Result<Vec<Entry>> {
    let path = dir.join("ledger.jsonl");
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).with_context(|| format!("parsing {}", path.display())))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn record(dir: &Path, entry: &Entry) -> anyhow::Result<()> {
    use std::io::Write as _;
    let path = dir.join("ledger.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(entry)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// A run file holds every selected question, in order.
fn complete(ids: &[String], have: impl Iterator<Item = String>) -> bool {
    have.collect::<Vec<_>>() == ids
}

/// One run of one cell.
struct Job {
    cell: Cell,
    label: String,
    out: PathBuf,
    config: PathBuf,
}

impl Job {
    /// The model a job mostly waits on, for spreading jobs across models.
    fn model(&self) -> &str {
        match (self.cell.kind, &self.cell.extract) {
            (Kind::Extract, Some(e)) => &e.model,
            _ => &self.cell.synth.model,
        }
    }
}

/// `jobs` reordered so neighbours wait on different models: each model's own
/// rate limit then carries part of the load when several run at once.
fn interleave(jobs: Vec<Job>) -> Vec<Job> {
    let mut queues: Vec<(String, std::collections::VecDeque<Job>)> = Vec::new();
    for job in jobs {
        let model = job.model().to_owned();
        match queues.iter_mut().find(|(m, _)| *m == model) {
            Some((_, q)) => q.push_back(job),
            None => queues.push((model, std::collections::VecDeque::from([job]))),
        }
    }
    let mut out = Vec::new();
    while queues.iter().any(|(_, q)| !q.is_empty()) {
        for (_, q) in &mut queues {
            if let Some(job) = q.pop_front() {
                out.push(job);
            }
        }
    }
    out
}

/// The sweep's money while runs are in flight: what the ledger holds, and
/// the caps handed to runs that have not settled.
struct Books {
    max_usd: f64,
    spent: f64,
    reserved: f64,
    /// Holds not yet settled. Waiting is decided on this count, not on
    /// `reserved`, which float rounding can leave a hair above zero.
    in_flight: usize,
}

/// What [`Books::reserve`] answers.
#[derive(Debug, PartialEq)]
enum Grant {
    /// This much is held for the run.
    Cap(f64),
    /// Not enough is free while other runs hold theirs: wait for one to settle.
    Wait,
    /// The sweep's cap is reached.
    Exhausted,
}

impl Books {
    /// Hold `want` for one run. Runs in flight hold their whole cap, so
    /// together they never pass `max_usd`. With less than `want` free, a run
    /// waits while others hold caps (they usually spend less and give the
    /// rest back), and takes what is left only when none does.
    fn reserve(&mut self, want: f64) -> Grant {
        let left = self.max_usd - self.spent - self.reserved;
        let cap = if left >= want {
            want
        } else if self.in_flight > 0 {
            return Grant::Wait;
        } else if left >= MIN_RUN_USD {
            left
        } else {
            return Grant::Exhausted;
        };
        self.reserved += cap;
        self.in_flight += 1;
        Grant::Cap(cap)
    }

    fn settle(&mut self, cap: f64, used: f64) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.reserved = if self.in_flight == 0 {
            0.0
        } else {
            self.reserved - cap
        };
        self.spent += used;
    }
}

/// What every job reads.
struct Shared<'a> {
    plan: &'a Plan,
    ids: &'a [String],
    questions: &'a [&'a GoldQuestion],
    dir: &'a Path,
    grader_config: Option<&'a Path>,
    cut: &'a HashSet<String>,
    concurrency: usize,
    books: std::cell::RefCell<Books>,
    /// Signalled whenever a run settles, for runs waiting on a cap.
    settled: tokio::sync::Notify,
}

impl<'a> Shared<'a> {
    /// [`Books::reserve`], waiting while it says to. The cap is given back
    /// when the [`Hold`] is finished or dropped, on every path out.
    async fn reserve(&self, want: f64) -> Option<Hold<'_, 'a>> {
        loop {
            // Made before the check, so a settle between the two still wakes it.
            let settled = self.settled.notified();
            match self.books.borrow_mut().reserve(want) {
                Grant::Cap(cap) => {
                    return Some(Hold {
                        sh: self,
                        cap,
                        settled: false,
                    });
                }
                Grant::Exhausted => return None,
                Grant::Wait => {}
            }
            settled.await;
        }
    }

    fn settle(&self, cap: f64, used: f64) {
        self.books.borrow_mut().settle(cap, used);
        self.settled.notify_waiters();
    }
}

/// A cap held for one run or grading pass.
struct Hold<'s, 'a> {
    sh: &'s Shared<'a>,
    cap: f64,
    settled: bool,
}

impl Hold<'_, '_> {
    /// Give back what `used` left of the cap.
    fn finish(mut self, used: f64) {
        self.settled = true;
        self.sh.settle(self.cap, used);
    }
}

impl Drop for Hold<'_, '_> {
    /// An error path gives the whole cap back, so no run waits on it.
    fn drop(&mut self) {
        if !self.settled {
            self.sh.settle(self.cap, 0.0);
        }
    }
}

/// An answer run that holds every question and was not cut at the cap.
fn answer_complete(run: &answer::Run, ids: &[String]) -> bool {
    !run.capped && complete(ids, run.rows.iter().map(|r| r.id.clone()))
}

/// An extraction run that holds every question and was not cut at the cap.
fn extract_complete(run: &ExtractRun, ids: &[String]) -> bool {
    !run.capped && complete(ids, run.rows.iter().map(|r| r.id.clone()))
}

fn is_done(job: &Job, ids: &[String]) -> bool {
    match job.cell.kind {
        Kind::Extract => {
            read_json::<ExtractRun>(&job.out).is_some_and(|run| extract_complete(&run, ids))
        }
        Kind::Synth | Kind::Pipeline => {
            read_json::<answer::Run>(&job.out).is_some_and(|run| answer_complete(&run, ids))
        }
    }
}

/// Run one job and grade it. `Ok(true)` when it finished and was graded.
async fn run_job(sh: &Shared<'_>, job: &Job) -> anyhow::Result<bool> {
    let label = &job.label;
    if is_done(job, sh.ids) {
        println!("{label}: done");
    } else if sh.cut.contains(label) {
        println!(
            "{label}: cut by its cap earlier; raise run_max_usd and delete {} to redo it",
            job.out.display()
        );
        return Ok(false);
    } else {
        let Some(hold) = sh.reserve(sh.plan.run_max_usd).await else {
            println!(
                "{label}: not started, the sweep cap ${:.2} is reached",
                sh.plan.max_usd
            );
            return Ok(false);
        };
        let cap = hold.cap;
        println!("{label}: {}  (cap ${cap:.2})", job.cell.describe());
        let pool = crate::connect(sh.concurrency).await?;
        let (used, finished, what) = match job.cell.kind {
            Kind::Extract => {
                let run = run_extract(pool, sh.questions, &job.config, cap, label, sh.concurrency)
                    .await?;
                // Settled before the write, so a failed write loses no spend.
                let (used, finished) = (run.total_usd, extract_complete(&run, sh.ids));
                hold.finish(used);
                write_json(&job.out, &run)?;
                println!("{label}: {}", extract_line(&[&run]));
                (used, finished, "extract")
            }
            Kind::Synth | Kind::Pipeline => {
                let a = answer::Options {
                    gold: sh.plan.gold.clone(),
                    limit: sh.ids.len(),
                    ids: sh.ids.to_vec(),
                    max_usd: cap,
                    out: job.out.clone(),
                    label: label.clone(),
                    gold_extraction: job.cell.extract.is_none(),
                    config: Some(job.config.clone()),
                    grade: false,
                    concurrency: sh.concurrency,
                };
                let (run, _) = answer::run(pool, &a).await?;
                let (used, finished) = (run.total_usd, answer_complete(&run, sh.ids));
                hold.finish(used);
                (used, finished, "answer")
            }
        };
        record(
            sh.dir,
            &Entry {
                run: label.clone(),
                what: what.into(),
                usd: used,
                cut: !finished,
            },
        )?;
        if !finished {
            println!("{label}: cut by its cap; not graded, not counted, not retried");
            return Ok(false);
        }
    }
    let (Kind::Synth | Kind::Pipeline, Some(path), Some((_, grade_cap))) =
        (job.cell.kind, sh.grader_config, &sh.plan.grader)
    else {
        return Ok(true);
    };
    let Some(hold) = sh.reserve(*grade_cap).await else {
        println!(
            "{label}: not graded, the sweep cap ${:.2} is reached",
            sh.plan.max_usd
        );
        return Ok(false);
    };
    let g = grade::Options {
        run: job.out.clone(),
        gold: sh.plan.gold.clone(),
        max_usd: hold.cap,
        config: Some(path.to_path_buf()),
        force: false,
        concurrency: sh.concurrency,
    };
    let (_, pass) = grade::run(&g).await?;
    hold.finish(pass.spent_usd);
    if pass.spent_usd > 0.0 {
        record(
            sh.dir,
            &Entry {
                run: label.clone(),
                what: "grade".into(),
                usd: pass.spent_usd,
                cut: false,
            },
        )?;
    }
    Ok(pass.complete())
}

/// `eval sweep`. `Ok(true)` when every run finished and was graded.
///
/// # Errors
/// An unreadable plan or gold file, or a config a cell generates that does
/// not load. A run that fails or is cut short by its cap is reported, and
/// the other runs go on.
pub async fn run(opts: &Options) -> anyhow::Result<bool> {
    let mut plan = Plan::load(&opts.plan)?;
    if let Some(n) = opts.repeats {
        plan.repeats = n;
    }
    if let Some(v) = opts.max_usd {
        plan.max_usd = v;
    }
    if let Some(n) = opts.concurrency {
        plan.concurrency = n;
    }
    if let Some(n) = opts.parallel {
        plan.parallel_runs = n;
    }
    if let Some(only) = &opts.only {
        plan.cells.retain(|c| only.contains(&c.kind));
    }
    let gold = crate::gold::load(&plan.gold)?;
    if let Some(unknown) = plan
        .ids
        .iter()
        .find(|id| !gold.questions.iter().any(|q| &q.id == *id))
    {
        anyhow::bail!("ids: no gold question with id {unknown:?}");
    }
    let questions: Vec<&GoldQuestion> = gold
        .questions
        .iter()
        .filter(|q| plan.ids.is_empty() || plan.ids.contains(&q.id))
        .collect();
    let ids: Vec<String> = questions.iter().map(|q| q.id.clone()).collect();
    let dir = plan.dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    if opts.summary_only {
        print_summary(&plan, &gold, &ids)?;
        return Ok(true);
    }

    let grader_config = match &plan.grader {
        Some((spec, _)) => {
            let path = dir.join("grader.judge.toml");
            std::fs::write(&path, plan.config_text(spec, spec)?)?;
            Some(path)
        }
        None => None,
    };
    let mut jobs = Vec::new();
    for cell in &plan.cells {
        let extract = cell.extract.clone().unwrap_or_else(|| StageSpec {
            effort: EffortName::Low,
            ..cell.synth.clone()
        });
        let config = dir.join(format!("{}.judge.toml", cell.name));
        std::fs::write(&config, plan.config_text(&extract, &cell.synth)?)?;
        // Every generated config must load before anything is spent.
        Config::load_from(Some(&config))
            .with_context(|| format!("the config for {} does not load", cell.name))?;
        for r in 1..=plan.repeats {
            let label = format!("{}-r{r}", cell.name);
            jobs.push(Job {
                cell: cell.clone(),
                out: dir.join(format!("{label}.json")),
                label,
                config: config.clone(),
            });
        }
    }
    let ledger = read_ledger(&dir)?;
    let cut: HashSet<String> = ledger
        .iter()
        .filter(|e| e.cut)
        .map(|e| e.run.clone())
        .collect();

    if opts.dry_run {
        for job in &jobs {
            let state = if is_done(job, &ids) {
                "done"
            } else if cut.contains(&job.label) {
                "cut by its cap"
            } else {
                "to run"
            };
            println!("{}: {state}  ({})", job.label, job.cell.describe());
        }
        println!(
            "{} cells × {} repeats over {} questions; {} runs at once, {} questions at once in each; sweep cap ${:.2} (${:.2} spent), run cap ${:.2}{}",
            plan.cells.len(),
            plan.repeats,
            ids.len(),
            plan.parallel_runs,
            plan.concurrency,
            plan.max_usd,
            ledger.iter().map(|e| e.usd).sum::<f64>(),
            plan.run_max_usd,
            plan.grader
                .as_ref()
                .map_or_else(String::new, |(g, c)| format!(
                    ", graded by {} (cap ${c:.2} a run)",
                    g.model
                ))
        );
        return Ok(true);
    }

    let sh = Shared {
        plan: &plan,
        ids: &ids,
        questions: &questions,
        dir: &dir,
        grader_config: grader_config.as_deref(),
        cut: &cut,
        concurrency: plan.concurrency,
        books: std::cell::RefCell::new(Books {
            max_usd: plan.max_usd,
            spent: ledger.iter().map(|e| e.usd).sum(),
            reserved: 0.0,
            in_flight: 0,
        }),
        settled: tokio::sync::Notify::new(),
    };
    let jobs = interleave(jobs);
    let results: Vec<bool> = futures::stream::iter(&jobs)
        .map(|job| {
            let sh = &sh;
            async move {
                match run_job(sh, job).await {
                    Ok(done) => done,
                    Err(e) => {
                        tracing::error!(run = %job.label, "run failed: {e:#}");
                        println!("{}: failed: {e:#}", job.label);
                        false
                    }
                }
            }
        })
        .buffer_unordered(plan.parallel_runs.max(1))
        .collect()
        .await;
    let spent = sh.books.borrow().spent;
    println!("\nsweep total ${spent:.4} of ${:.2}", plan.max_usd);
    print_summary(&plan, &gold, &ids)?;
    Ok(results.into_iter().all(|done| done))
}

fn ratio(n: usize, d: usize) -> Option<f64> {
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts of gold questions, far below 2^53"
    )]
    let r = (d > 0).then(|| n as f64 / d as f64);
    r
}

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_owned(), |f| format!("{:.0}%", f * 100.0))
}

/// Mean of `xs`, with the range when there is more than one.
fn spread(xs: &[f64], fmt: impl Fn(f64) -> String) -> String {
    if xs.is_empty() {
        return "-".to_owned();
    }
    #[expect(clippy::cast_precision_loss, reason = "a handful of repeats")]
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    if xs.len() == 1 {
        return fmt(mean);
    }
    let lo = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("{} ({}–{})", fmt(mean), fmt(lo), fmt(hi))
}

fn as_pct(f: f64) -> String {
    format!("{:.0}%", f * 100.0)
}

fn as_usd(f: f64) -> String {
    format!("${f:.3}")
}

fn as_secs(f: f64) -> String {
    format!("{f:.1}s")
}

/// One line over an extraction cell's runs, for the progress output.
fn extract_line(runs: &[&ExtractRun]) -> String {
    let rows: Vec<&ExtractRow> = runs.iter().flat_map(|r| &r.rows).collect();
    let n = rows.len();
    let shown: (usize, usize) = rows
        .iter()
        .fold((0, 0), |(h, t), r| (h + r.shown.0, t + r.shown.1));
    format!(
        "source {} cards {} primary {} shown {} errors {}",
        pct(ratio(rows.iter().filter(|r| r.source_ok).count(), n)),
        pct(ratio(rows.iter().filter(|r| r.cards_ok).count(), n)),
        pct(ratio(
            rows.iter().filter(|r| r.primary_ok == Some(true)).count(),
            rows.iter().filter(|r| r.primary_ok.is_some()).count()
        )),
        pct(ratio(shown.0, shown.1)),
        rows.iter().filter(|r| r.error.is_some()).count()
    )
}

fn mean_secs(ms: impl Iterator<Item = u128>) -> Option<f64> {
    let v: Vec<u128> = ms.collect();
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let s = (!v.is_empty()).then(|| v.iter().sum::<u128>() as f64 / v.len() as f64 / 1000.0);
    s
}

/// The summary table, printed and written to `summary.md`.
fn print_summary(plan: &Plan, gold: &Gold, ids: &[String]) -> anyhow::Result<()> {
    let answerable: HashSet<&str> = gold
        .questions
        .iter()
        .filter(|q| q.is_answerable())
        .map(|q| q.id.as_str())
        .collect();
    let dir = plan.dir();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# Sweep {}\n\n{} questions, up to {} repeats per cell; grader {}; {} questions at once in a run and {} runs at once, so secs/q includes waiting on the others. Each figure is the mean over the cell's complete runs, with the range across runs in brackets. Cost is per run of all questions.\n",
        plan.label,
        ids.len(),
        plan.repeats,
        plan.grader
            .as_ref()
            .map_or_else(|| "none".to_owned(), |(g, _)| g.model.clone()),
        plan.concurrency,
        plan.parallel_runs
    );
    let runs_of = |cell: &Cell| -> Vec<PathBuf> {
        (1..=plan.repeats)
            .map(|r| dir.join(format!("{}-r{r}.json", cell.name)))
            .collect()
    };
    let extract_cells: Vec<&Cell> = plan
        .cells
        .iter()
        .filter(|c| c.kind == Kind::Extract)
        .collect();
    if !extract_cells.is_empty() {
        let _ = writeln!(
            s,
            "## Extraction\n\nsource = source matches gold; cards = resolved cards equal gold; primary = primary category among gold's; shown = gold rule ids the synthesis prompt would show.\n\n| cell | runs | source | cards | primary | shown | errors | cost | secs/q |\n|---|---|---|---|---|---|---|---|---|"
        );
        for cell in extract_cells {
            let runs: Vec<ExtractRun> = runs_of(cell)
                .iter()
                .filter_map(|p| read_json::<ExtractRun>(p))
                .filter(|r| extract_complete(r, ids))
                .collect();
            let per = |f: &dyn Fn(&ExtractRun) -> Option<f64>| -> Vec<f64> {
                runs.iter().filter_map(f).collect()
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                cell.describe(),
                runs.len(),
                spread(
                    &per(&|r| ratio(r.rows.iter().filter(|x| x.source_ok).count(), r.rows.len())),
                    as_pct
                ),
                spread(
                    &per(&|r| ratio(r.rows.iter().filter(|x| x.cards_ok).count(), r.rows.len())),
                    as_pct
                ),
                spread(
                    &per(&|r| ratio(
                        r.rows.iter().filter(|x| x.primary_ok == Some(true)).count(),
                        r.rows.iter().filter(|x| x.primary_ok.is_some()).count()
                    )),
                    as_pct
                ),
                spread(
                    &per(&|r| {
                        let (h, t) = r
                            .rows
                            .iter()
                            .fold((0, 0), |(h, t), x| (h + x.shown.0, t + x.shown.1));
                        ratio(h, t)
                    }),
                    as_pct
                ),
                spread(
                    #[expect(clippy::cast_precision_loss, reason = "a count of questions")]
                    &per(&|r| Some(r.rows.iter().filter(|x| x.error.is_some()).count() as f64)),
                    |f| format!("{f:.1}")
                ),
                spread(&per(&|r| Some(r.total_usd)), as_usd),
                spread(
                    &per(&|r| mean_secs(r.rows.iter().map(|x| x.elapsed_ms))),
                    as_secs
                ),
            );
        }
        s.push('\n');
    }
    for (kind, title) in [(Kind::Synth, "Synthesis"), (Kind::Pipeline, "End to end")] {
        let cells: Vec<&Cell> = plan.cells.iter().filter(|c| c.kind == kind).collect();
        if cells.is_empty() {
            continue;
        }
        let _ = writeln!(
            s,
            "## {title}\n\nshape = answered or declined as gold says; errors = questions that ended in an error where gold expects an answer; recall = decisive rule ids cited; retried = first attempt rejected; graded = answers graded / questions gold expects an answer to; agree, partial and follows are shares of those questions (an error or an ungraded answer counts against them); supported = claims the quotes support, over graded answers.\n\n| cell | runs | shape | errors | recall | retried | graded | agree | partial | follows | supported | cost | grading | secs/q |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
        );
        for cell in cells {
            let runs: Vec<answer::Run> = runs_of(cell)
                .iter()
                .filter_map(|p| read_json::<answer::Run>(p))
                .filter(|r| answer_complete(r, ids))
                .collect();
            let per = |f: &dyn Fn(&answer::Run) -> Option<f64>| -> Vec<f64> {
                runs.iter().filter_map(f).collect()
            };
            let graded = |r: &answer::Run| -> Vec<grade::Grade> {
                r.rows.iter().filter_map(|x| x.grade.clone()).collect()
            };
            let expected = |r: &answer::Run| {
                r.rows
                    .iter()
                    .filter(|x| answerable.contains(x.id.as_str()))
                    .count()
            };
            let agreement = |a: grade::Agreement| {
                move |r: &answer::Run| {
                    let g = graded(r);
                    ratio(g.iter().filter(|x| x.agreement == a).count(), expected(r))
                }
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                cell.describe(),
                runs.len(),
                spread(
                    &per(&|r| ratio(
                        r.rows.iter().filter(|x| x.correct_shape).count(),
                        r.rows.len()
                    )),
                    as_pct
                ),
                spread(
                    #[expect(clippy::cast_precision_loss, reason = "a count of questions")]
                    &per(&|r| Some(
                        r.rows
                            .iter()
                            .filter(|x| answerable.contains(x.id.as_str())
                                && matches!(x.outcome, answer::Outcome::Error { .. }))
                            .count() as f64
                    )),
                    |f| format!("{f:.1}")
                ),
                spread(&per(&answer::Run::recall), as_pct),
                spread(
                    #[expect(clippy::cast_precision_loss, reason = "a count of questions")]
                    &per(&|r| Some(
                        r.rows
                            .iter()
                            .filter(|x| x.first_rejection.is_some())
                            .count() as f64
                    )),
                    |f| format!("{f:.1}")
                ),
                {
                    let g: Vec<String> = runs
                        .iter()
                        .map(|r| format!("{}/{}", graded(r).len(), expected(r)))
                        .collect();
                    if g.is_empty() {
                        "-".to_owned()
                    } else {
                        g.join(", ")
                    }
                },
                spread(&per(&agreement(grade::Agreement::Agrees)), as_pct),
                spread(&per(&agreement(grade::Agreement::Partial)), as_pct),
                spread(
                    &per(&|r| {
                        let g = graded(r);
                        ratio(
                            g.iter()
                                .filter(|x| x.grounding == grade::Grounding::Follows)
                                .count(),
                            expected(r),
                        )
                    }),
                    as_pct
                ),
                spread(
                    &per(&|r| {
                        let g = graded(r);
                        ratio(
                            g.iter().map(|x| x.count(grade::Support::Supported)).sum(),
                            g.iter().map(|x| x.claims.len()).sum(),
                        )
                    }),
                    as_pct
                ),
                spread(&per(&|r| Some(r.total_usd)), as_usd),
                spread(
                    &per(&|r| {
                        let g = graded(r);
                        (!g.is_empty()).then(|| g.iter().map(|x| x.usd).sum())
                    }),
                    as_usd
                ),
                spread(
                    &per(&|r| mean_secs(r.rows.iter().map(|x| x.elapsed_ms))),
                    as_secs
                ),
            );
        }
        s.push('\n');
    }
    print!("{s}");
    let path = dir.join("summary.md");
    std::fs::write(&path, &s).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> anyhow::Result<Plan> {
        Plan::from_raw(toml::from_str(text)?)
    }

    #[test]
    fn a_grid_expands_to_every_model_and_effort() -> anyhow::Result<()> {
        let p = plan(
            r#"
label = "t"
max_usd = 10
[extract]
models = ["claude-opus-5-5", "claude-haiku-5-5"]
efforts = ["low", "xhigh"]
[synth]
models = ["claude-sonnet-5-5"]
efforts = ["medium"]
[[pipeline]]
name = "default"
extract = { model = "claude-opus-5-5", effort = "low" }
synth = { model = "claude-opus-5-5", effort = "medium" }
"#,
        )?;
        let names: Vec<&str> = p.cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "extract-opus-5-5-low",
                "extract-opus-5-5-xhigh",
                "extract-haiku-5-5-low",
                "extract-haiku-5-5-xhigh",
                "synth-sonnet-5-5-medium",
                "pipeline-default"
            ]
        );
        assert!(
            p.cells
                .iter()
                .find(|c| c.kind == Kind::Synth)
                .is_some_and(|c| c.extract.is_none())
        );
        Ok(())
    }

    #[test]
    fn a_generated_config_names_each_stage() -> anyhow::Result<()> {
        let p = plan(
            r#"
label = "t"
max_usd = 10
[[pipeline]]
name = "budget"
extract = { model = "claude-sonnet-5-5", effort = "low" }
synth = { model = "claude-sonnet-5-5", effort = "xhigh" }
"#,
        )?;
        let cell = p.cells.first().ok_or_else(|| anyhow::anyhow!("no cell"))?;
        let extract = cell
            .extract
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no extract"))?;
        let doc: toml::Table = toml::from_str(&p.config_text(&extract, &cell.synth)?)?;
        let stage = |name: &str, key: &str| {
            doc.get("models")
                .and_then(|m| m.get(name))
                .and_then(|s| s.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        assert_eq!(stage("extract", "effort").as_deref(), Some("low"));
        assert_eq!(stage("synth", "effort").as_deref(), Some("xhigh"));
        assert_eq!(stage("synth", "provider").as_deref(), Some("anthropic"));
        assert_eq!(stage("embed", "model").as_deref(), Some("voyage-3.5"));
        assert!(
            doc.get("providers")
                .and_then(|p| p.get("anthropic"))
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn bad_plans_are_refused() {
        for (text, why) in [
            ("label = \"t\"\nmax_usd = 1", "no cells"),
            (
                "label = \"T x\"\nmax_usd = 1\n[synth]\nmodels=[\"m\"]\nefforts=[\"low\"]",
                "label",
            ),
            (
                "label = \"t\"\nmax_usd = 0\n[synth]\nmodels=[\"m\"]\nefforts=[\"low\"]",
                "cap",
            ),
            (
                "label = \"t\"\nmax_usd = 1\n[synth]\nmodels=[]\nefforts=[\"low\"]",
                "empty",
            ),
            (
                "label = \"t\"\nmax_usd = 1\n[synth]\nmodels=[\"m\"]\nefforts=[\"huge\"]",
                "effort",
            ),
            (
                "label = \"t\"\nmax_usd = 1\nrepeat = 2\n[synth]\nmodels=[\"m\"]\nefforts=[\"low\"]",
                "unknown key",
            ),
        ] {
            assert!(plan(text).is_err(), "{why} should be refused");
        }
    }

    #[test]
    fn the_ledger_adds_up_across_writes() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("sweep-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        assert!(read_ledger(&dir)?.is_empty());
        for (run, usd, cut) in [("a-r1", 1.25, false), ("b-r1", 0.5, true)] {
            record(
                &dir,
                &Entry {
                    run: run.into(),
                    what: "answer".into(),
                    usd,
                    cut,
                },
            )?;
        }
        let ledger = read_ledger(&dir)?;
        std::fs::remove_dir_all(&dir)?;
        assert!((ledger.iter().map(|e| e.usd).sum::<f64>() - 1.75).abs() < 1e-9);
        assert_eq!(
            ledger
                .iter()
                .filter(|e| e.cut)
                .map(|e| e.run.as_str())
                .collect::<Vec<_>>(),
            ["b-r1"]
        );
        Ok(())
    }

    #[test]
    fn runs_in_flight_never_hold_more_than_the_sweep_cap() {
        let mut books = Books {
            max_usd: 10.0,
            spent: 3.0,
            reserved: 0.0,
            in_flight: 0,
        };
        assert_eq!(books.reserve(4.0), Grant::Cap(4.0));
        assert_eq!(
            books.reserve(4.0),
            Grant::Wait,
            "3 free while a run holds 4"
        );
        books.settle(4.0, 1.0);
        assert_eq!(
            books.reserve(4.0),
            Grant::Cap(4.0),
            "the unspent 3 came back"
        );
        assert_eq!(books.reserve(4.0), Grant::Wait);
        books.settle(4.0, 4.0);
        assert_eq!(
            books.reserve(4.0),
            Grant::Cap(2.0),
            "the last of it, nothing in flight"
        );
        books.settle(2.0, 2.0);
        assert_eq!(books.reserve(4.0), Grant::Exhausted);
    }

    #[test]
    fn neighbouring_jobs_wait_on_different_models() -> anyhow::Result<()> {
        let p = plan(
            r#"
label = "t"
max_usd = 10
[synth]
models = ["claude-opus-5-5", "claude-haiku-5-5"]
efforts = ["low", "high"]
"#,
        )?;
        let jobs = p
            .cells
            .iter()
            .map(|cell| Job {
                cell: cell.clone(),
                label: cell.name.clone(),
                out: PathBuf::new(),
                config: PathBuf::new(),
            })
            .collect();
        let order: Vec<String> = interleave(jobs).into_iter().map(|j| j.label).collect();
        assert_eq!(
            order,
            [
                "synth-opus-5-5-low",
                "synth-haiku-5-5-low",
                "synth-opus-5-5-high",
                "synth-haiku-5-5-high"
            ]
        );
        Ok(())
    }

    #[test]
    fn spread_shows_the_range_only_over_repeats() {
        assert_eq!(spread(&[0.5], as_pct), "50%");
        assert_eq!(spread(&[0.5, 0.7], as_pct), "60% (50%–70%)");
        assert_eq!(spread(&[], as_pct), "-");
    }
}
