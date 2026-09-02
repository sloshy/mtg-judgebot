//! `judge-cli` — every agent operation as a subcommand printing JSON.
//!
//! ```text
//! judge-cli judge <question> [--thread T] [--pin "span=Full Name"]...   # built-in pipeline (API spend;
//!                                                                       #  own JUDGE_MAX_USD per invocation)
//! judge-cli begin <question> [--thread T]      # start a session; prints the extraction prompt
//! judge-cli prompt <session>                   # re-read the current step's prompt
//! judge-cli status <session>
//! judge-cli extract <session> <file|->         # submit the extraction JSON; prints the synthesis prompt
//! judge-cli rules <session> <id>...            # the one lookup_rules round
//! judge-cli verdict <session> <file|-> [--persist]   # submit the verdict JSON
//! judge-cli persist <session>
//! judge-cli card <name>                        # resolve a name (never guesses)
//! judge-cli card-info <oracle-uuid>            # faces, rulings, notes
//! judge-cli get-rules <id>...
//! judge-cli search <query> [--limit N]
//! judge-cli glossary <term>
//! ```
//!
//! Flags may appear anywhere after the subcommand; `--` ends them so a
//! question may itself start with `--`. Every reply is one JSON document on
//! stdout, `kind`-tagged where the outcome varies (`ready` / `ambiguous` /
//! `rejected` / …). A failure the caller cannot act on is
//! `{"kind":"error","message":…}` with exit status 1. Environment as for
//! `judge-mcp`; `RUST_LOG` controls stderr logging. Each invocation is its
//! own process: the spend cap and concurrency of `judge` are per invocation.

use std::{io::Read as _, path::Path, process::ExitCode};

use anyhow::{Context as _, Result};
use judge_agent::{
    Toolbox,
    ops::{
        BeginInput, CardInput, ExtractionInput, IdsInput, JudgeInput, LookupInput, NameInput, Pin, SearchInput,
        SessionInput, TermInput, VerdictInput,
    },
};
use judge_bot::{
    session::{AgentThread, SessionId},
    synth::Harness,
};
use judge_core::{CardId, RuleId};
use serde::Serialize;

const USAGE: &str = "usage: judge-cli <judge <question> [--thread T] [--pin span=Name]... \
| begin <question> [--thread T] | prompt <session> | status <session> \
| extract <session> <file|-> | rules <session> <id>... | verdict <session> <file|-> [--persist] \
| persist <session> | card <name> | card-info <uuid> | get-rules <id>... | search <query> [--limit N] \
| glossary <term>>";

#[derive(Debug)]
enum Command {
    Judge(JudgeInput),
    Begin(BeginInput),
    Prompt(SessionInput),
    Status(SessionInput),
    Extract { session: SessionId, input: String },
    ExtractParsed { session: SessionId, extraction: judge_core::Extraction },
    Rules(LookupInput),
    Verdict { session: SessionId, input: String, persist: bool },
    VerdictParsed { session: SessionId, verdict: judge_core::Verdict<judge_core::Unvalidated>, persist: bool },
    Persist(SessionInput),
    Card(NameInput),
    CardInfo(CardInput),
    GetRules(IdsInput),
    Search(SearchInput),
    Glossary(TermInput),
}

/// Flags this CLI knows, with whether each takes a value. Anything else that
/// starts with `--` is an error rather than a positional, so a typo cannot
/// become a question.
const FLAGS: &[(&str, bool)] = &[("--thread", true), ("--limit", true), ("--pin", true), ("--persist", false)];

/// The arguments after the subcommand, split into positionals and flags.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    positional: Vec<String>,
    thread: Option<String>,
    limit: Option<String>,
    pins: Vec<String>,
    persist: bool,
}

