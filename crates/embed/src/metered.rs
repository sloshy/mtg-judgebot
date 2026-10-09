//! The spend cap for embeddings: [`MeteredEmbedder`] wraps any
//! [`EmbedBackend`] and bills it to the process's [`SpendMeter`], the one its
//! chat models bill to (`docs/DECISIONS.md` D7).
//!
//! **By type.** [`WithSpace`] — what `Vectors`, the retriever, the call store
//! and the ingest embed step take — is sealed, and `MeteredEmbedder` is its one
//! implementation. The provider adapters ([`crate::VoyageEmbedder`],
//! [`crate::OpenAiEmbedder`]) implement only [`EmbedBackend`], which nothing
//! downstream accepts, so an embedder that skips the cap cannot reach a
//! vector column or a query.
//!
//! **Reserve, then settle.** Before a request the worst case is taken out of
//! the cap ([`SpendMeter::reserve_embedding_usd`]): every text's UTF-8 bytes plus
//! [`TOKENS_PER_TEXT`] tokens each, at the model's price ([`worst_case_tokens`]).
//! A token is at least one byte in every tokenizer the providers use (byte-level
//! BPE, or `SentencePiece` with byte fallback), so a text of `n` bytes is at most
//! `n` tokens, and the per-text allowance covers what the provider adds: Voyage's
//! `input_type` prompt ("Represent the query for retrieving supporting
//! documents: ", about ten tokens) and the start and end markers. Real text runs
//! three to five bytes a token, so the reservation is that many times the cost;
//! it is held only for the request. A request the cap cannot fit is refused
//! with [`LlmError::SpendCapExceeded`] before anything is sent. Afterwards the
//! reservation is replaced by the provider's reported usage (Voyage's
//! `usage.total_tokens`, `OpenAI`'s `usage.total_tokens` or `prompt_tokens`); a
//! billed response that reports none, or reports 0 tokens for texts it was
//! sent, keeps the whole reservation as its cost. A request that failed
//! before a 2xx was billed releases it. A request whose future is dropped
//! mid-flight (the provider may have billed it) keeps it, as a cancelled chat
//! call does. One request, one reservation: a batch of 128 texts is one.
//!
//! The bound can be beaten, by fractions of a cent: a server-side instruction
//! prompt longer than the allowance on a very short text, or a tokenizer that
//! normalises text into more pieces than it had bytes. Settlement records the
//! real cost either way.
//!
//! **Prices** are USD per million input tokens ([`EmbedPrice`]). The built-in
//! table ([`VOYAGE_PRICES`]) is Voyage's list price, from the first token:
//! Voyage's free allowance is per account and not known here, so it is not
//! subtracted. An unknown Voyage model is priced as the dearest one listed. An
//! OpenAI-compatible model has no table: the operator prices it or marks the
//! provider free (`judge_bot::config`). A free model never reserves and is only
//! counted, so an exhausted cap does not refuse a local embedder.

use std::fmt;

use async_trait::async_trait;
use judge_core::{Embedder, InputKind, JudgeError};
use judge_llm::{LlmError, SpendMeter};

use crate::{Provider, Space, WithSpace, space::sealed};

/// Tokens allowed per text beyond its bytes: the provider's `input_type`
/// prompt (Voyage's longest is 57 bytes) plus start/end markers, with margin.
pub const TOKENS_PER_TEXT: u64 = 16;

/// What one billed request used, as the response reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedUsage {
    /// The provider's token count for the whole request.
    Tokens(u64),
    /// A 2xx body with no usage (some OpenAI-compatible servers): billed, at
    /// an unknown amount, so it settles at the reservation.
    Unreported,
}

/// A successful request: one vector per text, in input order, and its usage.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedded {
    /// The vectors.
    pub vectors: Vec<Vec<f32>>,
    /// What it used.
    pub usage: EmbedUsage,
}

/// A failed request, and whether the provider billed it: a 2xx body that did
/// not hold what was asked (a vector short, the wrong width) was still paid
/// for, a transport failure or an error status was not.
#[derive(Debug)]
pub struct EmbedError {
    /// The failure.
    pub error: JudgeError,
    /// `Some` when a 2xx response was received and so billed.
    pub billed: Option<EmbedUsage>,
}

