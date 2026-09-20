//! The Discord adapter (build-order step 6): the `/judge` slash command,
//! rating buttons and the "did you mean…?" flow, on poise 0.7 / serenity 0.12.
//!
//! Everything that does not need the gateway lives in a pure submodule:
//! [`render`] (verdict / error → text), [`mana`] (card symbols → the bot's
//! custom emoji), [`ids`] (typed `custom_id`s), [`pending`] (questions waiting
//! on a card pick), [`question`] (span replacement) and [`capture`] (keeps the
//! retrieval `Context` so a call can be persisted). This file is the glue:
//! serenity types in, those modules out.
//!
//! Flows:
//! * `/judge question:<text>` → defer → `judge()` with the thread's last N
//!   Q&A → persist → answer + three rating buttons. Beyond
//!   [`Config::max_concurrent`] runs in flight a caller waits up to
//!   [`ACQUIRE_WAIT`] for a slot (FIFO), then gets "busy".
//! * Rating button → deferred ephemeral ack → `CallStore::rate` (`is_judge` =
//!   the member holds the [`Config::judge_role`] role, looked up over HTTP)
//!   → ephemeral follow-up. The ack comes first so a slow role lookup or DB
//!   cannot blow Discord's 3 s deadline.
//! * `AmbiguousCards` → the question is parked in [`PendingStore`] and up to
//!   five "did you mean…?" buttons are offered for the first ambiguous span.
//!   A pick (by the asker only) rewrites that span as `[[Full Name]]` and
//!   re-runs `judge()`, editing the same message; another ambiguous span
//!   simply starts the loop again.
//! * `/judge … private:True` → the same, shown to the asker alone
//!   ([`Audience::Private`]): no thread history read, nothing persisted, so
//!   no rating buttons and no trace in the channel's follow-ups.
//! * Each user gets [`Config::user_limit`] questions per window, charged once
//!   ([`cooldown`]); a pick re-runs a question already counted and is free.
//! * `/card` and `/rule` read the database and call no model.

pub mod capture;
pub mod cooldown;
pub mod ids;
pub mod mana;
pub mod pending;
pub mod question;
pub mod render;

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use judge_core::{
    CallId, CallStore, Deps, DiscordOperator, JudgeError, Question, Resolution, Retriever, RuleId,
    Score, SourceOffer, Validated, Verdict, judge,
};
use judge_llm::{ApiKey, SpendMeter};
use poise::serenity_prelude as serenity;
use serenity::{
    ButtonStyle, ComponentInteraction, CreateActionRow, CreateAllowedMentions, CreateButton,
    CreateEmbed, CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, EditInteractionResponse, FullEvent, GatewayIntents, GuildId,
    Interaction, Member, UserId,
};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::db::PgLibrary;
use capture::CapturingRetriever;
use cooldown::{Cooldowns, UserLimit};
use ids::ButtonAction;
use mana::SymbolTable;
use pending::{Pending, PendingId, PendingSpan, PendingStore, TakeError};

/// How long a `/judge` (or a card pick) waits for a free slot before replying
/// "busy". Under Discord's 3 s initial-response deadline, so short bursts queue
/// instead of being refused outright.
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(2);

/// Everything the adapter reads from the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bot token (`DISCORD_TOKEN`). An [`ApiKey`] so a `{:?}` of this
    /// struct (a log line, a panic, a test) prints `<redacted>`, never the token.
    pub token: ApiKey,
    /// Register `/judge` in this guild only (`GUILD_ID`; instant) instead of
    /// globally (up to an hour to propagate).
    pub guild_id: Option<GuildId>,
    /// Name of the role whose members' ratings count as judge rulings (`JUDGE_ROLE`).
    pub judge_role: String,
    /// Most `judge()` runs in flight at once (`JUDGE_CONCURRENCY`).
    pub max_concurrent: usize,
    /// Thread Q&A pairs handed to `judge()` as history.
    pub history_len: usize,
    /// How long a "did you mean…?" stays answerable.
    pub pending_ttl: Duration,
    /// `/judge` questions one user may ask per window (`JUDGE_USER_LIMIT` per
    /// `JUDGE_USER_WINDOW_SECS`); `None` is no per-user limit.
    pub user_limit: Option<UserLimit>,
}

/// Who sees a `/judge` reply.
///
/// Discord fixes this when the command is acknowledged, so it is chosen up
/// front and carried through a "did you mean…?" pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// The channel: thread history in, the call persisted, rating buttons.
    Channel,
    /// The asker alone. The question stands by itself: it reads no thread
    /// history and is never persisted, so it cannot be rated, cannot become a
    /// prior call and cannot surface in anyone's follow-up.
    Private,
}

impl Audience {
    const fn from_option(private: Option<bool>) -> Self {
        match private {
            Some(true) => Self::Private,
            Some(false) | None => Self::Channel,
        }
    }

    const fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }

    /// The call store as this audience may use it: not at all, for a private
    /// question.
    fn record(self, store: &dyn CallStore) -> Option<&dyn CallStore> {
        match self {
            Self::Channel => Some(store),
            Self::Private => None,
        }
    }
}

