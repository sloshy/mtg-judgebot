//! `eval grade`: a model grades each stored answer against its citations and
//! the gold reference (D28). It is a score beside citation recall, never a
//! gate: D20 rejected a model's opinion among the answer path's checks.
//!
//! The grader reads only what the run file holds: the question, the answer,
//! each citation with its quote, and the reference answer from the current
//! gold file. It splits the answer into claims and marks each as supported by
//! the quoted words alone, unsupported, or contradicted; says whether the
//! conclusion agrees with the reference; and lists wrong remarks. The grades
//! are written back into the run file, row by row, so a run stopped by the
//! spend cap resumes where it stopped.

use std::{fmt::Write as _, path::PathBuf, sync::Arc};

use anyhow::Context as _;
use judge_bot::config::Config;
use judge_llm::{
    ChatModel, ChatRequest, ChatResponse, Effort, LOG_TEXT_CHARS, LlmError, OutputSchema,
    SpendMeter, Stop, TextBlock, ToolChoice, Turn, needs_schema_in_prompt, schema_block,
    strip_json_fence, truncate_for_log,
};
use nonempty::NonEmpty;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    answer::{Outcome, Row, Run},
    gold::Gold,
};

/// The grader's instructions.
const SYSTEM_PROMPT: &str = include_str!("prompts/grade_system.md");

/// Output ceiling: a claim list for a long answer plus the reason.
const MAX_TOKENS: u32 = 6000;

/// Whether one claim follows from the quoted text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// A quote states it, or it follows directly from the quotes.
    Supported,
    /// No quote states it, though it may be true.
    Unsupported,
    /// A quote or the reference says otherwise.
    Contradicted,
}

/// Whether the answer's ruling follows from what it quotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Grounding {
    /// Every step from the quotes to the ruling is in the quoted words.
    Follows,
    /// The ruling rests on a step no quote states.
    Gap,
    /// The quotes do not support the ruling.
    Unfounded,
}

impl Grounding {
    /// The table's short form.
    const fn short(self) -> &'static str {
        match self {
            Self::Follows => "ok",
            Self::Gap => "gap",
            Self::Unfounded => "NO",
        }
    }
}

/// How the answer's conclusion compares with the reference's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Agreement {
    /// The same outcome on every point the question asks about.
    Agrees,
    /// The main outcome agrees; a point asked about is missing or different.
    Partial,
    /// The main outcome differs.
    Disagrees,
}

impl Agreement {
    /// The table's short form.
    const fn short(self) -> &'static str {
        match self {
            Self::Agrees => "agree",
            Self::Partial => "part",
            Self::Disagrees => "DIS",
        }
    }
}

/// One claim the answer makes about the game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    /// The claim, in the answer's words or close to them.
    pub claim: String,
    /// Whether the quoted text supports it.
    pub support: Support,
}

/// What the grader writes. Key order is the order it writes in: the claims,
/// then each grade after its reason.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GradeOutput {
    claims: Vec<Claim>,
    grounding_reason: String,
    grounding: Grounding,
    agreement_reason: String,
    agreement: Agreement,
    wrong_remarks: Vec<String>,
}

/// One answer's grade, as stored in the run file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    /// `provider/model` of the grader, as billed.
    pub grader: String,
    /// The answer's claims. A verdict always makes at least one.
    pub claims: NonEmpty<Claim>,
    /// Why the grounding grade is what it is, naming any missing step.
    pub grounding_reason: String,
    /// Whether the ruling follows from the quotes.
    pub grounding: Grounding,
    /// Why the agreement grade is what it is.
    pub agreement_reason: String,
    /// The conclusion against the reference.
    pub agreement: Agreement,
    /// Statements a quote or the reference shows to be false, in the answer's words.
    pub wrong_remarks: Vec<String>,
    /// What grading this answer cost.
    pub usd: f64,
    /// The reference answer it was graded against. A grade whose reference
    /// is no longer the gold file's is stale: `grade` makes it again, and
    /// `show`/`rescore` leave it out. Empty in a grade from before it was
    /// recorded, which is therefore stale.
    #[serde(default)]
    pub reference: String,
}

