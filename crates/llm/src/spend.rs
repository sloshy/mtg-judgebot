//! The spend cap: [`Metered`] wraps any [`ChatModel`] and refuses a request
//! whose worst-case cost would take the shared total past the cap.
//!
//! Enforcement is by *reservation*: before a request is sent, a worst-case
//! estimate of its cost (`max_tokens` at the output price plus the request
//! at the input price) is added to the shared counter and the request is
//! refused if that would exceed the cap; after the response the reservation
//! is replaced by the actual usage. Concurrent callers therefore cannot
//! collectively overshoot by more than the estimation error of one call, and
//! a 2xx body that fails to decode is still billed from its usage when the
//! backend could parse that much ([`LlmError::Decode`]'s `billed`).
//!
//! The counters live in a [`SpendMeter`]: every `Metered` built over the
//! same meter shares one total and one cap (one cap per process, as the
//! front doors expect), and the meter is what they read `spent_usd()` from.
//!
//! `Metered` is the only [`ChatModel`] there is (the trait is sealed), so
//! the pipeline cannot be handed a backend that skips this. A model that
//! costs nothing ([`Price::Free`], a local server) skips the reservation and
//! is only counted, so an exhausted cap on a paid sibling does not refuse it.
//!
//! The worst-case input term is sized from the serialized *neutral* request,
//! not the wire body the backend builds afterwards; it is a few percent
//! larger than the body (untransformed schemas, tags), pessimistic either
//! way, and replaced by the real usage after the call.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    Backend, Capabilities, ChatModel, ChatRequest, ChatResponse, LlmError, Usage, types::sealed,
};

/// `JUDGE_MAX_USD` fallback, and the cap every [`SpendMeter::new`] starts
/// with: deliberately conservative for a prototype on a small credit balance.
pub const DEFAULT_MAX_SPEND_USD: f64 = 5.00;
/// Micro-dollars per dollar (spend is tracked as an integer so `AtomicU64` can hold it).
const MICRO_PER_USD: f64 = 1_000_000.0;

/// USD per million tokens for one model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    /// Uncached input tokens.
    pub input: f64,
    /// Output tokens (reasoning included).
    pub output: f64,
    /// Prompt-cache reads.
    pub cache_read: f64,
    /// Prompt-cache writes.
    pub cache_write: f64,
}

impl Pricing {
    /// Estimated cost of `usage` in USD.
    #[must_use]
    pub fn usd(&self, usage: &Usage) -> f64 {
        // u64 → f64 loses nothing below 2^53 tokens; billing estimates do not need more.
        #[allow(clippy::cast_precision_loss)]
        let tok = |n: u64| n as f64 / 1_000_000.0;
        tok(usage.input) * self.input
            + tok(usage.output) * self.output
            + tok(usage.cache_read) * self.cache_read
            + tok(usage.cache_write) * self.cache_write
    }
}

/// What a model is billed at. A closed sum so that "costs nothing" is a
/// state the cap understands rather than a bypass beside it, and so that
/// where a rate came from decides how a response is settled: the built-in
/// table is looked up again by the model the *response* names (an Anthropic
/// fallback may have routed elsewhere), an operator's rate is the rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Price {
    /// Never reserves and never settles; calls are still counted and usage
    /// still logged. For a local server or a flat-rate proxy.
    Free,
    /// The built-in table's rate for the configured model. Reserves at this
    /// rate; settles at the table's rate for the response's model, or this
    /// one when the table does not know it.
    Table(Pricing),
    /// An operator-supplied rate (`[models.<stage>.pricing]`): reserves and
    /// settles at exactly this, whatever model the response names. The table
    /// is never consulted, so a proxy priced above or below it bills as the
    /// operator said.
    PerToken(Pricing),
}

/// The Anthropic provider key in [`PRICES`].
pub const ANTHROPIC: &str = "anthropic";
const OPUS_5: Pricing = Pricing {
    input: 5.0,
    output: 25.0,
    cache_read: 0.50,
    cache_write: 6.25,
};