impl Config {
    /// `JUDGE_ROLE` default.
    pub const DEFAULT_JUDGE_ROLE: &'static str = "Judge";
    /// `JUDGE_CONCURRENCY` default.
    pub const DEFAULT_CONCURRENCY: usize = 2;
    /// Thread history length.
    pub const DEFAULT_HISTORY: usize = 5;
    /// `JUDGE_USER_LIMIT` default.
    pub const DEFAULT_USER_LIMIT: u32 = 6;
    /// `JUDGE_USER_WINDOW_SECS` default.
    pub const DEFAULT_USER_WINDOW: Duration = Duration::from_mins(10);

    /// Read the process environment. See [`Self::from_vars`].
    ///
    /// # Errors
    /// As [`Self::from_vars`].
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Build from a variable lookup: `DISCORD_TOKEN` (required, non-blank),
    /// `GUILD_ID` (optional, non-zero integer), `JUDGE_ROLE` (default
    /// [`Self::DEFAULT_JUDGE_ROLE`]), `JUDGE_CONCURRENCY` (default
    /// [`Self::DEFAULT_CONCURRENCY`], at least 1), `JUDGE_USER_LIMIT`
    /// (default [`Self::DEFAULT_USER_LIMIT`]; `0` turns the per-user limit
    /// off) and `JUDGE_USER_WINDOW_SECS` (default
    /// [`Self::DEFAULT_USER_WINDOW`], at least 1). Blank values count as unset.
    ///
    /// # Errors
    /// A missing token, or a malformed `GUILD_ID`, `JUDGE_CONCURRENCY`,
    /// `JUDGE_USER_LIMIT` or `JUDGE_USER_WINDOW_SECS`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let token = var("DISCORD_TOKEN").map(ApiKey::from).ok_or_else(|| {
            anyhow::anyhow!(
                "DISCORD_TOKEN is not set: put the bot token in the environment (or .env) and retry"
            )
        })?;
        let guild_id = var("GUILD_ID")
            .map(|g| {
                g.parse::<NonZeroU64>()
                    .map(GuildId::from)
                    .with_context(|| format!("GUILD_ID must be a non-zero integer, got {g:?}"))
            })
            .transpose()?;
        let judge_role = var("JUDGE_ROLE").unwrap_or_else(|| Self::DEFAULT_JUDGE_ROLE.to_owned());
        let max_concurrent = var("JUDGE_CONCURRENCY")
            .map(|c| {
                c.parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .with_context(|| {
                        format!("JUDGE_CONCURRENCY must be an integer >= 1, got {c:?}")
                    })
            })
            .transpose()?
            .unwrap_or(Self::DEFAULT_CONCURRENCY);
        let user_max = var("JUDGE_USER_LIMIT")
            .map(|n| {
                n.parse::<u32>().with_context(|| {
                    format!("JUDGE_USER_LIMIT must be an integer >= 0 (0 = no limit), got {n:?}")
                })
            })
            .transpose()?
            .unwrap_or(Self::DEFAULT_USER_LIMIT);
        let user_window = var("JUDGE_USER_WINDOW_SECS")
            .map(|n| {
                n.parse::<u64>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .map(Duration::from_secs)
                    .with_context(|| {
                        format!("JUDGE_USER_WINDOW_SECS must be an integer >= 1, got {n:?}")
                    })
            })
            .transpose()?
            .unwrap_or(Self::DEFAULT_USER_WINDOW);
        let user_limit = NonZeroU32::new(user_max).map(|max| UserLimit {
            max,
            window: user_window,
        });
        Ok(Self {
            token,
            guild_id,
            judge_role,
            max_concurrent,
            history_len: Self::DEFAULT_HISTORY,
            pending_ttl: pending::DEFAULT_TTL,
            user_limit,
        })
    }
}

/// Shared state behind every command and button (poise's user data).
pub struct Data {
    deps: Deps,
    capture: Arc<CapturingRetriever>,
    store: Arc<dyn CallStore>,
    meter: SpendMeter,
    pending: PendingStore,
    permits: Semaphore,
    judge_role: String,
    history_len: usize,
    /// Card-symbol emoji, filled in on `Ready` by [`run`]; empty until then
    /// (and for good, if this application has none uploaded).
    symbols: SymbolTable,
    /// What `/help` and `/license` say about where this instance's source is.
    offer: SourceOffer,
    /// Whom `/help` and `/license` tell users to contact.
    operator: DiscordOperator,
    /// Per-user `/judge` windows; `None` when the operator turned the limit off.
    cooldowns: Option<Cooldowns>,
    /// Rulings for `/card`. The resolver and retriever in `deps` serve the
    /// rest of the lookups.
    library: PgLibrary,
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Data")
            .field("pending", &self.pending)
            .field("permits", &self.permits.available_permits())
            .field("judge_role", &self.judge_role)
            .field("history_len", &self.history_len)
            .finish_non_exhaustive()
    }
}

type Error = anyhow::Error;
type Ctx<'a> = poise::Context<'a, Data, Error>;