impl From<JudgeError> for EmbedError {
    /// A failure before any response was billed.
    fn from(error: JudgeError) -> Self {
        Self {
            error,
            billed: None,
        }
    }
}

impl From<anyhow::Error> for EmbedError {
    fn from(error: anyhow::Error) -> Self {
        JudgeError::from(error).into()
    }
}

/// What a provider adapter implements: one request, its vectors and its
/// usage, in the space it writes into. Open, like `judge_llm::Backend`; the
/// cap belongs to [`MeteredEmbedder`], not here, and nothing past it accepts
/// a bare backend.
#[async_trait]
pub trait EmbedBackend: Send + Sync {
    /// Embed `texts` in order. Rejects an empty slice.
    async fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Embedded, EmbedError>;
    /// The space `embed` writes into.
    fn space(&self) -> &Space;
}

/// What an embedding model is billed at, in USD per million input tokens.
/// Mirrors `judge_llm::Price`: a closed sum, so "costs nothing" is a state the
/// cap understands rather than a bypass beside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmbedPrice {
    /// Never reserves; calls are still counted. A local server.
    Free,
    /// The built-in table's price ([`table_price`]).
    Table(f64),
    /// The operator's (`[models.embed.pricing]`): reserved and settled at exactly this.
    PerToken(f64),
}

impl EmbedPrice {
    /// USD per million tokens, `None` when free.
    #[must_use]
    pub const fn per_million(self) -> Option<f64> {
        match self {
            Self::Free => None,
            Self::Table(usd) | Self::PerToken(usd) => Some(usd),
        }
    }
}

/// Voyage's list prices, USD per million tokens, for the models its
/// `/v1/embeddings` endpoint serves (the contextualized and multimodal models
/// have endpoints of their own). Taken from <https://docs.voyageai.com/docs/pricing>
/// on 2026-10-09. Free allowances are ignored: the cap prices from the first
/// token. Re-check when a model is added or a price changes.
pub const VOYAGE_PRICES: &[(&str, f64)] = &[
    ("voyage-4-large", 0.12),
    ("voyage-4", 0.06),
    ("voyage-4-lite", 0.02),
    ("voyage-code-4", 0.12),
    ("voyage-3-large", 0.18),
    ("voyage-3.5", 0.06),
    ("voyage-3.5-lite", 0.02),
    ("voyage-code-3", 0.18),
    ("voyage-finance-2", 0.12),
    ("voyage-law-2", 0.12),
    ("voyage-code-2", 0.12),
    ("voyage-multilingual-2", 0.12),
    ("voyage-large-2-instruct", 0.12),
    ("voyage-large-2", 0.12),
    ("voyage-3", 0.06),
    ("voyage-3-lite", 0.02),
    ("voyage-2", 0.10),
];

/// The dearest price in [`VOYAGE_PRICES`]: what an unknown Voyage model is reserved and settled at.
pub const VOYAGE_UNKNOWN_MODEL_PRICE: f64 = 0.18;

/// The built-in price of `model` at a `provider` kind. Any Voyage model is
/// priced (an unknown one as [`VOYAGE_UNKNOWN_MODEL_PRICE`], the dearest
/// listed, so a new model is over-counted rather than under); an
/// OpenAI-compatible model is `None`, because it could be anything and the
/// operator must say.
#[must_use]
pub fn table_price(provider: Provider, model: &str) -> Option<f64> {
    match provider {
        Provider::Voyage => Some(
            VOYAGE_PRICES
                .iter()
                .find(|(m, _)| *m == model)
                .map_or(VOYAGE_UNKNOWN_MODEL_PRICE, |(_, usd)| *usd),
        ),
        Provider::OpenAi => None,
    }
}

/// The most tokens `texts` can be billed as: their UTF-8 bytes plus
/// [`TOKENS_PER_TEXT`] each (see the module docs).
#[must_use]
pub fn worst_case_tokens(texts: &[&str]) -> u64 {
    texts.iter().fold(0u64, |sum, t| {
        sum.saturating_add(u64::try_from(t.len()).unwrap_or(u64::MAX))
            .saturating_add(TOKENS_PER_TEXT)
    })
}

/// `tokens` at `per_million` USD per million.
#[must_use]
pub fn usd_for(tokens: u64, per_million: f64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "u64 -> f64 loses nothing below 2^53 tokens; billing estimates need no more"
    )]
    let tokens = tokens as f64;
    tokens / 1_000_000.0 * per_million
}