impl Grade {
    /// Claims with this support.
    #[must_use]
    pub fn count(&self, support: Support) -> usize {
        self.claims.iter().filter(|c| c.support == support).count()
    }

    /// The table cell: agreement then grounding (`agree ok`, `part gap`),
    /// with `!` when a claim is contradicted or a remark is wrong.
    #[must_use]
    pub fn cell(&self) -> String {
        let flag = if self.count(Support::Contradicted) > 0 || !self.wrong_remarks.is_empty() {
            "!"
        } else {
            ""
        };
        format!(
            "{} {}{flag}",
            self.agreement.short(),
            self.grounding.short()
        )
    }
}

/// Parsed `grade` flags.
#[derive(Debug)]
pub struct Options {
    /// The run file, read and rewritten in place.
    pub run: PathBuf,
    /// Gold file the references come from.
    pub gold: PathBuf,
    /// Spend cap for the grading.
    pub max_usd: f64,
    /// A `judge.toml` whose synthesis model grades (`--config`), ahead of `JUDGE_CONFIG`.
    pub config: Option<PathBuf>,
    /// Grade rows that already have a grade again.
    pub force: bool,
}

impl Options {
    /// Parse `grade <run.json> [--max-usd X] [--config p] [--gold p] [--force]`.
    ///
    /// # Errors
    /// On an unknown flag, a missing value or a missing run file argument.
    pub fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let (mut run, mut gold, mut config, mut max_usd, mut force) =
            (None, None, None, 1.00f64, false);
        while let Some(a) = args.next() {
            let mut val = || {
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
            };
            match a.as_str() {
                "--gold" => gold = Some(PathBuf::from(val()?)),
                "--config" => config = Some(PathBuf::from(val()?)),
                "--max-usd" => max_usd = val()?.parse().context("--max-usd must be a number")?,
                "--force" => force = true,
                flag if flag.starts_with("--") => anyhow::bail!("unknown flag {flag}"),
                path if run.is_none() => run = Some(PathBuf::from(path)),
                extra => anyhow::bail!("unexpected argument {extra}"),
            }
        }
        if !(max_usd.is_finite() && max_usd >= 0.0) {
            anyhow::bail!("--max-usd must be a finite non-negative number");
        }
        Ok(Self {
            run: run.ok_or_else(|| anyhow::anyhow!("grade needs a run file"))?,
            gold: gold.unwrap_or_else(crate::gold::default_path),
            max_usd,
            config,
            force,
        })
    }
}

/// What one grading pass did.
#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    /// Rows graded now.
    pub graded: usize,
    /// Rows the grader failed on (left ungraded, logged).
    pub failed: usize,
    /// Whether the spend cap stopped the pass.
    pub capped: bool,
    /// The provider error that stopped the pass, when one did. A failed
    /// request would fail the same way on every row.
    pub stopped: Option<String>,
    /// What the pass spent, failed replies included.
    pub spent_usd: f64,
}

impl Pass {
    /// Every gradable row has a current grade: nothing failed or stopped.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed == 0 && !self.capped && self.stopped.is_none()
    }
}

/// `eval grade`: grade the run file's ungraded answers and write them back.
///
/// # Errors
/// Reading or parsing the run or gold file, building the model, or writing
/// the run file. A grader failure on one row is logged and counted.
pub async fn run(opts: &Options) -> anyhow::Result<(Run, Pass)> {
    let raw = std::fs::read_to_string(&opts.run)
        .with_context(|| format!("reading {}", opts.run.display()))?;
    let mut run: Run =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", opts.run.display()))?;
    let gold = crate::gold::load(&opts.gold)?;
    let config = Config::load_from(opts.config.as_deref())?;
    tracing::info!("{}", config.summary());
    let models = config.models()?;
    config.probe_auth().await?;
    models.meter().set_max_spend_usd(opts.max_usd)?;
    let out = opts.run.clone();
    let pass = grade_rows(
        &models.synth(),
        models.meter(),
        &mut run,
        &gold,
        opts.force,
        |run| write_run(&out, run),
    )
    .await?;
    // The table as `rescore` prints it, so a stale grade a stopped pass
    // left behind is not counted.
    print!("{}", crate::answer::rescore(&opts.run, &opts.gold)?);
    println!("{}", pass_line(&pass));
    Ok((run, pass))
}