impl Data {
    /// Wire the shared state. `meter` must be the one the models inside
    /// `deps` bill to, so its counters reflect the judge runs.
    #[must_use]
    pub fn new(
        mut deps: Deps,
        store: Arc<dyn CallStore>,
        meter: SpendMeter,
        cfg: &Config,
        offer: SourceOffer,
        operator: DiscordOperator,
        library: PgLibrary,
    ) -> Self {
        let capture = Arc::new(CapturingRetriever::new(Arc::clone(&deps.retriever)));
        deps.retriever = Arc::clone(&capture) as Arc<dyn Retriever>;
        Self {
            deps,
            capture,
            store,
            meter,
            pending: PendingStore::new(cfg.pending_ttl),
            permits: Semaphore::new(cfg.max_concurrent),
            judge_role: cfg.judge_role.clone(),
            history_len: cfg.history_len,
            symbols: SymbolTable::empty(),
            offer,
            operator,
            cooldowns: cfg.user_limit.map(Cooldowns::new),
            library,
        }
    }

    /// A judge slot, waiting up to [`ACQUIRE_WAIT`] for one (tokio's
    /// semaphore hands permits out FIFO); `None` means "busy".
    async fn acquire(&self) -> Option<SemaphorePermit<'_>> {
        match tokio::time::timeout(ACQUIRE_WAIT, self.permits.acquire()).await {
            Ok(Ok(p)) => Some(p),
            // The semaphore is never closed; a timeout is the only real `None`.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Run the pipeline for `q` and build the reply: answer with rating
    /// buttons, "did you mean…?" with pick buttons, or an error message.
    async fn answer(&self, q: &Question, asker: UserId, audience: Audience) -> Outgoing {
        // The only route to the store in here: `None` for a private question,
        // so it reads no history and cannot be persisted.
        let record = audience.record(&*self.store);
        let history = match record {
            None => vec![],
            Some(store) => match store.history(&q.thread_id, self.history_len).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(
                        error = format_args!("{e:#}"),
                        "thread history unavailable; judging without it"
                    );
                    vec![]
                }
            },
        };
        let (t0, usd0, calls0) = (Instant::now(), self.meter.spent_usd(), self.meter.calls());
        let result = judge(&self.deps, q, &history).await;
        let captured = self.capture.take(q);
        tracing::info!(
            user = %asker,
            thread = %q.thread_id,
            elapsed_ms = t0.elapsed().as_millis(),
            usd = format_args!("{:.4}", self.meter.spent_usd() - usd0),
            llm_calls = self.meter.calls() - calls0,
            outcome = outcome(&result),
            private = audience.is_private(),
            "/judge"
        );
        match result {
            Ok(v) => {
                // No record, no call id, and so no rating buttons.
                let call = match (record, captured.as_ref()) {
                    (None, _) => None,
                    (Some(store), Some(ctx)) => store
                        .persist(q, &v, ctx)
                        .await
                        .map_err(|e| {
                            tracing::error!(
                                error = format_args!("{e:#}"),
                                "persist failed; answering without rating buttons"
                            );
                        })
                        .ok(),
                    (Some(_), None) => {
                        tracing::warn!("no captured context for the question; call not persisted");
                        None
                    }
                };
                Outgoing::answer(&v, captured.as_ref(), call, asker, &q.text, &self.symbols)
            }
            Err(JudgeError::AmbiguousCards(spans)) => {
                let dym = render::did_you_mean(&spans);
                let token = self.pending.insert(Pending {
                    thread_id: q.thread_id.clone(),
                    user_id: asker.to_string(),
                    text: q.text.clone(),
                    spans: spans.map(PendingSpan::from),
                    audience,
                });
                Outgoing {
                    content: render::with_header(asker.get(), &q.text, &dym.content, &self.symbols),
                    embed: None,
                    components: vec![pick_row(token, &dym.choices)],
                }
            }
            Err(e) => {
                // Same classification as the HTTP front door, from the same
                // place: an unknown card or an out-of-scope question is the
                // pipeline working, and says all it needs to in the reply.
                if e.is_operator_failure() {
                    tracing::warn!(error = format_args!("{e:#}"), "judge failed");
                } else {
                    tracing::debug!(
                        error = format_args!("{e:#}"),
                        "question answered with a non-verdict reply"
                    );
                }
                Outgoing {
                    content: render::with_header(
                        asker.get(),
                        &q.text,
                        &render::error(&e),
                        &self.symbols,
                    ),
                    embed: None,
                    components: vec![],
                }
            }
        }
    }

    async fn on_component(
        &self,
        ctx: &serenity::Context,
        c: &ComponentInteraction,
    ) -> anyhow::Result<()> {
        match ButtonAction::parse(&c.data.custom_id) {
            Ok(ButtonAction::Rate { call, score }) => self.on_rate(ctx, c, call, score).await,
            Ok(ButtonAction::PickCard { token, choice }) => {
                self.on_pick(ctx, c, token, choice).await
            }
            Err(e) => {
                tracing::warn!(custom_id = %c.data.custom_id, error = %e, "unparseable button");
                ephemeral(ctx, c, render::UNKNOWN_BUTTON).await
            }
        }
    }

    async fn on_rate(
        &self,
        ctx: &serenity::Context,
        c: &ComponentInteraction,
        call: CallId,
        score: Score,
    ) -> anyhow::Result<()> {
        // Acknowledge before the role lookup (HTTP) and the upsert (DB): both
        // can outlast Discord's 3 s deadline under rate limiting or load.
        let ack = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        c.create_response(&ctx.http, ack)
            .await
            .context("acknowledge rating")?;
        let is_judge = match (c.guild_id, c.member.as_ref()) {
            (Some(guild), Some(member)) => has_role(ctx, guild, member, &self.judge_role).await,
            _ => false,
        };
        let user_id = c.user.id.to_string();
        let text = match self.store.rate(call, &user_id, score, is_judge).await {
            Ok(()) => {
                tracing::info!(%call, user = %user_id, score = score as u8, is_judge, "rated");
                render::rated(score, is_judge, &self.offer, &self.operator)
            }
            Err(e) => {
                tracing::error!(%call, error = format_args!("{e:#}"), "rate failed");
                render::RATE_FAILED.to_owned()
            }
        };
        ephemeral_followup(ctx, c, &text).await
    }

    async fn on_pick(
        &self,
        ctx: &serenity::Context,
        c: &ComponentInteraction,
        token: PendingId,
        choice: u8,
    ) -> anyhow::Result<()> {
        // Take the permit before the pending entry, so a "busy" leaves the buttons usable.
        let Some(_permit) = self.acquire().await else {
            return ephemeral(ctx, c, render::BUSY).await;
        };
        let pending = match self.pending.take_for(token, &c.user.id.to_string()) {
            Ok(p) => p,
            Err(TakeError::Missing) => return ephemeral(ctx, c, render::EXPIRED).await,
            Err(TakeError::NotOwner) => return ephemeral(ctx, c, render::NOT_YOURS).await,
        };
        let span = pending.spans.first();
        // Only the first MAX_CHOICES candidates were ever offered as buttons.
        let name = match usize::from(choice) {
            i if i < render::MAX_CHOICES => span.candidates.get(i),
            _ => None,
        };
        let Some(name) = name else {
            return ephemeral(ctx, c, render::UNKNOWN_BUTTON).await;
        };
        // Replace the "did you mean…?" in place, then edit it again with the verdict.
        let working = CreateInteractionResponseMessage::new()
            .content(render::working(name))
            .components(vec![])
            .embeds(vec![]);
        c.create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(working))
            .await
            .context("acknowledge pick")?;
        let q = Question {
            thread_id: pending.thread_id.clone(),
            text: question::pin_card(&pending.text, &span.query, name),
        };
        let out = self.answer(&q, c.user.id, pending.audience).await;
        if let Err(e) = c.edit_response(&ctx.http, out.into_edit()).await {
            // The pending entry is gone and the message shows "Working on it…"
            // with no buttons: tell the asker (ephemerally) to ask again rather
            // than leave them waiting on a message that will never change.
            tracing::error!(error = %e, "edit picked reply failed; asking the user to re-ask");
            ephemeral_followup(ctx, c, render::EDIT_FAILED)
                .await
                .context("report failed edit after pick")?;
            return Err(anyhow::Error::from(e).context("edit picked reply"));
        }
        Ok(())
    }
}

