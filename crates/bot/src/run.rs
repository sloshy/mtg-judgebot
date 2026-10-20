//! One run of the built-in pipeline, as every interface does it: judge,
//! store the call or the failed call, record the run (`judge_runs`).
//!
//! Discord, the HTTP API and the agent toolbox's `judge` call [`run`] and
//! render what it returns. The run's [`Tally`] is made here and passed to
//! every step that can spend, so its cost is its own whatever else the
//! process is answering, and the stored call's embedding is on it too.
//!
//! The run is a task of its own. A caller that goes away (a browser tab
//! closed, a cancelled MCP request) does not take it down half-way. The run
//! is marked abandoned ([`Tally::abandon`]): the request in flight finishes,
//! since the provider bills it either way, and no new model call is sent,
//! so the run ends `abandoned` at its next one. What it paid for is still
//! stored (an answer that was already written) and its run recorded with the
//! charges it made. The record is written after the result is handed back,
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
    let (mut tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let tally = Tally::new();
        let work = judge_and_store(&deps, store.as_ref(), &q, &origin, &history, &tally, t0);
        tokio::pin!(work);
        let (ran, record) = tokio::select! {
            done = &mut work => done,
            () = tx.closed() => {
                tally.abandon();
                tracing::info!(
                    thread = %q.thread_id,
                    "the caller went away; the run stops before its next model call"
                );
                work.await
            }
        };
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
    tally: &Tally,
    t0: Instant,
) -> (Ran, RunRecord) {
    let traced = judge_traced(deps, q, history, tally).await;
    let mut left = Left::Nothing;
    // A run stopped because its caller left has nothing for an operator to
    // diagnose; its `judge_runs` row says how it ended.
    if let Some(failed) = traced.failure(q, origin).filter(|_| !tally.was_stopped()) {
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
        (Ok(v), Some(ctx)) => match store.persist(q, origin, v, ctx, tally).await {
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
    let outcome = Outcome::of(&result, tally);
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use judge_core::{
        Card, Charge, Extraction, Extractor, FailedCall, FailedCallId, Forgotten, HistoryScope,
        RejectedAttempt, Resolution, Resolver, Retriever, RuleChunk, RuleId, Synthesizer,
        Unvalidated,
    };
    use tokio::sync::Notify;

    use super::*;

    /// Bills one request, says so, waits for the caller to leave, then tries
    /// a second request the way `Metered` would.
    struct Extract(Arc<Notify>);
    #[async_trait]
    impl Extractor for Extract {
        async fn extract(
            &self,
            _q: &Question,
            _h: &[Qa],
            tally: &Tally,
        ) -> Result<Extraction, JudgeError> {
            tally.charge(Charge {
                stage: Stage::Extraction,
                provider: "anthropic".into(),
                model: "m".into(),
                tokens: None,
                cost: Cost::Billed(0.01),
            });
            self.0.notify_one();
            for _ in 0..200 {
                if tally.is_abandoned() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if tally.bill(Stage::Synthesis).stop_if_abandoned() {
                return Err(anyhow::Error::from(judge_llm::LlmError::Abandoned).into());
            }
            Err(anyhow::anyhow!("the caller never went away").into())
        }
    }

    struct Unused;
    #[async_trait]
    impl Resolver for Unused {
        async fn resolve(&self, _span: &str) -> Result<Resolution, JudgeError> {
            Err(anyhow::anyhow!("unused").into())
        }
    }
    #[async_trait]
    impl Retriever for Unused {
        async fn retrieve(
            &self,
            _q: &Question,
            _c: &[Card],
            _e: &Extraction,
            _t: &Tally,
        ) -> Result<Context, JudgeError> {
            Err(anyhow::anyhow!("unused").into())
        }
        async fn lookup_rules(&self, _ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            Err(anyhow::anyhow!("unused").into())
        }
    }
    #[async_trait]
    impl Synthesizer for Unused {
        async fn answer(
            &self,
            _q: &Question,
            _ctx: &mut Context,
            _r: Option<&RejectedAttempt>,
            _t: &Tally,
        ) -> Result<Verdict<Unvalidated>, JudgeError> {
            Err(anyhow::anyhow!("unused").into())
        }
    }

    #[derive(Default)]
    struct Store(Mutex<Vec<RunRecord>>);
    #[async_trait]
    impl CallStore for Store {
        async fn persist(
            &self,
            _q: &Question,
            _o: &Origin,
            _v: &Verdict<Validated>,
            _c: &Context,
            _t: &Tally,
        ) -> Result<CallId, JudgeError> {
            Err(anyhow::anyhow!("unused").into())
        }
        async fn rate(
            &self,
            _c: CallId,
            _r: &judge_core::Rater,
            _s: &judge_core::Rating,
        ) -> Result<judge_core::Rated, JudgeError> {
            Ok(judge_core::Rated::Recorded)
        }
        async fn earlier_rating(
            &self,
            _c: CallId,
            _r: judge_core::RaterId,
        ) -> Result<Option<judge_core::Earlier>, JudgeError> {
            Ok(None)
        }
        async fn history(
            &self,
            _t: &str,
            _n: usize,
            _s: HistoryScope,
        ) -> Result<Vec<Qa>, JudgeError> {
            Ok(vec![])
        }
        async fn forget_user(&self, _u: &str) -> Result<Forgotten, JudgeError> {
            Ok(Forgotten::default())
        }
        async fn record_failure(&self, _f: &FailedCall) -> Result<FailedCallId, JudgeError> {
            Ok(FailedCallId(7))
        }
        async fn record_run(&self, run: &RunRecord) -> Result<(), JudgeError> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(run.clone());
            Ok(())
        }
    }

    /// A caller that goes away mid-run leaves the run to finish its request
    /// in flight, send nothing more, and be recorded `abandoned` with what it
    /// paid for.
    #[tokio::test]
    async fn a_caller_that_goes_away_stops_the_run_at_its_next_model_call() -> anyhow::Result<()> {
        let started = Arc::new(Notify::new());
        let deps = Deps {
            extractor: Arc::new(Extract(Arc::clone(&started))),
            resolver: Arc::new(Unused),
            retriever: Arc::new(Unused),
            synthesizer: Arc::new(Unused),
        };
        let store = Arc::new(Store::default());
        let calls: Arc<dyn CallStore> = Arc::clone(&store) as Arc<dyn CallStore>;
        let q = Question {
            thread_id: "web:x".into(),
            text: "does lifelink stack?".into(),
        };
        let slot = Arc::new(());
        tokio::select! {
            _ = run(&deps, &calls, &q, &Origin::Web, vec![], Arc::clone(&slot)) => {
                anyhow::bail!("the run finished while its caller waited");
            }
            () = started.notified() => {}
        }
        // The caller's future is gone; the run goes on and is recorded.
        let mut runs = vec![];
        for _ in 0..200 {
            runs = store
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if !runs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let [recorded] = runs.as_slice() else {
            anyhow::bail!("one run recorded, got {}", runs.len());
        };
        assert_eq!(recorded.outcome, Outcome::Abandoned);
        assert_eq!(recorded.charges.len(), 1, "the request in flight is kept");
        assert_eq!(recorded.left, Left::Nothing, "not a failure to diagnose");
        assert_eq!(Arc::strong_count(&slot), 1, "the slot is given back");
        Ok(())
    }
}