/// Built-in price table, `(provider, model)` → USD per million tokens.
/// Verified 2026-08-29 against the Anthropic pricing page; re-check when a
/// model is added or a price changes.
pub const PRICES: &[(&str, &str, Pricing)] = &[(ANTHROPIC, "claude-opus-5", OPUS_5)];

/// Pricing for `model` at `provider`. An unknown Anthropic model (including
/// a fallback the server routed to) is priced as Opus 5 so the estimate errs
/// high rather than low; an unknown model elsewhere is `None`, because it
/// could be anything and the operator must say.
#[must_use]
pub fn pricing_for(provider: &str, model: &str) -> Option<Pricing> {
    PRICES
        .iter()
        .find(|(p, m, _)| *p == provider && *m == model)
        .map(|(_, _, pricing)| *pricing)
        .or_else(|| (provider == ANTHROPIC).then_some(OPUS_5))
}

/// Spend counters and cap shared by everything built over one meter.
#[derive(Debug)]
struct Spend {
    /// Recorded spend plus in-flight reservations, in micro-dollars.
    micro_usd: AtomicU64,
    calls: AtomicU64,
    /// Cap in micro-dollars; shared, so a change through any handle applies to all.
    cap_micro_usd: AtomicU64,
}

impl Spend {
    /// Reserve `estimate` micro-dollars atomically, or report the cap.
    fn reserve(&self, estimate: u64) -> Result<(), LlmError> {
        let cap = self.cap_micro_usd.load(Ordering::Relaxed);
        let mut spent = self.micro_usd.load(Ordering::Relaxed);
        loop {
            if spent >= cap || spent.saturating_add(estimate) > cap {
                return Err(LlmError::SpendCapExceeded {
                    spent: from_micro(spent),
                    cap: from_micro(cap),
                });
            }
            match self.micro_usd.compare_exchange_weak(
                spent,
                spent + estimate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => spent = actual,
            }
        }
    }

    /// Replace a reservation by the actual cost; returns the new total.
    fn settle(&self, reserved: u64, actual: u64) -> u64 {
        if actual >= reserved {
            self.micro_usd
                .fetch_add(actual - reserved, Ordering::Relaxed)
                + (actual - reserved)
        } else {
            self.micro_usd
                .fetch_sub(reserved - actual, Ordering::Relaxed)
                - (reserved - actual)
        }
    }
}

/// The handle to one process's spend counters and cap. Cheap to clone;
/// every clone reads and caps the same total.
#[derive(Clone)]
pub struct SpendMeter(Arc<Spend>);

impl Default for SpendMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SpendMeter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpendMeter")
            .field("spent_usd", &self.spent_usd())
            .field("max_spend_usd", &self.max_spend_usd())
            .field("calls", &self.calls())
            .finish()
    }
}