/// A reply before it is shaped for poise (`CreateReply`) or serenity (`EditInteractionResponse`).
struct Outgoing {
    content: String,
    embed: Option<CreateEmbed>,
    components: Vec<CreateActionRow>,
}

/// Allowed-mentions with an empty parse list and no explicit users/roles:
/// the `<@asker>` in the restated question renders as a mention but pings
/// nobody.
fn no_pings() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
}

impl Outgoing {
    fn answer(
        v: &Verdict<Validated>,
        ctx: Option<&judge_core::Context>,
        call: Option<CallId>,
        asker: UserId,
        question: &str,
        symbols: &SymbolTable,
    ) -> Self {
        let a = render::answer(v, ctx, asker.get(), question, symbols);
        let mut embed = CreateEmbed::new().footer(CreateEmbedFooter::new(a.footer));
        if !a.citations.is_empty() {
            embed = embed.description(a.citations);
        }
        Self {
            content: a.content,
            embed: Some(embed),
            components: call.map(rating_row).into_iter().collect(),
        }
    }

    fn into_reply(self) -> poise::CreateReply {
        let mut r = poise::CreateReply::default()
            .content(self.content)
            .components(self.components)
            .allowed_mentions(no_pings());
        if let Some(e) = self.embed {
            r = r.embed(e);
        }
        r
    }

    fn into_edit(self) -> EditInteractionResponse {
        EditInteractionResponse::new()
            .content(self.content)
            .components(self.components)
            .embeds(self.embed.into_iter().collect())
            .allowed_mentions(no_pings())
    }
}

fn rating_row(call: CallId) -> CreateActionRow {
    let button = |score: Score, style: ButtonStyle| {
        CreateButton::new(ButtonAction::Rate { call, score }.to_custom_id())
            .label(render::rating_label(score))
            .style(style)
    };
    CreateActionRow::Buttons(vec![
        button(Score::Incorrect, ButtonStyle::Danger),
        button(Score::Partial, ButtonStyle::Secondary),
        button(Score::Correct, ButtonStyle::Success),
    ])
}