/// Write the run file whole: a temporary file renamed over it, so an
/// interrupted write leaves the previous file.
///
/// # Errors
/// Serializing or writing.
pub fn write_run(path: &std::path::Path, run: &Run) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(run)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// The line after a pass.
#[must_use]
pub fn pass_line(pass: &Pass) -> String {
    let stop = match (&pass.stopped, pass.capped) {
        (Some(e), _) => format!("; stopped by a provider error: {e}"),
        (None, true) => "; stopped at the spend cap, run again to resume".to_owned(),
        (None, false) => String::new(),
    };
    format!(
        "graded {} now, {} failed, ${:.4} spent{stop}",
        pass.graded, pass.failed, pass.spent_usd
    )
}

/// Grade every gradable row of `run` with no current grade, calling `saved`
/// after each new grade. A row is gradable when it holds a verdict on a
/// question the gold file says the bot should answer; its grade is current
/// when it was made against the gold file's reference. `force` first clears
/// every grade, so a pass the cap stops resumes from there.
///
/// # Errors
/// Only what `saved` returns. Grader failures are logged and counted.
pub async fn grade_rows(
    model: &Arc<dyn ChatModel>,
    meter: &SpendMeter,
    run: &mut Run,
    gold: &Gold,
    force: bool,
    mut saved: impl FnMut(&Run) -> anyhow::Result<()>,
) -> anyhow::Result<Pass> {
    let mut pass = Pass::default();
    let start_usd = meter.spent_usd();
    if force && run.rows.iter().any(|r| r.grade.is_some()) {
        for row in &mut run.rows {
            row.grade = None;
        }
        saved(run)?;
    }
    let quotes_cut = !run.traced;
    for i in 0..run.rows.len() {
        let Some(row) = run.rows.get(i) else { break };
        let Some(q) = gold.questions.iter().find(|q| q.id == row.id) else {
            tracing::warn!(id = %row.id, "row not in gold file; not graded");
            continue;
        };
        if !q.is_answerable()
            || row
                .grade
                .as_ref()
                .is_some_and(|g| g.reference == q.expected_answer)
        {
            continue;
        }
        let Some(req) = build_request(model.capabilities(), row, &q.expected_answer, quotes_cut)
        else {
            continue;
        };
        let usd0 = meter.spent_usd();
        let result = model.complete(&req).await;
        let usd = meter.spent_usd() - usd0;
        let graded = match result {
            Ok(resp) => parse(&resp, model.provider(), usd, &q.expected_answer),
            Err(LlmError::SpendCapExceeded { .. }) => {
                tracing::warn!("spend cap reached; stopping the grading");
                pass.capped = true;
                break;
            }
            Err(e) => {
                tracing::warn!(id = %q.id, "grading request failed, stopping: {e}");
                pass.failed += 1;
                pass.stopped = Some(e.to_string());
                break;
            }
        };
        match graded {
            Ok(grade) => {
                tracing::info!(id = %q.id, grade = %grade.cell(), usd = format_args!("{usd:.4}"), "graded");
                if let Some(row) = run.rows.get_mut(i) {
                    row.grade = Some(grade);
                }
                pass.graded += 1;
                saved(run)?;
            }
            Err(e) => {
                tracing::warn!(id = %q.id, "grading failed: {e:#}");
                pass.failed += 1;
            }
        }
    }
    pass.spent_usd = meter.spent_usd() - start_usd;
    Ok(pass)
}