/// One pass over the tokens: a known flag consumes its value, `--` ends flag
/// parsing (so a question may start with `--`), everything else is positional.
fn split_args(tokens: Vec<String>) -> Result<Args> {
    let mut out = Args::default();
    let mut it = tokens.into_iter();
    let mut literal = false;
    while let Some(t) = it.next() {
        if literal || !t.starts_with("--") {
            out.positional.push(t);
            continue;
        }
        if t == "--" {
            literal = true;
            continue;
        }
        let Some((name, takes_value)) = FLAGS.iter().find(|(n, _)| *n == t) else {
            anyhow::bail!("unknown flag {t:?}; {USAGE}");
        };
        let value = if *takes_value {
            Some(it.next().ok_or_else(|| anyhow::anyhow!("{name} needs a value"))?)
        } else {
            None
        };
        match (*name, value) {
            ("--thread", Some(v)) => out.thread = Some(v),
            ("--limit", Some(v)) => out.limit = Some(v),
            ("--pin", Some(v)) => out.pins.push(v),
            ("--persist", None) => out.persist = true,
            _ => anyhow::bail!("{USAGE}"),
        }
    }
    Ok(out)
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let cmd = args.next().ok_or_else(|| anyhow::anyhow!("{USAGE}"))?;
    let a = split_args(args.collect())?;
    let positional = |n: usize| -> Result<&String> { a.positional.get(n).ok_or_else(|| anyhow::anyhow!("{USAGE}")) };
    let thread = || -> Result<Option<AgentThread>> {
        a.thread.as_deref().map(|t| t.parse::<AgentThread>().map_err(|e| anyhow::anyhow!("--thread: {e}"))).transpose()
    };
    let session = |n: usize| -> Result<SessionId> {
        let raw = positional(n)?;
        raw.parse().with_context(|| format!("session id {raw:?} is not a uuid"))
    };
    let rule_ids = |from: usize| -> Result<Vec<RuleId>> {
        let ids = a.positional.get(from..).unwrap_or_default();
        anyhow::ensure!(!ids.is_empty(), "{USAGE}");
        ids.iter().map(|s| RuleId::try_new(s.clone()).map_err(|e| anyhow::anyhow!("rule id {s:?}: {e}"))).collect()
    };
    let no_extra = |n: usize| -> Result<()> {
        anyhow::ensure!(a.positional.len() <= n, "unexpected argument {:?}; {USAGE}", a.positional.get(n).map_or("", String::as_str));
        Ok(())
    };
    // A flag the subcommand does not take is an error, not a silent no-op: a
    // `--pin` on `begin` or a `--persist` on `judge` would otherwise look honoured.
    let uses = |thread: bool, limit: bool, pins: bool, persist: bool| -> Result<()> {
        anyhow::ensure!(thread || a.thread.is_none(), "{cmd} does not take --thread; {USAGE}");
        anyhow::ensure!(limit || a.limit.is_none(), "{cmd} does not take --limit; {USAGE}");
        anyhow::ensure!(pins || a.pins.is_empty(), "{cmd} does not take --pin; {USAGE}");
        anyhow::ensure!(persist || !a.persist, "{cmd} does not take --persist; {USAGE}");
        Ok(())
    };
    match cmd.as_str() {
        "judge" => uses(true, false, true, false)?,
        "begin" => uses(true, false, false, false)?,
        "verdict" => uses(false, false, false, true)?,
        "search" => uses(false, true, false, false)?,
        _ => uses(false, false, false, false)?,
    }
    Ok(match cmd.as_str() {
        "judge" => {
            no_extra(1)?;
            let pins = a
                .pins
                .iter()
                .map(|p| {
                    let (span, name) = p.split_once('=').ok_or_else(|| anyhow::anyhow!("--pin takes span=Full Name, got {p:?}"))?;
                    Ok(Pin { span: span.to_owned(), name: name.to_owned() })
                })
                .collect::<Result<Vec<_>>>()?;
            Command::Judge(JudgeInput { question: positional(0)?.clone(), thread: thread()?, pins })
        }
        "begin" => {
            no_extra(1)?;
            Command::Begin(BeginInput { question: positional(0)?.clone(), thread: thread()? })
        }
        "prompt" => {
            no_extra(1)?;
            Command::Prompt(SessionInput { session: session(0)? })
        }
        "status" => {
            no_extra(1)?;
            Command::Status(SessionInput { session: session(0)? })
        }
        "extract" => {
            no_extra(2)?;
            Command::Extract { session: session(0)?, input: positional(1)?.clone() }
        }
        "rules" => Command::Rules(LookupInput { session: session(0)?, ids: rule_ids(1)? }),
        "verdict" => {
            no_extra(2)?;
            Command::Verdict { session: session(0)?, input: positional(1)?.clone(), persist: a.persist }
        }
        "persist" => {
            no_extra(1)?;
            Command::Persist(SessionInput { session: session(0)? })
        }
        "card" => {
            no_extra(1)?;
            Command::Card(NameInput { name: positional(0)?.clone() })
        }
        "card-info" => {
            no_extra(1)?;
            Command::CardInfo(CardInput { card: CardId::new(positional(0)?.parse().context("card id must be the oracle uuid")?) })
        }
        "get-rules" => Command::GetRules(IdsInput { ids: rule_ids(0)? }),
        "search" => {
            no_extra(1)?;
            Command::Search(SearchInput {
                query: positional(0)?.clone(),
                limit: a.limit.as_deref().map(|l| l.parse::<usize>().context("--limit must be an integer")).transpose()?,
            })
        }
        "glossary" => {
            no_extra(1)?;
            Command::Glossary(TermInput { term: positional(0)?.clone() })
        }
        other => anyhow::bail!("{USAGE} (got {other:?})"),
    })
}

