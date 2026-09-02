//! Retire calls whose citations the current data no longer supports.
//!
//! A persisted call was admitted by [`judge_core::Verdict::validate`]: every
//! citation named a source that was in the model's context and quoted it
//! verbatim. Those citations are the call's declared dependencies on the
//! world, so the question "is this call still good?" is the question that
//! admitted it, asked again against today's rules, rulings and Oracle text —
//! [`judge_core::citation_supported`] per citation.
//!
//! This replaces the old rule that a prior call was retrievable only while its
//! `cr_version` was the newest loaded. That retired every call on every CR
//! release, including the many whose cited rules had not changed a character,
//! and retired nothing when Scryfall corrected a card's Oracle text or a
//! ruling — the one case where an old answer can be flatly wrong.
//!
//! One dependency is checked beyond the citations: the Oracle text of every
//! card that was in the call's context, fingerprinted at persist time
//! (`context_ids.card_text`, [`judge_core::oracle_fingerprint`]). The common
//! answer about a card cites the CR and not the card, so without this an
//! erratum to the card would leave the call live as an example with the old
//! wording baked in. Calls persisted before the fingerprint existed declare no
//! card dependency and are judged on their citations alone.
//!
//! What it deliberately does not catch: a rule the model *read* but did not
//! cite. Citations are the declared dependencies, the prompt demands them, and
//! prior calls are rendered below the CR as examples only, so the model still
//! answers from current rule text. Nor does retiring a call cascade to calls
//! that cite it as a prior call: such a citation quotes what that answer *said*,
//! which stays true, and the citing call's own rule and card dependencies are
//! checked independently.
//!
//! Both directions are computed on every run: a call is restored when its
//! citations hold again (a rule reworded back, a ruling re-added), so the state
//! is a function of the data, not a one-way flag. `cr_version` stays on the row
//! as a record of what the call was answered under.

use std::collections::{BTreeMap, BTreeSet};

use judge_core::{
    CallId, Category, Citation, Context, CrVersion, JudgeError, PriorCall, citation_supported, oracle_fingerprint,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{cards, retrieve, rules, upstream};

/// What one pass did. `checked` counts every call; the other three partition
/// the calls whose state was set or confirmed this run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetireSummary {
    /// Calls examined.
    pub checked: usize,
    /// Live calls retired this run.
    pub retired: usize,
    /// Retired calls whose citations hold again; made live.
    pub restored: usize,
    /// Calls already retired that remain so (reason refreshed).
    pub still_retired: usize,
}

/// Longest `retired_reason` stored; the offending citation is quoted in it.
const REASON_CHARS: usize = 400;

/// Advisory lock key shared with the CR loader, which rewrites the same rows
/// (renumbering) in its own transaction. Taking it in both makes the two
/// serialize instead of deadlocking when `ingest retire` is run by hand while
/// `ingest rules` is loading. The value is arbitrary and only has to match.
pub const CALLS_REWRITE_LOCK: i64 = 0x6a75_6467_6563_6c6c; // "judgecll"

/// One stored call as the pass reads it.
struct StoredCall {
    id: Uuid,
    /// `Ok` when the stored JSON decodes; `Err` carries the decode error.
    citations: Result<Vec<Citation>, String>,
    /// `context_ids.card_text` — card id → fingerprint at answer time.
    card_text: BTreeMap<Uuid, String>,
    retired_reason: Option<String>,
}