/// The user turn for one row: question, reference, answer, citations.
/// `None` for a row with no verdict.
#[must_use]
pub fn user_turn(row: &Row, reference: &str, quotes_cut: bool) -> Option<String> {
    let Outcome::Verdict {
        answer, citations, ..
    } = &row.outcome
    else {
        return None;
    };
    let mut s = String::new();
    let _ = writeln!(s, "## Question\n{}\n", row.question.trim());
    let _ = writeln!(s, "## Reference answer\n{}\n", reference.trim());
    let _ = writeln!(s, "## Bot answer\n{}\n", answer.trim());
    s.push_str("## Bot citations\n");
    if citations.is_empty() {
        s.push_str("(none)\n");
    }
    for (n, c) in citations.iter().enumerate() {
        let _ = writeln!(s, "{}. {c}", n + 1);
    }
    if quotes_cut {
        s.push_str(
            "\nThis run file shortened long quotes, so a quote may end early. Do not mark a \
             claim unsupported only because its quote is cut.\n",
        );
    }
    Some(s)
}

/// One grading request: the cached rubric, the row in the user turn, the
/// `GradeOutput` schema as structured output (also in the user turn when the
/// backend cannot enforce it). `None` for a row with no verdict.
#[must_use]
pub fn build_request(
    caps: judge_llm::Capabilities,
    row: &Row,
    reference: &str,
    quotes_cut: bool,
) -> Option<ChatRequest> {
    let output = OutputSchema::of::<GradeOutput>();
    let mut user = vec![TextBlock::plain(user_turn(row, reference, quotes_cut)?)];
    if needs_schema_in_prompt(caps) {
        user.push(schema_block(&output));
    }
    Some(ChatRequest {
        max_tokens: MAX_TOKENS,
        system: vec![TextBlock::cached(SYSTEM_PROMPT.trim_end())],
        turns: vec![Turn::User(user)],
        tools: vec![],
        tool_choice: ToolChoice::None,
        output: Some(output),
        effort: Some(Effort::Medium),
        thinking: false,
        fallbacks: None,
    })
}

/// The grader's reply as a [`Grade`].
///
/// # Errors
/// A refusal, truncation, another stop reason, JSON that does not match the
/// schema, or an empty claim list.
pub fn parse(
    resp: &ChatResponse,
    provider: &str,
    usd: f64,
    reference: &str,
) -> anyhow::Result<Grade> {
    match &resp.stop {
        Stop::EndTurn => {}
        Stop::Refusal(r) => anyhow::bail!("grader refused: {r:?}"),
        Stop::MaxTokens => anyhow::bail!("grader truncated at max_tokens"),
        other => anyhow::bail!("grader stopped with {other:?}"),
    }
    let text = resp
        .last_text()
        .ok_or_else(|| anyhow::anyhow!("grader response had no text block"))?;
    let text = strip_json_fence(text);
    let out: GradeOutput = serde_json::from_str(text).with_context(|| {
        format!(
            "grader JSON did not match the schema: {}",
            truncate_for_log(text, LOG_TEXT_CHARS)
        )
    })?;
    let claims =
        NonEmpty::from_vec(out.claims).ok_or_else(|| anyhow::anyhow!("grader listed no claims"))?;
    Ok(Grade {
        grader: format!("{provider}/{}", resp.model),
        claims,
        grounding_reason: out.grounding_reason,
        grounding: out.grounding,
        agreement_reason: out.agreement_reason,
        agreement: out.agreement,
        wrong_remarks: out.wrong_remarks,
        usd,
        reference: reference.to_owned(),
    })
}

/// The run's grade totals for the table, `None` when nothing is graded.
#[must_use]
pub fn totals_line(run: &Run) -> Option<String> {
    let grades: Vec<&Grade> = run.rows.iter().filter_map(|r| r.grade.as_ref()).collect();
    if grades.is_empty() {
        return None;
    }
    let agreeing = |a| grades.iter().filter(|g| g.agreement == a).count();
    let grounded = |a| grades.iter().filter(|g| g.grounding == a).count();
    let claims = |s| grades.iter().map(|g| g.count(s)).sum::<usize>();
    let total: usize = grades.iter().map(|g| g.claims.len()).sum();
    let remarks: usize = grades.iter().map(|g| g.wrong_remarks.len()).sum();
    let mut models: Vec<&str> = grades.iter().map(|g| g.grader.as_str()).collect();
    models.sort_unstable();
    models.dedup();
    let usd: f64 = grades.iter().map(|g| g.usd).sum();
    Some(format!(
        "graded {} answers by {}: {} agree, {} partial, {} disagree; ruling follows from the quotes {}, gap {}, unfounded {}; claims supported {}/{total}, {} contradicted; {remarks} wrong remarks; grading ${usd:.4}",
        grades.len(),
        models.join(", "),
        agreeing(Agreement::Agrees),
        agreeing(Agreement::Partial),
        agreeing(Agreement::Disagrees),
        grounded(Grounding::Follows),
        grounded(Grounding::Gap),
        grounded(Grounding::Unfounded),
        claims(Support::Supported),
        claims(Support::Contradicted),
    ))
}