/// A JSON document from a file path, or stdin for `-`.
fn read_json<T: serde::de::DeserializeOwned>(input: &str, what: &str) -> Result<T> {
    let text = if input == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("read stdin")?;
        s
    } else {
        std::fs::read_to_string(Path::new(input)).with_context(|| format!("read {input}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("{what} JSON did not match the schema"))
}

fn print<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

async fn run(cmd: Command) -> Result<()> {
    // Read any input file before opening a connection: a malformed file
    // should not cost a pool.
    let cmd = match cmd {
        Command::Extract { session, input } => Command::ExtractParsed { session, extraction: read_json(&input, "extraction")? },
        Command::Verdict { session, input, persist } => {
            Command::VerdictParsed { session, verdict: read_json(&input, "verdict")?, persist }
        }
        other => other,
    };
    let toolbox = Toolbox::from_env(Harness::Cli).await?;
    match cmd {
        Command::Judge(i) => print(&toolbox.judge(i).await?),
        Command::Begin(i) => print(&toolbox.begin_session(i).await?),
        Command::Prompt(i) => print(&toolbox.session_prompt(i).await?),
        Command::Status(i) => print(&toolbox.session_status(i).await?),
        Command::Extract { .. } | Command::Verdict { .. } => anyhow::bail!("unreachable: parsed above"),
        Command::ExtractParsed { session, extraction } => {
            print(&toolbox.submit_extraction(ExtractionInput { session, extraction }).await?)
        }
        Command::Rules(i) => print(&toolbox.lookup_rules(i).await?),
        Command::VerdictParsed { session, verdict, persist } => {
            print(&toolbox.submit_verdict(VerdictInput { session, verdict, persist }).await?)
        }
        Command::Persist(i) => print(&toolbox.persist_session(i).await?),
        Command::Card(i) => print(&toolbox.resolve_card(i).await?),
        Command::CardInfo(i) => print(&toolbox.card_info(i).await?),
        Command::GetRules(i) => print(&toolbox.get_rules(i).await?),
        Command::Search(i) => print(&toolbox.search_rules(i).await?),
        Command::Glossary(i) => print(&toolbox.glossary(i).await?),
    }
}

#[derive(Serialize)]
struct ErrorReply<'a> {
    kind: &'static str,
    message: &'a str,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let outcome = match parse_args(std::env::args().skip(1)) {
        Ok(cmd) => run(cmd).await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let message = format!("{e:#}");
            match serde_json::to_string_pretty(&ErrorReply { kind: "error", message: &message }) {
                Ok(j) => println!("{j}"),
                Err(_) => eprintln!("{message}"),
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &[&str]) -> Result<Command> {
        parse_args(line.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn flags_may_sit_anywhere_and_their_values_are_not_positionals() -> Result<()> {
        let Command::Search(s) = parse(&["search", "--limit", "2", "lifelink"])? else { anyhow::bail!("search") };
        assert_eq!((s.query.as_str(), s.limit), ("lifelink", Some(2)));
        let thread = AgentThread::new();
        let Command::Begin(b) = parse(&["begin", "--thread", thread.as_str(), "q?"])? else { anyhow::bail!("begin") };
        assert_eq!((b.question.as_str(), b.thread), ("q?", Some(thread.clone())));
        let Command::Judge(j) = parse(&["judge", "--thread", thread.as_str(), "does bob work?", "--pin", "bob=Dark Confidant"])?
        else {
            anyhow::bail!("judge")
        };
        assert_eq!(j.question, "does bob work?");
        assert_eq!(j.thread, Some(thread));
        assert_eq!(j.pins, vec![Pin { span: "bob".into(), name: "Dark Confidant".into() }]);
        let Command::Verdict { persist, input, .. } =
            parse(&["verdict", "--persist", "0b6a0f4e-1c5b-4a2e-9d3e-7f4c1b2a3d4e", "v.json"])?
        else {
            anyhow::bail!("verdict")
        };
        assert!(persist);
        assert_eq!(input, "v.json");
        Ok(())
    }

    #[test]
    fn mistakes_are_errors_not_questions() {
        for bad in [
            vec!["begin", "--thread"],
            vec!["begin", "--thread", "12345", "q"],
            vec!["begin", "--typo", "q"],
            vec!["begin", "q", "extra"],
            vec!["judge", "q", "--pin", "no-equals"],
            vec!["begin", "q", "--pin", "bob=Dark Confidant"],
            vec!["judge", "q", "--persist"],
            vec!["search", "q", "--thread", "agent:0b6a0f4e-1c5b-4a2e-9d3e-7f4c1b2a3d4e"],
            vec!["rules", "0b6a0f4e-1c5b-4a2e-9d3e-7f4c1b2a3d4e", "702.19", "--limit", "3"],
            vec!["search", "--limit", "x", "q"],
            vec!["rules", "0b6a0f4e-1c5b-4a2e-9d3e-7f4c1b2a3d4e"],
            vec!["prompt", "not-a-uuid"],
            vec!["nope"],
            vec![],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_double_dash_lets_a_question_start_with_dashes() -> Result<()> {
        let Command::Begin(b) = parse(&["begin", "--", "--is this a flag?"])? else { anyhow::bail!("begin") };
        assert_eq!(b.question, "--is this a flag?");
        Ok(())
    }
}