fn pick_row(token: PendingId, choices: &[String]) -> CreateActionRow {
    let buttons = choices
        .iter()
        .take(render::MAX_CHOICES)
        .enumerate()
        .filter_map(|(i, name)| {
            let choice = u8::try_from(i).ok()?;
            Some(
                CreateButton::new(ButtonAction::PickCard { token, choice }.to_custom_id())
                    .label(render::button_label(name))
                    .style(ButtonStyle::Primary),
            )
        })
        .collect();
    CreateActionRow::Buttons(buttons)
}

const fn outcome(r: &Result<Verdict<Validated>, JudgeError>) -> &'static str {
    match r {
        Ok(_) => "answered",
        Err(JudgeError::AmbiguousCards(_)) => "ambiguous",
        Err(JudgeError::CardsNotFound(_)) => "not_found",
        Err(JudgeError::OutOfScope(_)) => "out_of_scope",
        Err(JudgeError::BadCitation(_)) => "bad_citation",
        Err(JudgeError::MalformedCitation(_)) => "malformed_citation",
        Err(JudgeError::EmptyVerdict(_)) => "empty_verdict",
        Err(JudgeError::LlmRefused) => "refused",
        Err(JudgeError::Upstream(_)) => "upstream",
    }
}

async fn ephemeral(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    text: &str,
) -> anyhow::Result<()> {
    let msg = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(true);
    c.create_response(&ctx.http, CreateInteractionResponse::Message(msg))
        .await
        .context("respond to button")
}

/// An ephemeral follow-up after a deferred acknowledgement.
async fn ephemeral_followup(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    text: &str,
) -> anyhow::Result<()> {
    let msg = CreateInteractionResponseFollowup::new()
        .content(text)
        .ephemeral(true);
    c.create_followup(&ctx.http, msg)
        .await
        .map(drop)
        .context("follow up on button")
}

/// Whether `member` holds a role named `role_name` in `guild`. Looked up over
/// HTTP each time (the gateway runs with no intents, so the cache is empty);
/// a lookup failure counts as "not a judge" and is logged.
async fn has_role(
    ctx: &serenity::Context,
    guild: GuildId,
    member: &Member,
    role_name: &str,
) -> bool {
    match guild.roles(&ctx.http).await {
        Ok(roles) => roles
            .values()
            .any(|r| r.name == role_name && member.roles.contains(&r.id)),
        Err(e) => {
            tracing::warn!(%guild, error = %e, "could not fetch guild roles; treating rater as non-judge");
            false
        }
    }
}

/// Ask a Magic rules question; the answer cites the Comprehensive Rules.
#[poise::command(slash_command, rename = "judge", guild_only)]
async fn judge_command(
    ctx: Ctx<'_>,
    #[description = "Your rules question. Use brackets like [[Full Card Name]] to avoid ambiguity."]
    question: String,
    #[description = "Show the answer only to you. A private answer stands alone: no follow-ups, no ratings, not saved."]
    private: Option<bool>,
) -> Result<(), Error> {
    let data = ctx.data();
    let audience = Audience::from_option(private);
    let Some(_permit) = data.acquire().await else {
        ctx.send(
            poise::CreateReply::default()
                .content(render::BUSY)
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };
    // After the slot, so a "busy" costs the asker none of their allowance.
    // Everything past here counts, answered or not: an ambiguous or unknown
    // card has already paid for an extraction call.
    if let Some(cooldowns) = &data.cooldowns
        && let Err(wait) = cooldowns.take(ctx.author().id.get(), Instant::now())
    {
        ctx.send(
            poise::CreateReply::default()
                .content(render::cooling_down(cooldowns.limit(), wait))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }
    // Answers take 20–45 s; Discord wants an acknowledgement within 3 s. The
    // acknowledgement is also where Discord fixes who sees the reply.
    match audience {
        Audience::Channel => ctx.defer().await?,
        Audience::Private => ctx.defer_ephemeral().await?,
    }
    let q = Question {
        thread_id: ctx.channel_id().to_string(),
        text: question,
    };
    let out = data.answer(&q, ctx.author().id, audience).await;
    ctx.send(out.into_reply().ephemeral(audience.is_private()))
        .await?;
    Ok(())
}

/// Look up a card: Oracle text and rulings. Free: no model is called.
#[poise::command(slash_command, rename = "card")]
async fn card_command(
    ctx: Ctx<'_>,
    #[description = "A card name or nickname."] name: String,
    #[description = "Show the card only to you."] private: Option<bool>,
) -> Result<(), Error> {
    let data = ctx.data();
    let private = Audience::from_option(private).is_private();
    let name = name.trim();
    if name.is_empty() || name.chars().count() > render::LOOKUP_INPUT_LIMIT {
        return say(ctx, render::LOOKUP_BAD_NAME, true).await;
    }
    // The resolver's fuzzy rung and a cold pool can outlast Discord's 3 s.
    defer(ctx, private).await?;
    let resolution = match data.deps.resolver.resolve(name).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = format_args!("{e:#}"), "/card resolve failed");
            return say(ctx, render::LOOKUP_FAILED, true).await;
        }
    };
    match resolution {
        Resolution::Resolved { card, .. } => {
            let rulings = data.library.rulings(card.id).await.unwrap_or_else(|e| {
                tracing::warn!(error = format_args!("{e:#}"), "/card rulings unavailable");
                vec![]
            });
            let view = render::card(&card, &rulings, &data.symbols);
            let embed = lookup_embed(view);
            ctx.send(
                poise::CreateReply::default()
                    .embed(embed)
                    .ephemeral(private)
                    .allowed_mentions(no_pings()),
            )
            .await?;
            Ok(())
        }
        Resolution::Ambiguous { candidates, .. } => {
            let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
            say(ctx, &render::card_ambiguous(name, &names), true).await
        }
        Resolution::NotFound { .. } => say(ctx, &render::card_not_found(name), true).await,
    }
}