impl SpendMeter {
    /// A fresh meter capped at [`DEFAULT_MAX_SPEND_USD`].
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Spend {
            micro_usd: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            cap_micro_usd: AtomicU64::new(to_micro(DEFAULT_MAX_SPEND_USD)),
        }))
    }

    /// A fresh meter capped by `JUDGE_MAX_USD`, defaulting to [`DEFAULT_MAX_SPEND_USD`].
    ///
    /// # Errors
    /// `BadMaxSpend` when the variable is set but not a finite non-negative number.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_var(std::env::var("JUDGE_MAX_USD").ok().as_deref())
    }

    /// [`Self::from_env`] over the variable's value as read by the caller
    /// (a config loader that injects its environment), so nothing here
    /// touches the process environment.
    ///
    /// # Errors
    /// `BadMaxSpend` when `raw` is not a finite non-negative number.
    pub fn from_var(raw: Option<&str>) -> Result<Self, LlmError> {
        let meter = Self::new();
        match raw {
            Some(raw) => meter.with_max_spend_usd(env_cap(raw)?),
            None => Ok(meter),
        }
    }

    /// Cap cumulative estimated spend. The cap is shared with every clone
    /// (made before or after this call), like the counter itself.
    ///
    /// # Errors
    /// `BadMaxSpend` unless `cap_usd` is finite and non-negative.
    pub fn with_max_spend_usd(self, cap_usd: f64) -> Result<Self, LlmError> {
        self.set_max_spend_usd(cap_usd)?;
        Ok(self)
    }

    /// [`Self::with_max_spend_usd`] in place.
    ///
    /// # Errors
    /// `BadMaxSpend` unless `cap_usd` is finite and non-negative.
    pub fn set_max_spend_usd(&self, cap_usd: f64) -> Result<(), LlmError> {
        if !(cap_usd.is_finite() && cap_usd >= 0.0) {
            return Err(LlmError::BadMaxSpend {
                setting: "spend cap",
                value: cap_usd.to_string(),
            });
        }
        self.0
            .cap_micro_usd
            .store(to_micro(cap_usd), Ordering::Relaxed);
        Ok(())
    }

    /// The cap in USD.
    #[must_use]
    pub fn max_spend_usd(&self) -> f64 {
        from_micro(self.0.cap_micro_usd.load(Ordering::Relaxed))
    }

    /// Estimated USD spent through every model sharing this meter.
    #[must_use]
    pub fn spent_usd(&self) -> f64 {
        from_micro(self.0.micro_usd.load(Ordering::Relaxed))
    }

    /// Billed calls through every model sharing this meter.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.0.calls.load(Ordering::Relaxed)
    }
}

/// A [`Backend`] behind the spend cap, and the only [`ChatModel`]. Every
/// send goes through [`ChatModel::complete`] here: reserve, send, settle.
pub struct Metered<B> {
    inner: B,
    meter: SpendMeter,
    /// What the configured model is billed at (see [`Price`] for how the
    /// response's model figures in settlement).
    price: Price,
}

impl<B: Clone> Clone for Metered<B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            meter: self.meter.clone(),
            price: self.price,
        }
    }
}

impl<B: fmt::Debug> fmt::Debug for Metered<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metered")
            .field("inner", &self.inner)
            .field("meter", &self.meter)
            .field("price", &self.price)
            .finish()
    }
}

impl<B: Backend> Metered<B> {
    /// Put `inner` behind `meter` at the built-in table's price.
    ///
    /// # Errors
    /// `Unpriced` when the table cannot price the model (see [`pricing_for`]).
    pub fn new(inner: B, meter: SpendMeter) -> Result<Self, LlmError> {
        let pricing =
            pricing_for(inner.provider(), inner.model()).ok_or_else(|| LlmError::Unpriced {
                provider: inner.provider().to_owned(),
                model: inner.model().to_owned(),
            })?;
        Ok(Self::priced(inner, meter, Price::Table(pricing)))
    }

    /// Put `inner` behind `meter` at a given price: an operator-supplied
    /// rate ([`Price::PerToken`], authoritative), the table's
    /// ([`Price::Table`]), or [`Price::Free`].
    #[must_use]
    pub fn priced(inner: B, meter: SpendMeter, price: Price) -> Self {
        Self {
            inner,
            meter,
            price,
        }
    }

    /// What this model is billed at.
    #[must_use]
    pub fn price(&self) -> Price {
        self.price
    }

    /// The shared meter.
    #[must_use]
    pub fn meter(&self) -> SpendMeter {
        self.meter.clone()
    }

    /// The wrapped backend.
    #[must_use]
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// Reserve the worst case of `req`, or nothing for a free model.
    fn reserve(&self, req: &ChatRequest) -> Result<Option<Reservation>, LlmError> {
        let (pricing, from_table) = match self.price {
            Price::Free => return Ok(None),
            Price::Table(pricing) => (pricing, true),
            Price::PerToken(pricing) => (pricing, false),
        };
        let micro = estimate_micro(req, &pricing);
        self.meter.0.reserve(micro)?;
        Ok(Some(Reservation {
            pricing,
            from_table,
            micro,
        }))
    }