/// Whether `err` is the spend cap refusing a request (nothing was sent).
#[must_use]
pub fn is_spend_cap(err: &JudgeError) -> bool {
    match err {
        JudgeError::Upstream(e) => e.chain().any(|c| {
            matches!(
                c.downcast_ref::<LlmError>(),
                Some(LlmError::SpendCapExceeded { .. })
            )
        }),
        _ => false,
    }
}

/// An [`EmbedBackend`] behind the spend cap, and the only [`WithSpace`].
pub struct MeteredEmbedder<B> {
    inner: B,
    meter: SpendMeter,
    price: EmbedPrice,
}

impl<B: fmt::Debug> fmt::Debug for MeteredEmbedder<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeteredEmbedder")
            .field("inner", &self.inner)
            .field("meter", &self.meter)
            .field("price", &self.price)
            .finish()
    }
}

impl<B: EmbedBackend> MeteredEmbedder<B> {
    /// Put `inner` behind `meter` at the built-in table's price.
    ///
    /// # Errors
    /// [`LlmError::Unpriced`] for a model the table cannot price (any
    /// OpenAI-compatible one).
    pub fn new(inner: B, meter: SpendMeter) -> Result<Self, LlmError> {
        let space = inner.space();
        let usd = table_price(space.provider, &space.model).ok_or_else(|| LlmError::Unpriced {
            provider: space.provider.to_string(),
            model: space.model.clone(),
        })?;
        Ok(Self::priced(inner, meter, EmbedPrice::Table(usd)))
    }

    /// Put `inner` behind `meter` at `price`.
    #[must_use]
    pub fn priced(inner: B, meter: SpendMeter, price: EmbedPrice) -> Self {
        Self {
            inner,
            meter,
            price,
        }
    }

    /// The wrapped backend.
    #[must_use]
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// The cost of a billed request, logged; settles `reserved` (the
    /// reservation and the rate it was taken at) to it.
    fn record(
        &self,
        reserved: Option<(judge_llm::Reservation, f64)>,
        usage: EmbedUsage,
        texts: usize,
    ) {
        // A count of nothing for texts that were sent is not a measurement: a
        // server that fills the field with 0 is billed as one that leaves it out.
        let usage = match usage {
            EmbedUsage::Tokens(0) if texts > 0 => EmbedUsage::Unreported,
            other => other,
        };
        let (usd, total) = if let Some((r, rate)) = reserved {
            match usage {
                EmbedUsage::Tokens(t) => {
                    let usd = usd_for(t, rate);
                    (usd, r.settle_usd(usd))
                }
                EmbedUsage::Unreported => {
                    let usd = r.usd();
                    (usd, r.settle_reserved())
                }
            }
        } else {
            // Free: nothing reserved, the call is only counted.
            self.meter.count_free_embedding();
            (0.0, self.meter.spent_usd())
        };
        let space = self.inner.space();
        let tokens = match usage {
            EmbedUsage::Tokens(t) => Some(t),
            EmbedUsage::Unreported => None,
        };
        tracing::info!(
            provider = %space.provider,
            model = %space.model,
            texts,
            tokens,
            usd = format_args!("{usd:.6}"),
            cumulative_usd = format_args!("{total:.4}"),
            embedding_calls = self.meter.embedding_calls(),
            "embedding call"
        );
    }
}

#[async_trait]
impl<B: EmbedBackend> Embedder for MeteredEmbedder<B> {
    /// Reserve the worst case, send, settle to the reported usage.
    async fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Vec<Vec<f32>>, JudgeError> {
        let reserved = match self.price.per_million() {
            Some(rate) => Some((
                self.meter
                    .reserve_embedding_usd(usd_for(worst_case_tokens(texts), rate))
                    .map_err(anyhow::Error::from)?,
                rate,
            )),
            None => None,
        };
        match self.inner.embed(texts, kind).await {
            Ok(Embedded { vectors, usage }) => {
                self.record(reserved, usage, texts.len());
                Ok(vectors)
            }
            Err(EmbedError {
                error,
                billed: Some(usage),
            }) => {
                self.record(reserved, usage, texts.len());
                Err(error)
            }
            // Not billed: the reservation is handed back. (Dropped without
            // this, as by a cancelled future, it would be kept as the cost.)
            Err(EmbedError {
                error,
                billed: None,
            }) => {
                if let Some((r, _)) = reserved {
                    r.release();
                }
                Err(error)
            }
        }
    }