/// Look up Comprehensive Rules text by number. Free: no model is called.
#[poise::command(slash_command, rename = "rule")]
async fn rule_command(
    ctx: Ctx<'_>,
    #[description = "A rule number such as 702.19, 702.19b or 702."] id: String,
    #[description = "Show the rule only to you."] private: Option<bool>,
) -> Result<(), Error> {
    let data = ctx.data();
    let private = Audience::from_option(private).is_private();
    let Ok(rule) = RuleId::try_new(id.trim().trim_end_matches('.')) else {
        return say(ctx, render::LOOKUP_BAD_RULE, true).await;
    };
    defer(ctx, private).await?;
    let chunks = match data
        .deps
        .retriever
        .lookup_rules(std::slice::from_ref(&rule))
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = format_args!("{e:#}"), "/rule lookup failed");
            return say(ctx, render::LOOKUP_FAILED, true).await;
        }
    };
    let Some(view) = render::rules(&rule, &chunks, &data.symbols) else {
        return say(ctx, &render::rule_not_found(&rule), true).await;
    };
    let embed = lookup_embed(view);
    ctx.send(
        poise::CreateReply::default()
            .embed(embed)
            .ephemeral(private)
            .allowed_mentions(no_pings()),
    )
    .await?;
    Ok(())
}

/// Acknowledge a lookup. Discord fixes the reply's visibility here, so what
/// follows (the result, or "not found") is seen by whoever `private` says.
async fn defer(ctx: Ctx<'_>, private: bool) -> Result<(), Error> {
    if private {
        ctx.defer_ephemeral().await?;
    } else {
        ctx.defer().await?;
    }
    Ok(())
}

fn lookup_embed(view: render::Lookup) -> CreateEmbed {
    let embed = CreateEmbed::new()
        .title(view.title)
        .url(view.url)
        .description(view.description);
    match view.footer {
        Some(f) => embed.footer(CreateEmbedFooter::new(f)),
        None => embed,
    }
}

/// A plain text reply, pinging nobody.
async fn say(ctx: Ctx<'_>, text: &str, ephemeral: bool) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .content(text)
            .ephemeral(ephemeral)
            .allowed_mentions(no_pings()),
    )
    .await?;
    Ok(())
}

/// What the bot does, how to ask, what it stores, and where its source is.
#[poise::command(slash_command, rename = "help", ephemeral)]
async fn help_command(ctx: Ctx<'_>) -> Result<(), Error> {
    // Two ephemeral messages: the second is a follow-up to the first.
    for part in render::help(&ctx.data().offer, &ctx.data().operator) {
        ctx.say(part).await?;
    }
    Ok(())
}

/// Where this bot's source code is, at which commit, and under which licence.
#[poise::command(slash_command, rename = "license", ephemeral)]
async fn license_command(ctx: Ctx<'_>) -> Result<(), Error> {
    ctx.say(render::license(&ctx.data().offer, &ctx.data().operator))
        .await?;
    Ok(())
}

/// Delete every rating you have recorded; nothing else is stored about you.
#[poise::command(slash_command, rename = "forget", ephemeral)]
async fn forget_command(ctx: Ctx<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id.to_string();
    // The id stays out of the log: the point of the command is to stop
    // keeping it.
    let text = match ctx.data().store.forget_user(&user_id).await {
        Ok(n) => {
            tracing::info!(ratings = n, "forgot a user's ratings");
            render::forgotten(n)
        }
        Err(e) => {
            tracing::error!(error = format_args!("{e:#}"), "forget failed");
            render::FORGET_FAILED.to_owned()
        }
    };
    ctx.say(text).await?;
    Ok(())
}

async fn event_handler(
    framework: poise::FrameworkContext<'_, Data, Error>,
    event: &FullEvent,
) -> Result<(), Error> {
    let (ctx, data) = (framework.serenity_context, framework.user_data);
    match event {
        FullEvent::Ready { data_about_bot } => {
            tracing::info!(user = %data_about_bot.user.name, guilds = data_about_bot.guilds.len(), "connected to Discord");
        }
        FullEvent::InteractionCreate {
            interaction: Interaction::Component(c),
        } => data.on_component(ctx, c).await?,
        _ => {}
    }
    Ok(())
}