/// The grade under one answer in `show`.
#[must_use]
pub fn show_block(grade: &Grade) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "grade ({}): {:?}. {}\ngrounding: {:?}, claims supported {}/{}. {}",
        grade.grader,
        grade.agreement,
        grade.agreement_reason.trim(),
        grade.grounding,
        grade.count(Support::Supported),
        grade.claims.len(),
        grade.grounding_reason.trim()
    );
    for c in grade
        .claims
        .iter()
        .filter(|c| c.support != Support::Supported)
    {
        let _ = writeln!(s, "  {:?}: {}", c.support, c.claim);
    }
    for w in &grade.wrong_remarks {
        let _ = writeln!(s, "  wrong remark: {w}");
    }
    s
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use judge_llm::{AssistantTurn, Backend, Capabilities, Metered, StructuredOutput, Usage};

    use super::*;
    use crate::{answer::RunModels, score::Recall};

    fn row(id: &str, outcome: Outcome) -> Row {
        Row {
            id: id.to_owned(),
            question: "does it?".to_owned(),
            expected_answer: "stored reference".to_owned(),
            expected_source: "CR".to_owned(),
            expected_rule_ids: vec![],
            supporting_rule_ids: vec![],
            supporting_cited: vec![],
            outcome,
            cited_rule_ids: vec![],
            recall: Recall {
                hit: vec![],
                missed: vec![],
            },
            any_expected_cited: false,
            cites: crate::score::CiteCounts::default(),
            source_ok: true,
            correct_shape: true,
            elapsed_ms: 0,
            calls: 0,
            first_rejection: None,
            stubs_dropped: 0,
            usd: 0.0,
            grade: None,
        }
    }

    fn verdict() -> Outcome {
        Outcome::Verdict {
            answer: "Yes, it does.".to_owned(),
            confidence: "high".to_owned(),
            citations: vec![
                "rule 614.6: \"If an event is replaced, it never happens.\"".to_owned(),
            ],
            category: "replacement_effects".to_owned(),
            source: "cr".to_owned(),
            cr_version: "20260819".to_owned(),
        }
    }

    fn error() -> Outcome {
        Outcome::Error {
            variant: "OutOfScope".to_owned(),
            message: "tournament".to_owned(),
        }
    }

    fn run_of(rows: Vec<Row>) -> Run {
        Run {
            label: "t".to_owned(),
            gold: "g".to_owned(),
            max_usd: 1.0,
            models: Some(RunModels {
                extract: "a/b".to_owned(),
                synth: "a/b".to_owned(),
                synth_effort: None,
            }),
            traced: true,
            rows,
            total_usd: 0.0,
            total_calls: 0,
        }
    }

    fn gold() -> anyhow::Result<Gold> {
        Ok(serde_yaml_ng::from_str(
            "questions:\n\
             - {id: a, question: q, source: CR, expected_answer: current reference}\n\
             - {id: b, question: q, source: CR, expected_answer: other}\n\
             - {id: c, question: q, source: Tournament, expected_answer: decline}\n",
        )?)
    }

    const GOOD: &str = r#"{"claims":[{"claim":"it does","support":"supported"},{"claim":"always","support":"contradicted"}],"grounding_reason":"a step is missing","grounding":"gap","agreement_reason":"same outcome","agreement":"agrees","wrong_remarks":["always"]}"#;

    fn response(text: &str, stop: Stop) -> ChatResponse {
        ChatResponse {
            text: vec![text.to_owned()],
            tool_calls: vec![],
            stop,
            usage: Usage {
                input: 1000,
                output: 200,
                cache_read: 0,
                cache_write: 0,
            },
            model: "claude-opus-5-5".to_owned(),
            assistant: AssistantTurn {
                backend: "fake",
                raw: serde_json::Value::Null,
            },
        }
    }

    #[test]
    fn the_user_turn_carries_the_reference_and_every_quote() -> anyhow::Result<()> {
        let r = row("a", verdict());
        let turn =
            user_turn(&r, "current reference", false).ok_or_else(|| anyhow::anyhow!("no turn"))?;
        assert!(turn.contains("## Reference answer\ncurrent reference"));
        assert!(turn.contains("1. rule 614.6: \"If an event is replaced, it never happens.\""));
        assert!(!turn.contains("shortened"));
        assert!(
            user_turn(&r, "x", true).is_some_and(|t| t.contains("shortened long quotes")),
            "an untraced run says its quotes may be cut"
        );
        assert!(user_turn(&row("a", error()), "x", false).is_none());
        Ok(())
    }

    #[test]
    fn the_schema_goes_in_the_user_turn_only_when_the_backend_cannot_enforce_it()
    -> anyhow::Result<()> {
        let mut caps = Capabilities {
            structured_output: StructuredOutput::Enforced,
            strict_tools: true,
            effort: true,
            cache_hints: true,
            refusal_fallbacks: false,
        };
        let r = row("a", verdict());
        let enforced =
            build_request(caps, &r, "x", false).ok_or_else(|| anyhow::anyhow!("none"))?;
        caps.structured_output = StructuredOutput::PromptOnly;
        let prompted =
            build_request(caps, &r, "x", false).ok_or_else(|| anyhow::anyhow!("none"))?;
        let blocks = |req: &ChatRequest| match req.turns.first() {
            Some(Turn::User(b)) => b.len(),
            _ => 0,
        };
        assert_eq!((blocks(&enforced), blocks(&prompted)), (1, 2));
        assert_eq!(
            enforced.system, prompted.system,
            "the rubric is the same bytes"
        );
        Ok(())
    }

    #[test]
    fn a_reply_parses_into_a_grade_and_bad_ones_do_not() -> anyhow::Result<()> {
        let g = parse(&response(GOOD, Stop::EndTurn), "anthropic", 0.03, "r")?;
        assert_eq!(g.grader, "anthropic/claude-opus-5-5");
        assert_eq!(
            (g.count(Support::Supported), g.count(Support::Contradicted)),
            (1, 1)
        );
        assert_eq!(
            (g.grounding, g.cell().as_str()),
            (Grounding::Gap, "agree gap!")
        );
        let empty = r#"{"claims":[],"grounding_reason":"r","grounding":"follows","agreement_reason":"r","agreement":"agrees","wrong_remarks":[]}"#;
        assert!(parse(&response(empty, Stop::EndTurn), "a", 0.0, "r").is_err());
        assert!(parse(&response(GOOD, Stop::MaxTokens), "a", 0.0, "r").is_err());
        let extra = GOOD.replacen('{', r#"{"score":5,"#, 1);
        assert!(parse(&response(&extra, Stop::EndTurn), "a", 0.0, "r").is_err());
        let fenced = format!("```json\n{GOOD}\n```");
        assert!(parse(&response(&fenced, Stop::EndTurn), "a", 0.0, "r").is_ok());
        Ok(())
    }

    #[test]
    fn options_need_a_run_file() -> anyhow::Result<()> {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let o = Options::parse(args(&["r.json", "--force", "--max-usd", "0.5"]).into_iter())?;
        assert_eq!(o.run, PathBuf::from("r.json"));
        assert!(o.force && (o.max_usd - 0.5).abs() < f64::EPSILON);
        assert!(Options::parse(args(&["--force"]).into_iter()).is_err());
        assert!(Options::parse(args(&["a.json", "b.json"]).into_iter()).is_err());
        assert!(Options::parse(args(&["a.json", "--max-usd", "-1"]).into_iter()).is_err());
        assert!(Options::parse(args(&["a.json", "--bogus"]).into_iter()).is_err());
        Ok(())
    }

    /// Replies with [`GOOD`] and records the user turns it was sent.
    struct Fake {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait]
    impl Backend for Fake {
        async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
            if let Some(Turn::User(blocks)) = req.turns.first()
                && let Some(b) = blocks.first()
                && let Ok(mut seen) = self.seen.lock()
            {
                seen.push(b.text.clone());
            }
            if self.fail {
                return Err(LlmError::Request("bad schema".to_owned()));
            }
            Ok(response(GOOD, Stop::EndTurn))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                structured_output: StructuredOutput::Enforced,
                strict_tools: true,
                effort: true,
                cache_hints: true,
                refusal_fallbacks: false,
            }
        }
        fn provider(&self) -> &'static str {
            "anthropic"
        }
        fn model(&self) -> &'static str {
            "claude-opus-5-5"
        }
    }

    #[tokio::test]
    async fn only_ungraded_answers_to_answerable_questions_are_graded() -> anyhow::Result<()> {
        let meter = SpendMeter::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fake = Fake {
            seen: Arc::clone(&seen),
            fail: false,
        };
        let model: Arc<dyn ChatModel> = Arc::new(Metered::new(fake, meter.clone())?);
        let mut done = row("b", verdict());
        done.grade = Some(parse(&response(GOOD, Stop::EndTurn), "x", 0.0, "other")?);
        let mut run = run_of(vec![
            row("a", verdict()),
            done,
            row("c", verdict()),
            row("a", error()),
            row("missing", verdict()),
        ]);
        let mut saves = 0;
        let pass = grade_rows(&model, &meter, &mut run, &gold()?, false, |_| {
            saves += 1;
            Ok(())
        })
        .await?;
        assert_eq!(
            (pass.graded, pass.failed, pass.complete()),
            (1, 0, true),
            "b's grade is current, c is out of scope, an error has no verdict"
        );
        assert_eq!(saves, 1, "saved after each new grade");
        let seen = seen.lock().map(|s| s.clone()).unwrap_or_default();
        assert_eq!(seen.len(), 1);
        assert!(
            seen.first()
                .is_some_and(|t| t.contains("current reference")),
            "the reference comes from the current gold file, not the run file"
        );
        assert!(run.rows.first().is_some_and(|r| r.grade.is_some()));
        assert!(run.rows.get(2).is_some_and(|r| r.grade.is_none()));

        if let Some(g) = run.rows.get_mut(1).and_then(|r| r.grade.as_mut()) {
            g.reference = "an older reference".to_owned();
        }
        let pass = grade_rows(&model, &meter, &mut run, &gold()?, false, |_| Ok(())).await?;
        assert_eq!(pass.graded, 1, "a stale grade is made again");

        let mut cleared = 0;
        let pass = grade_rows(&model, &meter, &mut run, &gold()?, true, |r| {
            cleared += usize::from(r.rows.iter().all(|r| r.grade.is_none()));
            Ok(())
        })
        .await?;
        assert_eq!(pass.graded, 2, "--force regrades the graded rows too");
        assert_eq!(cleared, 1, "--force saves the cleared file first");
        assert!(totals_line(&run).is_some_and(|l| l.starts_with("graded 2 answers")));
        assert!(totals_line(&run_of(vec![row("a", verdict())])).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_provider_error_stops_the_pass() -> anyhow::Result<()> {
        let meter = SpendMeter::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fake = Fake {
            seen: Arc::clone(&seen),
            fail: true,
        };
        let model: Arc<dyn ChatModel> = Arc::new(Metered::new(fake, meter.clone())?);
        let mut run = run_of(vec![row("a", verdict()), row("b", verdict())]);
        let pass = grade_rows(&model, &meter, &mut run, &gold()?, false, |_| Ok(())).await?;
        assert_eq!((pass.failed, pass.complete()), (1, false));
        assert!(pass.stopped.is_some());
        assert_eq!(seen.lock().map(|s| s.len()).unwrap_or_default(), 1);
        Ok(())
    }
}