    fn dimensions(&self) -> usize {
        self.inner.space().dimensions
    }
}

impl<B: EmbedBackend> sealed::Sealed for MeteredEmbedder<B> {}

impl<B: EmbedBackend> WithSpace for MeteredEmbedder<B> {
    fn space(&self) -> &Space {
        self.inner.space()
    }

    fn price(&self) -> EmbedPrice {
        self.price
    }

    fn meter(&self) -> &SpendMeter {
        &self.meter
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// Answers with scripted outcomes and counts its sends.
    struct Stub {
        space: Space,
        sent: AtomicUsize,
        replies: Mutex<Vec<Result<EmbedUsage, Option<EmbedUsage>>>>,
    }

    fn stub(
        provider: Provider,
        model: &str,
        replies: Vec<Result<EmbedUsage, Option<EmbedUsage>>>,
    ) -> Stub {
        Stub {
            space: Space {
                provider,
                model: model.to_owned(),
                dimensions: 2,
            },
            sent: AtomicUsize::new(0),
            replies: Mutex::new(replies),
        }
    }

    #[async_trait]
    impl EmbedBackend for Stub {
        async fn embed(&self, texts: &[&str], _kind: InputKind) -> Result<Embedded, EmbedError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut r = self
                    .replies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if r.is_empty() { Err(None) } else { r.remove(0) }
            };
            match next {
                Ok(usage) => Ok(Embedded {
                    vectors: texts.iter().map(|_| vec![0.0; 2]).collect(),
                    usage,
                }),
                Err(billed) => Err(EmbedError {
                    error: anyhow::anyhow!("scripted failure").into(),
                    billed,
                }),
            }
        }
        fn space(&self) -> &Space {
            &self.space
        }
    }

    #[test]
    fn the_worst_case_is_bytes_plus_a_per_text_allowance() {
        assert_eq!(worst_case_tokens(&[]), 0);
        assert_eq!(worst_case_tokens(&["abcd"]), 4 + TOKENS_PER_TEXT);
        // Bytes, not characters: "é" is two bytes and may be two byte tokens.
        assert_eq!(worst_case_tokens(&["é", ""]), 2 + 2 * TOKENS_PER_TEXT);
        // It bounds a real count from above: Voyage counted this query at 8 tokens.
        assert!(worst_case_tokens(&["Sample text"]) >= 8);
        assert!((usd_for(1_000_000, 0.06) - 0.06).abs() < 1e-12);
    }

    #[test]
    fn voyage_models_are_tabled_and_an_unknown_one_is_priced_at_the_dearest() {
        assert_eq!(table_price(Provider::Voyage, "voyage-3.5"), Some(0.06));
        assert_eq!(table_price(Provider::Voyage, "voyage-3.5-lite"), Some(0.02));
        assert_eq!(table_price(Provider::Voyage, "voyage-3-large"), Some(0.18));
        let dearest = VOYAGE_PRICES
            .iter()
            .map(|(_, p)| *p)
            .fold(0.0_f64, f64::max);
        assert!((dearest - VOYAGE_UNKNOWN_MODEL_PRICE).abs() < f64::EPSILON);
        assert_eq!(
            table_price(Provider::Voyage, "voyage-9-ultra"),
            Some(VOYAGE_UNKNOWN_MODEL_PRICE)
        );
        assert_eq!(
            table_price(Provider::OpenAi, "text-embedding-3-small"),
            None
        );
        assert!(matches!(
            MeteredEmbedder::new(stub(Provider::OpenAi, "m", vec![]), SpendMeter::new()),
            Err(LlmError::Unpriced { .. })
        ));
    }

    #[tokio::test]
    async fn usage_settles_the_reservation_and_a_capped_meter_sends_nothing()
    -> Result<(), Box<dyn std::error::Error>> {
        let meter = SpendMeter::new().with_max_spend_usd(1.0)?;
        // $1 per million tokens: 500k reported tokens cost $0.50.
        let e = MeteredEmbedder::priced(
            stub(
                Provider::Voyage,
                "voyage-3.5",
                vec![Ok(EmbedUsage::Tokens(500_000))],
            ),
            meter.clone(),
            EmbedPrice::PerToken(1.0),
        );
        assert_eq!(e.embed(&["a", "b"], InputKind::Document).await?.len(), 2);
        assert!(
            (meter.spent_usd() - 0.5).abs() < 1e-9,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.embedding_calls(), 1);
        // A worst case of 600k tokens ($0.60) cannot fit in the $0.50 left: refused unsent.
        let big = "x".repeat(600_000);
        let err = e.embed(&[&big], InputKind::Document).await.err();
        assert!(err.as_ref().is_some_and(is_spend_cap), "{err:?}");
        assert_eq!(e.inner().sent.load(Ordering::SeqCst), 1, "never sent");
        assert_eq!((meter.embedding_refusals(), meter.refusals()), (1, 0));
        assert!(
            (meter.spent_usd() - 0.5).abs() < 1e-9,
            "the refusal reserved nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unreported_usage_keeps_the_reservation_and_an_unbilled_failure_frees_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let meter = SpendMeter::new();
        let e = MeteredEmbedder::priced(
            stub(
                Provider::OpenAi,
                "m",
                vec![
                    Ok(EmbedUsage::Unreported),
                    Err(None),
                    Err(Some(EmbedUsage::Tokens(1_000_000))),
                    Ok(EmbedUsage::Tokens(0)),
                ],
            ),
            meter.clone(),
            EmbedPrice::PerToken(2.0),
        );
        // 4 bytes + 16 = 20 tokens at $2/M.
        e.embed(&["abcd"], InputKind::Query).await?;
        let reserved = usd_for(20, 2.0);
        assert!(
            (meter.spent_usd() - reserved).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        // A failure nobody billed: released, not counted.
        assert!(e.embed(&["abcd"], InputKind::Query).await.is_err());
        assert!((meter.spent_usd() - reserved).abs() < 1e-6);
        assert_eq!(meter.embedding_calls(), 1);
        // A billed 2xx that failed afterwards is paid for at its usage.
        assert!(e.embed(&["abcd"], InputKind::Query).await.is_err());
        assert!(
            (meter.spent_usd() - reserved - 2.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.embedding_calls(), 2);
        // Zero tokens for a text that was sent is no measurement: billed at the reservation.
        e.embed(&["abcd"], InputKind::Query).await?;
        assert!(
            (meter.spent_usd() - 2.0 * reserved - 2.0).abs() < 1e-6,
            "{}",
            meter.spent_usd()
        );
        Ok(())
    }

    /// Never answers: a request whose future is dropped mid-flight.
    struct Hang(Space);

    #[async_trait]
    impl EmbedBackend for Hang {
        async fn embed(&self, _texts: &[&str], _kind: InputKind) -> Result<Embedded, EmbedError> {
            std::future::pending().await
        }
        fn space(&self) -> &Space {
            &self.0
        }
    }

    #[tokio::test]
    async fn a_cancelled_request_keeps_its_worst_case() -> Result<(), Box<dyn std::error::Error>> {
        let meter = SpendMeter::new();
        let e = MeteredEmbedder::priced(
            Hang(Space {
                provider: Provider::Voyage,
                model: "voyage-3.5".into(),
                dimensions: 2,
            }),
            meter.clone(),
            EmbedPrice::PerToken(2.0),
        );
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            e.embed(&["abcd"], InputKind::Query),
        )
        .await;
        assert!(cancelled.is_err(), "timed out");
        // The provider may have billed it: the cap keeps the worst case, as for chat.
        assert!(
            (meter.spent_usd() - usd_for(20, 2.0)).abs() < 1e-9,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.embedding_calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_free_embedder_is_counted_but_never_capped() -> Result<(), Box<dyn std::error::Error>>
    {
        let meter = SpendMeter::new().with_max_spend_usd(0.0)?;
        let e = MeteredEmbedder::priced(
            stub(
                Provider::OpenAi,
                "nomic",
                vec![Ok(EmbedUsage::Tokens(5_000_000))],
            ),
            meter.clone(),
            EmbedPrice::Free,
        );
        e.embed(&["q"], InputKind::Query).await?;
        assert_eq!(meter.embedding_calls(), 1);
        assert!(meter.spent_usd().abs() < f64::EPSILON);
        assert_eq!(e.price(), EmbedPrice::Free);
        assert_eq!(e.dimensions(), 2);
        Ok(())
    }
}