    /// Replace `reserved` by the real cost of a billed response in the shared counters and log it.
    fn record(&self, reserved: Option<Reservation>, model: &str, usage: &Usage) {
        let usd = reserved.map_or(0.0, |r| {
            // Only a table price is re-read for the model the response names;
            // an operator's rate is what the operator pays, whatever the server
            // called the model.
            let rate = if r.from_table {
                pricing_for(self.inner.provider(), model).unwrap_or(r.pricing)
            } else {
                r.pricing
            };
            let usd = rate.usd(usage);
            self.meter.0.settle(r.micro, to_micro(usd));
            usd
        });
        let total = self.meter.0.micro_usd.load(Ordering::Relaxed);
        let calls = self.meter.0.calls.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            provider = self.inner.provider(),
            model,
            input_tokens = usage.input,
            output_tokens = usage.output,
            cache_read_tokens = usage.cache_read,
            cache_write_tokens = usage.cache_write,
            usd = format_args!("{usd:.4}"),
            cumulative_usd = format_args!("{:.4}", from_micro(total)),
            calls,
            "llm call"
        );
    }
}

/// A worst case taken out of the shared total before a send, to be replaced by the real cost.
#[derive(Clone, Copy)]
struct Reservation {
    /// The rate reserved at.
    pricing: Pricing,
    /// Whether `pricing` came from the table (settle by the response's model) or the operator (settle at `pricing`).
    from_table: bool,
    micro: u64,
}

impl<B: Backend> sealed::Sealed for Metered<B> {}

#[async_trait]
impl<B: Backend> ChatModel for Metered<B> {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let reserved = self.reserve(req)?;
        match self.inner.complete(req).await {
            Ok(resp) => {
                self.record(reserved, &resp.model, &resp.usage);
                Ok(resp)
            }
            // A body that was billed but did not decode still counts; anything else frees the reservation.
            Err(LlmError::Decode {
                source,
                billed: Some(billed),
            }) => {
                self.record(
                    reserved,
                    billed
                        .model
                        .as_deref()
                        .unwrap_or_else(|| self.inner.model()),
                    &billed.usage,
                );
                Err(LlmError::Decode {
                    source,
                    billed: Some(billed),
                })
            }
            Err(err) => {
                if let Some(r) = reserved {
                    self.meter.0.settle(r.micro, 0);
                }
                Err(err)
            }
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }
}

/// `JUDGE_MAX_USD`'s value as a cap, naming the variable when it is not one
/// (an operator reading the startup failure must know which key to fix).
fn env_cap(raw: &str) -> Result<f64, LlmError> {
    let bad = || LlmError::BadMaxSpend {
        setting: "JUDGE_MAX_USD",
        value: raw.to_owned(),
    };
    let cap = raw.trim().parse::<f64>().map_err(|_| bad())?;
    (cap.is_finite() && cap >= 0.0)
        .then_some(cap)
        .ok_or_else(bad)
}

/// Worst-case cost of `req` in micro-dollars: `max_tokens` at the output
/// price plus the serialized neutral request at roughly four bytes per token
/// at the uncached input price. Deliberately pessimistic; the reservation is
/// replaced by the real usage afterwards.
fn estimate_micro(req: &ChatRequest, p: &Pricing) -> u64 {
    let body_bytes = serde_json::to_vec(req).map_or(0, |b| b.len());
    #[allow(clippy::cast_precision_loss)]
    let input_tokens = (body_bytes / 4) as f64;
    let usd = (f64::from(req.max_tokens) * p.output + input_tokens * p.input) / 1_000_000.0;
    to_micro(usd)
}

/// USD → micro-dollars, rounded to nearest; negative or NaN clamps to 0.
fn to_micro(usd: f64) -> u64 {
    // Rounded, clamped conversion; values are tiny relative to u64::MAX.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let m = (usd * MICRO_PER_USD).round().max(0.0) as u64;
    m
}