/// Re-check every call and set `retired_at`/`retired_reason` accordingly, in
/// one transaction. Reads all cited rules, cards, rulings and prior calls in
/// four queries and validates in memory, so the cost is a few hundred rows,
/// not a query per call. Rows whose state and reason are unchanged are not
/// written.
///
/// Every read happens inside the transaction, after the advisory lock: a CR
/// load committing between reading the calls and reading the rules would
/// otherwise show old citations against new rules and retire every relocated
/// call until the next run.
///
/// # Errors
/// On a database failure. Bad data is never fatal to the pass: a call whose
/// stored citations do not decode is retired with that as its reason, and a
/// cited source that cannot be loaded is simply absent, which retires its
/// citers.
pub async fn retire_unsupported(pool: &PgPool) -> Result<RetireSummary, JudgeError> {
    let mut tx = pool.begin().await.map_err(upstream("begin retirement"))?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", CALLS_REWRITE_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(upstream("lock calls for retirement"))?;

    let rows = sqlx::query!(
        r#"SELECT id, citations, context_ids -> 'card_text' AS card_text, retired_reason FROM calls ORDER BY created_at"#
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(upstream("calls for retirement"))?;

    let calls: Vec<StoredCall> = rows
        .into_iter()
        .map(|r| {
            let card_text = match r.card_text {
                None => BTreeMap::new(),
                Some(v) => serde_json::from_value(v).unwrap_or_else(|e| {
                    tracing::warn!(call = %r.id, error = %e, "context_ids.card_text is malformed; no card dependency recorded");
                    BTreeMap::new()
                }),
            };
            StoredCall {
                id: r.id,
                citations: serde_json::from_value::<Vec<Citation>>(r.citations).map_err(|e| e.to_string()),
                card_text,
                retired_reason: r.retired_reason,
            }
        })
        .collect();

    let ctx = context_for(&mut tx, &calls).await?;

    let mut summary = RetireSummary::default();
    for call in &calls {
        summary.checked += 1;
        let was_retired = call.retired_reason.is_some();
        match (reason_to_retire(call, &ctx), was_retired) {
            (None, false) => {}
            (None, true) => {
                sqlx::query!("UPDATE calls SET retired_at = NULL, retired_reason = NULL WHERE id = $1", call.id)
                    .execute(&mut *tx)
                    .await
                    .map_err(upstream("restore call"))?;
                tracing::info!(call = %call.id, "call restored: its citations hold again");
                summary.restored += 1;
            }
            (Some(reason), true) => {
                if call.retired_reason.as_deref() != Some(reason.as_str()) {
                    // Only the reason changed; keep the original retirement time.
                    sqlx::query!("UPDATE calls SET retired_reason = $2 WHERE id = $1", call.id, reason)
                        .execute(&mut *tx)
                        .await
                        .map_err(upstream("update retirement reason"))?;
                }
                summary.still_retired += 1;
            }
            (Some(reason), false) => {
                sqlx::query!("UPDATE calls SET retired_at = now(), retired_reason = $2 WHERE id = $1", call.id, reason)
                    .execute(&mut *tx)
                    .await
                    .map_err(upstream("retire call"))?;
                tracing::info!(call = %call.id, %reason, "call retired");
                summary.retired += 1;
            }
        }
    }
    tx.commit().await.map_err(upstream("commit retirement"))?;
    tracing::info!(?summary, "retirement pass");
    Ok(summary)
}

/// Why a call must be retired, or `None` while every citation is supported and
/// every context card still reads as it did. A call with no readable citations
/// is retired: every persisted verdict had at least one, so an empty or
/// undecodable list is a broken record, not a call that depends on nothing.
fn reason_to_retire(call: &StoredCall, ctx: &Context) -> Option<String> {
    let reason = match &call.citations {
        Err(e) => format!("stored citations do not decode: {e}"),
        Ok(c) if c.is_empty() => "no citations".to_owned(),
        Ok(c) => match c.iter().find(|c| !citation_supported(c, ctx)) {
            Some(c) => format!("unsupported citation: {c}"),
            None => changed_card(call, ctx)?,
        },
    };
    Some(truncate(&reason, REASON_CHARS))
}

/// The first context card whose Oracle text no longer matches its fingerprint,
/// or that no longer exists, as a reason. A card is `context_ids.card_text`
/// only if it was in the context, so a missing card is a removed card.
fn changed_card(call: &StoredCall, ctx: &Context) -> Option<String> {
    call.card_text.iter().find_map(|(id, fingerprint)| {
        match ctx.cards.iter().find(|c| c.id.into_inner() == *id) {
            None => Some(format!("card {id} is no longer in the card data")),
            Some(card) if oracle_fingerprint(card) != *fingerprint => {
                Some(format!("the Oracle text of {} ({id}) changed since the answer", card.name))
            }
            Some(_) => None,
        }
    })
}

fn truncate(s: &str, chars: usize) -> String {
    let mut out: String = s.chars().take(chars).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Everything the calls depend on, loaded once: exactly the rules, cards (with
/// faces), rulings and prior-call answers their citations name, plus the cards
/// their contexts were fingerprinted with, nothing else.
async fn context_for(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, calls: &[StoredCall]) -> Result<Context, JudgeError> {
    let mut rule_ids: BTreeSet<String> = BTreeSet::new();
    let mut card_ids: BTreeSet<Uuid> = BTreeSet::new();
    let mut call_ids: BTreeSet<Uuid> = BTreeSet::new();
    card_ids.extend(calls.iter().flat_map(|c| c.card_text.keys().copied()));
    for c in calls.iter().filter_map(|c| c.citations.as_ref().ok()).flatten() {
        match c {
            Citation::Rule { id, .. } => {
                rule_ids.insert(id.as_ref().to_owned());
            }
            Citation::ScryfallRuling { card, .. } | Citation::OracleText { card, .. } => {
                card_ids.insert(card.into_inner());
            }
            Citation::PriorCall { id, .. } => {
                call_ids.insert(id.into_inner());
            }
        }
    }
    let rule_ids: Vec<String> = rule_ids.into_iter().collect();
    let card_ids: Vec<Uuid> = card_ids.into_iter().collect();
    let call_ids: Vec<Uuid> = call_ids.into_iter().collect();

    let rules = rules::by_ids(&mut **tx, &[], &rule_ids).await?;
    let cards = cards::load_cards(&mut **tx, &card_ids).await?;
    let rulings = retrieve::load_rulings(&mut **tx, &card_ids).await?;
    let prior = prior_answers(&mut **tx, &call_ids).await?;
    Ok(Context { cards, rules, rulings, prior, ..Context::default() })
}

/// The cited prior calls, with only what a `prior_call` citation is checked
/// against (the answer text) filled in meaningfully. Retired or not: the quote
/// is of the answer as written, which does not change with its status. A row
/// with an unreadable `cr_version` is skipped with a warning, as retrieval
/// skips it, so one corrupt row retires its citers rather than aborting the pass.
async fn prior_answers(pool: impl sqlx::PgExecutor<'_>, ids: &[Uuid]) -> Result<Vec<PriorCall>, JudgeError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!("SELECT id, question, answer, category, cr_version FROM calls WHERE id = ANY($1)", ids)
        .fetch_all(pool)
        .await
        .map_err(upstream("cited prior calls"))?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let cr_version = match CrVersion::try_new(r.cr_version) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(call = %r.id, error = %e, "cited prior call has a bad cr_version; treated as absent");
                    return None;
                }
            };
            Some(PriorCall {
                id: CallId::new(r.id),
                question: r.question,
                answer: r.answer,
                category: r.category.parse().unwrap_or(Category::Other),
                citations: Vec::new(),
                cr_version,
                rating: 0.0,
                rating_count: 0,
            })
        })
        .collect())
}