async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::error!(
                command = &*ctx.command().name,
                error = format_args!("{error:#}"),
                "command failed"
            );
            if let Err(e) = ctx
                .send(poise::CreateReply::default().content(render::FAILED))
                .await
            {
                tracing::error!(error = %e, "could not report the failure to the user");
            }
        }
        poise::FrameworkError::EventHandler { error, event, .. } => {
            tracing::error!(
                event = event.snake_case_name(),
                error = format_args!("{error:#}"),
                "event handler failed"
            );
        }
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                tracing::error!(error = %e, "error while handling an error");
            }
        }
    }
}

/// The application's card-symbol emoji, for [`render::answer`].
///
/// Never fails the startup: Discord being unreachable or the emoji never
/// having been uploaded (see the `judge-ingest emoji` subcommand) both leave an
/// empty table, and symbols then render as the literal `{W}` Scryfall writes.
async fn load_symbols(http: &serenity::Http) -> SymbolTable {
    match http.get_application_emojis().await {
        Ok(emojis) => {
            let table = SymbolTable::new(emojis.into_iter().map(|e| (e.name, e.id.get())));
            if table.is_empty() {
                tracing::warn!(
                    "no `{}…` application emoji found; card symbols will render as text \
                     (run `judge-ingest emoji` to upload them)",
                    judge_core::symbol::NAME_PREFIX
                );
            } else {
                tracing::info!(symbols = table.len(), "loaded card-symbol emoji");
            }
            table
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not list application emoji; card symbols will render as text"
            );
            SymbolTable::empty()
        }
    }
}

