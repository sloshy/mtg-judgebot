//! One run of the built-in pipeline, as every interface does it: judge,
//! store the call or the failed call, record the run (`judge_runs`).
//!
//! Discord, the HTTP API and the agent toolbox's `judge` call [`run`] and
//! render what it returns. The run's [`Tally`] is made here and passed to
//! every step that can spend, so its cost is its own whatever else the
//! process is answering, and the stored call's embedding is on it too.
//!
//! The run is a task of its own. A caller that goes away (a browser tab
//! closed, a cancelled MCP request) does not take it down half-way: the
//! models it paid for still leave their answer stored and their run
//! recorded. The run's record is written after the result is handed back,
//! so a slow database does not hold the reply.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use judge_core::{
    CallId, CallStore, Context, Cost, Deps, JudgeError, Left, Origin, Outcome, Qa, Question,
    RunRecord, Stage, Tally, Traced, Validated, Verdict, judge_traced,
};
use tokio::sync::oneshot;

/// What a run returned and left.
#[derive(Debug)]
#[must_use]
pub struct Ran {
    /// The pipeline's result.
    pub result: Result<Verdict<Validated>, JudgeError>,
    /// The material it was judged from, when retrieval ran.
    pub context: Option<Context>,
    /// The stored call, when it answered and the store took it.
    pub call: Option<CallId>,
    /// How it ended.
    pub outcome: Outcome,
    /// What it cost, the stored call's embedding included.
    pub cost: Cost,
    /// Billed model calls (embeddings not counted).
    pub model_calls: usize,
    /// From the start of the pipeline to the stored call.
    pub elapsed: Duration,
}

/// Run the pipeline for `q`, asked from `origin` with thread `history`, on
/// a task of its own that holds `slot` (the caller's judge slot, so a run
/// whose caller went away still counts against the concurrency limit) until
/// the answer is stored.
///
/// A failure an operator needs is stored with `record_failure`; an answer
/// is stored with `persist`; the run is recorded either way, after the
/// result is returned. A store failure is logged and leaves the answer
/// unstored, never unanswered.
pub async fn run<H: Send + 'static>(
    deps: &Deps,
    store: &Arc<dyn CallStore>,
    q: &Question,
    origin: &Origin,
    history: Vec<Qa>,
    slot: H,
) -> Ran {
    let t0 = Instant::now();
    let (deps, store, q, origin) = (deps.clone(), Arc::clone(store), q.clone(), origin.clone());
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (ran, record) = judge_and_store(&deps, store.as_ref(), &q, &origin, &history, t0).await;
        drop(slot);
        // The caller may be gone; the run is recorded either way.
        let _ = tx.send(ran);
        if let Err(e) = store.record_run(&record).await {
            tracing::warn!(error = format_args!("{e:#}"), "could not record the run");
        }
    });
    rx.await.unwrap_or_else(|_| {
        tracing::error!("the judge run's task ended without a result");
        Ran {
            result: Err(anyhow::anyhow!("the judge run ended without a result").into()),
            context: None,
            call: None,
            outcome: Outcome::Upstream,
            cost: Cost::ZERO,
            model_calls: 0,
            elapsed: t0.elapsed(),
        }
    })
}

/// Judge `q` and store what it left; the run's record is returned for the
/// caller to write.
async fn judge_and_store(
    deps: &Deps,
    store: &dyn CallStore,
    q: &Question,
    origin: &Origin,
    history: &[Qa],
    t0: Instant,
) -> (Ran, RunRecord) {
    let tally = Tally::new();
    let traced = judge_traced(deps, q, history, &tally).await;
    let mut left = Left::Nothing;
    if let Some(failed) = traced.failure(q, origin) {
        match store.record_failure(&failed).await {
            Ok(id) => left = Left::Failure(id),
            Err(e) => tracing::warn!(
                error = format_args!("{e:#}"),
                "could not record the failed call"
            ),
        }
    }
    let Traced {
        result,
        first_rejection,
        context,
        ..
    } = traced;
    let call = match (&result, &context) {
        (Ok(v), Some(ctx)) => match store.persist(q, origin, v, ctx, &tally).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::error!(
                    error = format_args!("{e:#}"),
                    "persist failed; answering without a stored call"
                );
                None
            }
        },
        (Ok(_), None) => {
            tracing::error!("a verdict without the context it was judged from; call not persisted");
            None
        }
        (Err(_), _) => None,
    };
    if let Some(id) = call {
        left = Left::Call(id);
    }
    let outcome = Outcome::of(&result, &tally);
    let elapsed = t0.elapsed();
    let record = RunRecord {
        interface: origin.interface(),
        private: origin.is_private(),
        outcome,
        elapsed,
        retried: first_rejection.is_some(),
        charges: tally.charges(),
        left,
    };
    let ran = Ran {
        result,
        context,
        call,
        outcome,
        cost: tally.cost(),
        model_calls: tally.count(Stage::Extraction)
            + tally.count(Stage::Synthesis)
            + tally.count(Stage::ToolRound),
        elapsed,
    };
    (ran, record)
}