fn from_micro(micro: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let usd = micro as f64 / MICRO_PER_USD;
    usd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssistantTurn, Billed, Stop, StructuredOutput, ToolChoice};
    use std::sync::Mutex;

    /// Answers with a scripted outcome and remembers nothing else.
    struct Stub {
        provider: &'static str,
        replies: Mutex<Vec<Result<ChatResponse, LlmError>>>,
    }

    fn reply(input: u64, output: u64, cache_read: u64, cache_write: u64) -> ChatResponse {
        ChatResponse {
            text: vec!["hi".into()],
            tool_calls: vec![],
            stop: Stop::EndTurn,
            usage: Usage {
                input,
                output,
                cache_read,
                cache_write,
            },
            model: "claude-opus-5".into(),
            assistant: AssistantTurn {
                backend: "stub",
                raw: serde_json::Value::Null,
            },
        }
    }

    fn stub(replies: Vec<Result<ChatResponse, LlmError>>) -> Stub {
        Stub {
            provider: ANTHROPIC,
            replies: Mutex::new(replies),
        }
    }

    #[async_trait]
    impl Backend for Stub {
        async fn complete(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let mut r = self
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if r.is_empty() {
                return Err(LlmError::Request("no scripted reply".into()));
            }
            r.remove(0)
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                structured_output: StructuredOutput::Enforced,
                strict_tools: true,
                effort: true,
                cache_hints: true,
                refusal_fallbacks: true,
            }
        }
        fn provider(&self) -> &'static str {
            self.provider
        }
        fn model(&self) -> &'static str {
            "claude-opus-5"
        }
    }

    fn req(max_tokens: u32) -> ChatRequest {
        ChatRequest {
            max_tokens,
            system: vec![],
            turns: vec![crate::Turn::User(vec![crate::TextBlock::plain("hi")])],
            tools: vec![],
            tool_choice: ToolChoice::None,
            output: None,
            effort: None,
            thinking: false,
            fallbacks: None,
        }
    }

    #[test]
    fn opus_5_pricing_matches_table() {
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
        };
        let usd = pricing_for(ANTHROPIC, "claude-opus-5")
            .map(|p| p.usd(&u))
            .unwrap_or_default();
        assert!((usd - (5.0 + 25.0 + 0.5 + 6.25)).abs() < 1e-9, "{usd}");
        // Unknown Anthropic models price as Opus 5 (never under-estimate); unknown providers are unpriced.
        assert_eq!(
            pricing_for(ANTHROPIC, "claude-something-new"),
            pricing_for(ANTHROPIC, "claude-opus-5")
        );
        assert_eq!(pricing_for("somewhere-else", "claude-opus-5"), None);
    }

    #[tokio::test]
    async fn usage_accumulates_and_cap_blocks_without_sending() -> Result<(), LlmError> {
        // 1M input + 200k output = $5 + $5 = $10 per call.
        let replies = (0..3)
            .map(|_| Ok(reply(1_000_000, 200_000, 0, 0)))
            .collect();
        let meter = SpendMeter::new().with_max_spend_usd(15.0)?;
        let m = Metered::new(stub(replies), meter.clone())?;
        m.complete(&req(64)).await?;
        assert!(
            (meter.spent_usd() - 10.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        m.complete(&req(64)).await?;
        assert!(
            (m.meter().spent_usd() - 20.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.calls(), 2);
        let third = m.complete(&req(64)).await;
        assert!(
            matches!(third, Err(LlmError::SpendCapExceeded { spent, cap }) if (spent - 20.0).abs() < 1e-6 && (cap - 15.0).abs() < 1e-6),
            "{third:?}"
        );
        assert_eq!(
            m.inner()
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "the third was not sent"
        );
        assert_eq!(meter.calls(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn failed_calls_do_not_count_and_free_the_reservation() -> Result<(), LlmError> {
        let err = LlmError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            kind: "invalid_request_error".into(),
            message: "nope".into(),
        };
        let m = Metered::new(stub(vec![Err(err)]), SpendMeter::new())?;
        assert!(matches!(
            m.complete(&req(64)).await,
            Err(LlmError::Api { .. })
        ));
        assert_eq!(m.meter().calls(), 0);
        assert!(m.meter().spent_usd().abs() < f64::EPSILON);
        Ok(())
    }

    #[tokio::test]
    async fn undecodable_2xx_body_is_still_billed() -> Result<(), LlmError> {
        let source = match serde_json::from_str::<u8>("x") {
            Err(e) => e,
            Ok(_) => serde::de::Error::custom("unreachable: \"x\" is not a number"),
        };
        let billed = Some(Billed {
            model: None,
            usage: Usage {
                input: 1_000_000,
                ..Usage::default()
            },
        });
        let m = Metered::new(
            stub(vec![Err(LlmError::Decode { source, billed })]),
            SpendMeter::new(),
        )?;
        assert!(matches!(
            m.complete(&req(64)).await,
            Err(LlmError::Decode {
                billed: Some(_),
                ..
            })
        ));
        assert!(
            (m.meter().spent_usd() - 5.0).abs() < 1e-6,
            "{}",
            m.meter().spent_usd()
        );
        assert_eq!(m.meter().calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reservation_refuses_a_request_that_cannot_fit() -> Result<(), LlmError> {
        // 16k output tokens at $25/MTok reserve $0.40; a $0.10 cap cannot fit it.
        let m = Metered::new(
            stub(vec![Ok(reply(1, 1, 0, 0))]),
            SpendMeter::new().with_max_spend_usd(0.10)?,
        )?;
        assert!(matches!(
            m.complete(&req(16_000)).await,
            Err(LlmError::SpendCapExceeded { .. })
        ));
        // The small request fits, and afterwards the reservation is gone.
        m.complete(&req(64)).await?;
        assert!(m.meter().spent_usd() < 0.001, "{}", m.meter().spent_usd());
        assert_eq!(m.meter().calls(), 1);
        Ok(())
    }

    #[test]
    fn cap_is_shared_by_clones_and_validated() -> Result<(), LlmError> {
        let a = SpendMeter::new();
        assert!((a.max_spend_usd() - DEFAULT_MAX_SPEND_USD).abs() < 1e-9);
        let b = a.clone();
        let a = a.with_max_spend_usd(1.5)?;
        assert!(
            (b.max_spend_usd() - 1.5).abs() < 1e-9,
            "clone made before the call shares the cap"
        );
        for bad in [f64::NAN, -1.0, f64::INFINITY] {
            assert!(
                matches!(
                    a.clone().with_max_spend_usd(bad),
                    Err(LlmError::BadMaxSpend {
                        setting: "spend cap",
                        ..
                    })
                ),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_unpriced_model_is_refused_at_construction_unless_priced_explicitly() {
        let unpriced = || Stub {
            provider: "elsewhere",
            replies: Mutex::new(vec![]),
        };
        assert!(matches!(
            Metered::new(unpriced(), SpendMeter::new()),
            Err(LlmError::Unpriced { .. })
        ));
        let rate = Pricing {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 1.25,
        };
        assert_eq!(
            Metered::priced(unpriced(), SpendMeter::new(), Price::PerToken(rate)).price(),
            Price::PerToken(rate)
        );
        assert_eq!(
            Metered::priced(unpriced(), SpendMeter::new(), Price::Free).price(),
            Price::Free
        );
        assert_eq!(
            Metered::new(stub(vec![]), SpendMeter::new())
                .map(|m| m.price())
                .ok(),
            Some(Price::Table(OPUS_5)),
            "the table's price is marked as such"
        );
    }

    #[tokio::test]
    async fn an_operator_price_settles_at_that_price_even_on_a_tabled_provider()
    -> Result<(), LlmError> {
        // An Anthropic-kind provider (a proxy, say) whose reply names claude-opus-5, which the
        // table prices at $5/$25: the operator said $1/$5, so 1M input + 200k output is $2, not $10.
        let rate = Pricing {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        };
        let meter = SpendMeter::new();
        let m = Metered::priced(
            stub(vec![Ok(reply(1_000_000, 200_000, 0, 0))]),
            meter.clone(),
            Price::PerToken(rate),
        );
        m.complete(&req(64)).await?;
        assert!(
            (meter.spent_usd() - 2.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        // The reverse direction matters more: a rate above the table's must not settle below it.
        let dear = Pricing {
            input: 50.0,
            output: 250.0,
            cache_read: 5.0,
            cache_write: 62.5,
        };
        let meter = SpendMeter::new().with_max_spend_usd(1_000.0)?;
        let m = Metered::priced(
            stub(vec![Ok(reply(1_000_000, 200_000, 0, 0))]),
            meter.clone(),
            Price::PerToken(dear),
        );
        m.complete(&req(64)).await?;
        assert!(
            (meter.spent_usd() - 100.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        // A table price does follow the response's model (a fallback may have routed elsewhere).
        let meter = SpendMeter::new();
        let m = Metered::priced(
            stub(vec![Ok(reply(1_000_000, 200_000, 0, 0))]),
            meter.clone(),
            Price::Table(rate),
        );
        m.complete(&req(64)).await?;
        assert!(
            (meter.spent_usd() - 10.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_free_model_is_counted_but_never_capped() -> Result<(), LlmError> {
        // A paid sibling exhausts the shared cap ($10 of a $8 cap)...
        let meter = SpendMeter::new().with_max_spend_usd(8.0)?;
        let paid = Metered::new(
            stub(vec![Ok(reply(1_000_000, 200_000, 0, 0))]),
            meter.clone(),
        )?;
        paid.complete(&req(64)).await?;
        assert!(matches!(
            paid.complete(&req(64)).await,
            Err(LlmError::SpendCapExceeded { .. })
        ));
        // ...and the free one still answers, counted but not billed, even for a huge output.
        let free = Metered::priced(
            Stub {
                provider: "elsewhere",
                replies: Mutex::new(vec![Ok(reply(5_000_000, 5_000_000, 0, 0))]),
            },
            meter.clone(),
            Price::Free,
        );
        free.complete(&req(16_000)).await?;
        assert!(
            (meter.spent_usd() - 10.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.calls(), 2);
        // A failure on a free model has nothing to free and nothing to count.
        let failing = Metered::priced(
            Stub {
                provider: "elsewhere",
                replies: Mutex::new(vec![Err(LlmError::Request("nope".into()))]),
            },
            meter.clone(),
            Price::Free,
        );
        assert!(matches!(
            failing.complete(&req(64)).await,
            Err(LlmError::Request(_))
        ));
        assert_eq!(meter.calls(), 2);
        assert!((meter.spent_usd() - 10.0).abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn a_bad_env_cap_names_the_variable() -> Result<(), LlmError> {
        assert!((env_cap(" 2.5 ")? - 2.5).abs() < 1e-9);
        assert!((SpendMeter::from_var(Some("2.5"))?.max_spend_usd() - 2.5).abs() < 1e-9);
        assert!((SpendMeter::from_var(None)?.max_spend_usd() - DEFAULT_MAX_SPEND_USD).abs() < 1e-9);
        assert!(matches!(
            SpendMeter::from_var(Some("$5")),
            Err(LlmError::BadMaxSpend {
                setting: "JUDGE_MAX_USD",
                ..
            })
        ));
        for raw in ["$5", "", "-1", "inf", "NaN"] {
            let err = env_cap(raw).err().map(|e| e.to_string());
            assert_eq!(
                err,
                Some(format!(
                    "JUDGE_MAX_USD is not a finite non-negative number: {raw:?}"
                )),
                "{raw}"
            );
        }
        Ok(())
    }
}