/// Connect to the gateway and serve until the connection ends.
///
/// # Errors
/// Building the client (bad token) or a fatal gateway error.
pub async fn run(cfg: Config, data: Data) -> anyhow::Result<()> {
    let guild = cfg.guild_id;
    let options = poise::FrameworkOptions {
        commands: vec![
            judge_command(),
            card_command(),
            rule_command(),
            help_command(),
            license_command(),
            forget_command(),
        ],
        event_handler: |framework, event| Box::pin(event_handler(framework, event)),
        on_error: |e| Box::pin(on_error(e)),
        ..Default::default()
    };
    let framework = poise::Framework::builder()
        .options(options)
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                let mut data = data;
                let commands = &framework.options().commands;
                if let Some(g) = guild {
                    poise::builtins::register_in_guild(ctx, commands, g).await?;
                    tracing::info!(guild = %g, "registered /judge, /card, /rule, /help, /license and /forget in one guild");
                } else {
                    poise::builtins::register_globally(ctx, commands).await?;
                    tracing::info!(
                        "registered /judge, /card, /rule, /help, /license and /forget globally (propagation can take up to an hour)"
                    );
                }
                // The application id arrives with `Ready`, which is what got us
                // here, so `ctx.http` can answer this now.
                data.symbols = load_symbols(&ctx.http).await;
                Ok(data)
            })
        })
        .build();
    // Slash commands and button presses arrive without any gateway intent.
    let mut client = serenity::ClientBuilder::new(cfg.token.expose(), GatewayIntents::empty())
        .framework(framework)
        .await
        .context("build Discord client")?;
    client.start().await.context("Discord gateway")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_per_user_limit_defaults_on_and_zero_turns_it_off() -> anyhow::Result<()> {
        let cfg = Config::from_vars(vars(&[("DISCORD_TOKEN", "t")]))?;
        assert_eq!(
            cfg.user_limit.map(|l| (l.max.get(), l.window)),
            Some((Config::DEFAULT_USER_LIMIT, Config::DEFAULT_USER_WINDOW))
        );
        let cfg = Config::from_vars(vars(&[
            ("DISCORD_TOKEN", "t"),
            ("JUDGE_USER_LIMIT", "2"),
            ("JUDGE_USER_WINDOW_SECS", "60"),
        ]))?;
        assert_eq!(
            cfg.user_limit.map(|l| (l.max.get(), l.window)),
            Some((2, Duration::from_mins(1)))
        );
        let off = Config::from_vars(vars(&[("DISCORD_TOKEN", "t"), ("JUDGE_USER_LIMIT", "0")]))?;
        assert_eq!(off.user_limit, None);
        for (k, v) in [
            ("JUDGE_USER_LIMIT", "-1"),
            ("JUDGE_USER_LIMIT", "many"),
            ("JUDGE_USER_WINDOW_SECS", "0"),
        ] {
            let r = Config::from_vars(vars(&[("DISCORD_TOKEN", "t"), (k, v)]));
            assert!(r.is_err_and(|e| e.to_string().contains(k)), "{k}={v}");
        }
        Ok(())
    }

    /// A store that must never be reached.
    struct Untouchable;

    #[async_trait::async_trait]
    impl CallStore for Untouchable {
        async fn persist(
            &self,
            _: &Question,
            _: &Verdict<Validated>,
            _: &judge_core::Context,
        ) -> Result<CallId, JudgeError> {
            Err(JudgeError::Upstream(anyhow::anyhow!("persist reached")))
        }
        async fn rate(&self, _: CallId, _: &str, _: Score, _: bool) -> Result<(), JudgeError> {
            Err(JudgeError::Upstream(anyhow::anyhow!("rate reached")))
        }
        async fn history(&self, _: &str, _: usize) -> Result<Vec<judge_core::Qa>, JudgeError> {
            Err(JudgeError::Upstream(anyhow::anyhow!("history reached")))
        }
        async fn forget_user(&self, _: &str) -> Result<u64, JudgeError> {
            Err(JudgeError::Upstream(anyhow::anyhow!("forget reached")))
        }
    }

    /// `Data::answer` reaches the store only through this, so a private
    /// question has no store to read history from or persist to.
    #[test]
    fn a_private_question_is_handed_no_store() {
        assert!(Audience::Private.record(&Untouchable).is_none());
        assert!(Audience::Channel.record(&Untouchable).is_some());
    }

    #[test]
    fn only_an_explicit_true_makes_an_answer_private() {
        assert_eq!(Audience::from_option(None), Audience::Channel);
        assert_eq!(Audience::from_option(Some(false)), Audience::Channel);
        assert_eq!(Audience::from_option(Some(true)), Audience::Private);
    }

    #[test]
    fn config_requires_a_token_and_applies_defaults() {
        let err = Config::from_vars(vars(&[])).err().map(|e| e.to_string());
        assert!(
            err.as_ref().is_some_and(|m| m.contains("DISCORD_TOKEN")),
            "{err:?}"
        );
        let err = Config::from_vars(vars(&[("DISCORD_TOKEN", "   ")]))
            .err()
            .map(|e| e.to_string());
        assert!(
            err.is_some_and(|m| m.contains("DISCORD_TOKEN")),
            "blank counts as unset"
        );

        let cfg = Config::from_vars(vars(&[("DISCORD_TOKEN", "s3cret-bot-token")])).ok();
        let cfg = cfg.as_ref();
        assert_eq!(cfg.map(|c| c.token.expose()), Some("s3cret-bot-token"));
        assert!(
            cfg.is_some_and(|c| !format!("{c:?}").contains("s3cret")),
            "the token must not reach a Debug rendering of the config"
        );
        assert_eq!(cfg.and_then(|c| c.guild_id), None);
        assert_eq!(
            cfg.map(|c| c.judge_role.as_str()),
            Some(Config::DEFAULT_JUDGE_ROLE)
        );
        assert_eq!(
            cfg.map(|c| c.max_concurrent),
            Some(Config::DEFAULT_CONCURRENCY)
        );
        assert_eq!(cfg.map(|c| c.pending_ttl), Some(pending::DEFAULT_TTL));
    }

    #[test]
    fn config_parses_guild_role_and_concurrency() {
        let cfg = Config::from_vars(vars(&[
            ("DISCORD_TOKEN", "tok"),
            ("GUILD_ID", " 123456789 "),
            ("JUDGE_ROLE", "L2 Judge"),
            ("JUDGE_CONCURRENCY", "4"),
        ]))
        .ok();
        let cfg = cfg.as_ref();
        assert_eq!(
            cfg.and_then(|c| c.guild_id).map(GuildId::get),
            Some(123_456_789)
        );
        assert_eq!(cfg.map(|c| c.judge_role.as_str()), Some("L2 Judge"));
        assert_eq!(cfg.map(|c| c.max_concurrent), Some(4));
        for bad in [
            ("GUILD_ID", "0"),
            ("GUILD_ID", "abc"),
            ("JUDGE_CONCURRENCY", "0"),
            ("JUDGE_CONCURRENCY", "two"),
        ] {
            let r = Config::from_vars(vars(&[("DISCORD_TOKEN", "tok"), bad]));
            assert!(
                r.as_ref().is_err_and(|e| e.to_string().contains(bad.0)),
                "{bad:?}: {r:?}"
            );
        }
    }

    #[test]
    fn rows_carry_typed_ids_and_at_most_five_picks() {
        let call = CallId::new(uuid::Uuid::from_u128(5));
        let CreateActionRow::Buttons(buttons) = rating_row(call) else {
            unreachable!("rating_row builds a button row");
        };
        assert_eq!(buttons.len(), 3);
        let token = PendingId::random();
        let names: Vec<String> = (0..7).map(|i| format!("Card {i}")).collect();
        let CreateActionRow::Buttons(buttons) = pick_row(token, &names) else {
            unreachable!("pick_row builds a button row");
        };
        assert_eq!(buttons.len(), render::MAX_CHOICES);
        // Serialised shape: custom ids round-trip through the parser.
        let json = serde_json::to_value(&buttons).unwrap_or_default();
        let ids: Vec<ButtonAction> = json
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b.get("custom_id").and_then(|s| s.as_str()))
            .filter_map(|s| ButtonAction::parse(s).ok())
            .collect();
        assert_eq!(ids.len(), render::MAX_CHOICES);
        assert!(
            matches!(ids.first(), Some(ButtonAction::PickCard { token: t, choice: 0 }) if *t == token)
        );
        assert!(matches!(
            ids.last(),
            Some(ButtonAction::PickCard { choice: 4, .. })
        ));
    }

    #[test]
    fn outcome_labels_every_variant() {
        assert_eq!(outcome(&Err(JudgeError::LlmRefused)), "refused");
        assert_eq!(
            outcome(&Err(JudgeError::Upstream(anyhow::anyhow!("x")))),
            "upstream"
        );
    }
}
