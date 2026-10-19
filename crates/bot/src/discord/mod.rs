//! The Discord adapter (build-order step 6): the `/judge` slash command,
//! rating buttons and the "did you mean…?" flow, on serenity 0.12.
//!
//! Everything that does not need the gateway lives in a pure submodule:
//! [`command`] (the slash commands: registration and parsing), [`render`]
//! (verdict / error → text), [`mana`] (card symbols → the bot's custom
//! emoji), [`ids`] (typed `custom_id`s), [`pick`] (how old a card pick's
//! prompt is) and [`question`] (span replacement). [`reply`] answers a
//! command over the REST API. [`symbols`] lists the
//! card-symbol emoji and keeps the table current while the bot runs.
//! [`gateway`] keeps the gateway connection to one process of all those
//! running `--discord` on a database. This file is the glue: serenity types
//! in, those modules out.
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
//! * `AmbiguousCards` → up to five "did you mean…?" buttons for the first
//!   ambiguous span, on a message that restates the question verbatim
//!   ([`render::PickPrompt`]). Nothing is held in memory: a click is answered
//!   from that message and the button's `custom_id` ([`ids::Pick`]: asker,
//!   audience, the span's byte range, the card), so any process holding the
//!   gateway answers it, after any restart. A pick (by the asker only, within
//!   [`Config::pick_ttl`], once: `pick_claims`) rewrites that span as
//!   `[[Full Name]]` and re-runs `judge()`, editing the same message; another
//!   ambiguous span simply starts the loop again. A question too long for the
//!   message gets the choices listed and no buttons.
//! * `/judge … private:True` → the same, shown to the asker alone
//!   ([`Audience::Private`]): no thread history read, and the call stored
//!   private (with rating buttons), so it never reaches the channel's
//!   follow-ups or the prior-call examples.
//! * Each user gets [`Config::user_limit`] questions per window, charged once
//!   ([`cooldown`]); a pick re-runs a question already counted and is free.
//! * `/card` and `/rule` read the database and call no model.

pub mod command;
pub mod cooldown;
pub mod endpoint;
pub mod gateway;
pub mod ids;
pub mod mana;
pub mod pick;
pub mod question;
pub mod render;
pub mod reply;
pub mod symbols;

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context as _;
use judge_core::{
    Ambiguous, Asker, CallId, CallStore, Card, Deps, DiscordOperator, JudgeError, Origin, Question,
    Rated, Resolution, RuleId, Score, SourceOffer, Validated, Verdict, Visibility,
};
use judge_llm::ApiKey;
use nonempty::NonEmpty;
use serenity::all::{
    ButtonStyle, CommandInteraction, ComponentInteraction, CreateActionRow, CreateAllowedMentions,
    CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse,
    GatewayIntents, GuildId, Http, Interaction, Member, MessageFlags, Ready, User, UserId,
};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::{
    db::{PgLibrary, PickKey, claim_pick, release_pick},
    maintenance::Availability,
    run,
    serving::Serving,
    shutdown::{Drained, Shutdown},
};
pub use command::QUESTION_MAX_CHARS;
use command::{Command, Unparsed};
use cooldown::{Cooldowns, UserLimit};
use ids::{ButtonAction, ByteSpan, Pick, Retry};
use mana::{SharedSymbols, SymbolTable};
use pick::Digest;
use render::{PickPrompt, RetryPrompt};
use reply::{Ack, Message, Reply};

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
    /// How long a "did you mean…?" stays answerable, from when its prompt
    /// was shown ([`pick`]).
    pub pick_ttl: Duration,
    /// `/judge` questions one user may ask per window (`JUDGE_USER_LIMIT` per
    /// `JUDGE_USER_WINDOW_SECS`); `None` is no per-user limit.
    pub user_limit: Option<UserLimit>,
    /// How Discord delivers interactions (`DISCORD_INTERACTIONS`).
    pub interactions: Delivery,
}

/// How Discord delivers this application's interactions: one way or the
/// other, set per application in the Developer Portal (D30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Over the gateway connection one process holds ([`gateway`]): the
    /// application has no Interactions Endpoint URL.
    Gateway,
    /// Posted to the application's Interactions Endpoint URL, which reaches
    /// [`endpoint::PATH`] on this process's HTTP listener. No gateway
    /// connection is opened.
    Http,
}

impl Delivery {
    /// The variable that chooses it.
    pub const ENV: &'static str = "DISCORD_INTERACTIONS";

    fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        match raw {
            None | Some("gateway") => Ok(Self::Gateway),
            Some("http") => Ok(Self::Http),
            Some(other) => anyhow::bail!("{} must be gateway or http, got {other:?}", Self::ENV),
        }
    }
}

/// Who sees a `/judge` reply.
///
/// Discord fixes this when the command is acknowledged, so it is chosen up
/// front and carried through a "did you mean…?" pick in the button's
/// `custom_id` ([`ids::Pick`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Audience {
    /// The channel: thread history in, the call stored public.
    Channel,
    /// The asker alone. The question stands by itself: it reads no thread
    /// history, and its call is stored private, so it cannot become a prior
    /// call or surface in anyone's follow-up ([`Visibility::Private`]).
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

    /// How the call is stored.
    const fn visibility(self) -> Visibility {
        match self {
            Self::Channel => Visibility::Public,
            Self::Private => Visibility::Private,
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
    /// `JUDGE_USER_LIMIT` or `JUDGE_USER_WINDOW_SECS`, or a
    /// `DISCORD_INTERACTIONS` other than `gateway` (the default) or `http`.
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
        let interactions = Delivery::parse(var(Delivery::ENV).as_deref())?;
        Ok(Self {
            token,
            guild_id,
            judge_role,
            max_concurrent,
            history_len: Self::DEFAULT_HISTORY,
            pick_ttl: pick::DEFAULT_TTL,
            user_limit,
            interactions,
        })
    }
}

/// Shared state behind every command and button.
pub struct Data {
    deps: Deps,
    store: Arc<dyn CallStore>,
    /// How long a "did you mean…?" stays answerable.
    pick_ttl: Duration,
    permits: Semaphore,
    judge_role: String,
    history_len: usize,
    /// Card-symbol emoji, listed after `Ready` and kept current by
    /// [`symbols::watch`]; empty until its first listing (and for as long as
    /// this application has none uploaded).
    symbols: Arc<SharedSymbols>,
    /// What `/help` and `/license` say about where this instance's source is.
    offer: SourceOffer,
    /// Whom `/help` and `/license` tell users to contact.
    operator: DiscordOperator,
    /// Per-user `/judge` windows; `None` when the operator turned the limit off.
    cooldowns: Option<Cooldowns>,
    /// Rulings for `/card`. The resolver and retriever in `deps` serve the
    /// rest of the lookups.
    library: PgLibrary,
    /// Whether a connection of this role has registered the slash commands,
    /// so a reconnect after a lost lease does not register them again.
    registered: AtomicBool,
    /// Open, or closed while a major upgrade migrates: then every command but
    /// `/help` and `/license` gets the maintenance reply.
    availability: Availability,
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Data")
            .field("pick_ttl", &self.pick_ttl)
            .field("permits", &self.permits.available_permits())
            .field("judge_role", &self.judge_role)
            .field("history_len", &self.history_len)
            .finish_non_exhaustive()
    }
}

impl Data {
    /// Wire the shared state from the process's composition: its pipeline,
    /// store, source offer and availability.
    #[must_use]
    pub fn new(serving: &Serving, cfg: &Config, operator: DiscordOperator) -> Self {
        let deps = serving.deps();
        let store = serving.store();
        let offer = serving.judge().source_offer().clone();
        // `/card` reads rulings by card id only, so its library needs no vectors.
        let library = PgLibrary::new(serving.pool().clone());
        let availability = serving.availability().clone();
        Self {
            deps,
            store,
            pick_ttl: cfg.pick_ttl,
            permits: Semaphore::new(cfg.max_concurrent),
            judge_role: cfg.judge_role.clone(),
            history_len: cfg.history_len,
            symbols: Arc::new(SharedSymbols::default()),
            offer,
            operator,
            cooldowns: cfg.user_limit.map(Cooldowns::new),
            library,
            registered: AtomicBool::new(false),
            availability,
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
    async fn answer(&self, q: &Question, user: &User, audience: Audience) -> Outgoing {
        let asker = user.id;
        let origin = Origin::Discord {
            asker: Asker::new(asker.into(), &user.name),
            visibility: audience.visibility(),
        };
        // `None` for a private question: a channel is shared, so it reads none.
        let history = match origin.history_scope() {
            None => vec![],
            Some(scope) => match self
                .store
                .history(&q.thread_id, self.history_len, scope)
                .await
            {
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
        // An interaction runs on a task of its own, so the caller's permit
        // (held until this returns) already outlives the run's result.
        let ran = run::run(&self.deps, &self.store, q, &origin, history, ()).await;
        tracing::info!(
            user = %asker,
            thread = %q.thread_id,
            elapsed_ms = ran.elapsed.as_millis(),
            usd = format_args!("{:.4}", ran.cost.usd()),
            worst_case = ran.cost.is_estimate(),
            llm_calls = ran.model_calls,
            outcome = %ran.outcome,
            private = audience.is_private(),
            "/judge"
        );
        let run::Ran {
            result,
            context,
            call,
            ..
        } = ran;
        match result {
            Ok(v) => {
                // No stored call, no rating buttons.
                Outgoing::answer(
                    &v,
                    context.as_ref(),
                    call,
                    asker,
                    &q.text,
                    &self.symbols.get(),
                )
            }
            Err(JudgeError::AmbiguousCards(spans)) => {
                Outgoing::pick(asker, audience, &q.text, &spans).unwrap_or_else(|why| {
                    tracing::info!(
                        chars = q.text.chars().count(),
                        bytes = q.text.len(),
                        why = why.as_str(),
                        "no pick buttons for this question; listing the choices without them"
                    );
                    Outgoing {
                        content: render::with_header(
                            asker.get(),
                            &q.text,
                            &render::did_you_mean(&spans),
                            &self.symbols.get(),
                        ),
                        embed: None,
                        components: vec![],
                    }
                })
            }
            Err(e) => {
                // Same classification as the HTTP interface, from the same
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
                        &self.symbols.get(),
                    ),
                    embed: None,
                    components: vec![],
                }
            }
        }
    }

    async fn on_component(
        &self,
        http: &Http,
        c: &ComponentInteraction,
        ack: &Ack,
    ) -> anyhow::Result<()> {
        match ButtonAction::parse(&c.data.custom_id) {
            Ok(ButtonAction::Retry(retry)) => self.on_retry(http, c, ack, retry).await,
            Ok(ButtonAction::Rate { .. } | ButtonAction::Pick(_))
                if !self.availability.is_open() =>
            {
                ephemeral(http, c, ack, render::MAINTENANCE).await
            }
            Ok(ButtonAction::Rate { call, score }) => self.on_rate(http, c, ack, call, score).await,
            Ok(ButtonAction::Pick(pick)) => self.on_pick(http, c, ack, pick).await,
            Ok(ButtonAction::LegacyPick) => ephemeral(http, c, ack, render::EXPIRED).await,
            Err(e) => {
                tracing::warn!(custom_id = %c.data.custom_id, error = %e, "unparseable button");
                ephemeral(http, c, ack, render::UNKNOWN_BUTTON).await
            }
        }
    }

    async fn on_rate(
        &self,
        http: &Http,
        c: &ComponentInteraction,
        ack: &Ack,
        call: CallId,
        score: Score,
    ) -> anyhow::Result<()> {
        // Acknowledge before the role lookup (HTTP) and the upsert (DB): both
        // can outlast Discord's 3 s deadline under rate limiting or load.
        let defer = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        respond(http, c, ack, defer)
            .await
            .context("acknowledge rating")?;
        let is_judge = match (c.guild_id, c.member.as_ref()) {
            (Some(guild), Some(member)) => has_role(http, guild, member, &self.judge_role).await,
            _ => false,
        };
        let user_id = c.user.id.to_string();
        let text = match self.store.rate(call, &user_id, score, is_judge).await {
            Ok(Rated::CallGone) => {
                tracing::info!(%call, "rating for a call no longer stored");
                render::CALL_GONE.to_owned()
            }
            Ok(Rated::Recorded) => {
                tracing::info!(%call, user = %user_id, score = score as u8, is_judge, "rated");
                render::rated(score, is_judge, &self.offer, &self.operator)
            }
            Err(e) => {
                tracing::error!(%call, error = format_args!("{e:#}"), "rate failed");
                render::RATE_FAILED.to_owned()
            }
        };
        ephemeral_followup(http, c, &text).await
    }

    /// A "did you mean…?" click, answered from the button and the message it
    /// is on. Every check that changes nothing comes first, then the judge
    /// slot, then the claim, so a "busy" (or a failed check) leaves the
    /// buttons usable. A claim whose acknowledgement fails is given back, for
    /// the same reason.
    async fn on_pick(
        &self,
        http: &Http,
        c: &ComponentInteraction,
        ack: &Ack,
        pick: Pick,
    ) -> anyhow::Result<()> {
        if c.user.id.get() != pick.asker.get() {
            return ephemeral(http, c, ack, render::NOT_YOURS).await;
        }
        let message = &c.message;
        let shown = pick::shown_ms(
            message.id.get(),
            message.edited_timestamp.map(|t| t.timestamp_millis()),
        );
        if pick::expired(shown, pick::now_ms(), self.pick_ttl) {
            return ephemeral(http, c, ack, render::EXPIRED).await;
        }
        // The message is the bot's own, so its content arrives without the
        // message-content intent. It must be the very prompt the button was
        // made for (not a later prompt on the same message, nor content the
        // button was never on), still read as one, and asked by the same user.
        // Anything else, content that arrived empty included, is treated as
        // expired.
        let digest = Digest::of(&message.content);
        let prompt = (digest == pick.digest)
            .then(|| PickPrompt::parse(&message.content))
            .flatten()
            .filter(|p| p.asker() == pick.asker.get());
        let Some(prompt) = prompt else {
            tracing::warn!(
                message = %message.id,
                content_chars = message.content.chars().count(),
                digest_matches = digest == pick.digest,
                "a pick's message does not hold the prompt it was made for; answering as expired"
            );
            return ephemeral(http, c, ack, render::EXPIRED).await;
        };
        // The answer goes where the message is: a private button on a channel
        // message, or the reverse, is refused.
        let ephemeral_flag = message.flags.map(|f| f.contains(MessageFlags::EPHEMERAL));
        let Some(audience) = pick::audience(pick.audience, ephemeral_flag) else {
            tracing::warn!(
                button = ?pick.audience,
                ephemeral = ?ephemeral_flag,
                "a pick's audience disagrees with its message; answering as expired"
            );
            return ephemeral(http, c, ack, render::EXPIRED).await;
        };
        let name = match self.library.card(pick.card).await {
            Ok(Some(card)) if prompt.lists(&card.name) => card.name,
            Ok(Some(_)) => {
                tracing::warn!(card = %pick.card, "a pick's card is not on its prompt's list");
                return ephemeral(http, c, ack, render::UNKNOWN_BUTTON).await;
            }
            Ok(None) => return ephemeral(http, c, ack, render::UNKNOWN_BUTTON).await,
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "pick: card lookup failed");
                return ephemeral(http, c, ack, render::PICK_FAILED).await;
            }
        };
        let Some(text) = question::pin_at(prompt.question(), pick.span.map(ByteSpan::range), &name)
        else {
            tracing::warn!(span = ?pick.span, "a pick's span is not in its question; answering as expired");
            return ephemeral(http, c, ack, render::EXPIRED).await;
        };
        // Take the permit before the claim, so a "busy" leaves the buttons usable.
        let Some(_permit) = self.acquire().await else {
            return ephemeral(http, c, ack, render::BUSY).await;
        };
        let key = PickKey {
            message_id: message.id.get(),
            shown_ms: shown,
            digest: digest.as_i64(),
        };
        let pool = self.library.pool();
        match claim_pick(pool, key).await {
            Ok(true) => {}
            Ok(false) => return ephemeral(http, c, ack, render::ALREADY_PICKED).await,
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "pick: claim failed");
                return ephemeral(http, c, ack, render::PICK_FAILED).await;
            }
        }
        // Replace the "did you mean…?" in place, then edit it again with the verdict.
        let working = CreateInteractionResponseMessage::new()
            .content(render::working(&name))
            .components(vec![])
            .embeds(vec![]);
        let sent = respond(
            http,
            c,
            ack,
            CreateInteractionResponse::UpdateMessage(working),
        )
        .await;
        if let Err(e) = sent {
            // The buttons are most likely still there: give the claim back so
            // the next click is not told the card was already picked.
            if let Err(released) = release_pick(pool, key).await {
                tracing::error!(
                    error = format_args!("{released:#}"),
                    "pick: could not give the claim back after a failed acknowledgement"
                );
            }
            return Err(anyhow::Error::from(e).context("acknowledge pick"));
        }
        let q = Question {
            thread_id: c.channel_id.to_string(),
            text,
        };
        let out = self.answer(&q, &c.user, audience).await;
        if let Err(e) = c.edit_response(http, out.into_edit()).await {
            // The pick is claimed and the message shows "Working on it…" with
            // no buttons: tell the asker (ephemerally) to ask again rather
            // than leave them waiting on a message that will never change.
            tracing::error!(error = %e, "edit picked reply failed; asking the user to re-ask");
            ephemeral_followup(http, c, render::EDIT_FAILED)
                .await
                .context("report failed edit after pick")?;
            return Err(anyhow::Error::from(e).context("edit picked reply"));
        }
        Ok(())
    }
}

impl Data {
    /// A command while a major upgrade migrates: `/judge` gets the question
    /// back with a Retry button (in the audience it asked for), anything
    /// else [`render::MAINTENANCE`], privately. Nothing is charged or stored.
    async fn maintenance(
        &self,
        reply: &mut Reply<'_>,
        c: &CommandInteraction,
        command: &Command,
    ) -> anyhow::Result<()> {
        let message = match command {
            Command::Judge { question, audience } => {
                match (
                    RetryPrompt::new(c.user.id.get(), question),
                    NonZeroU64::new(c.user.id.get()),
                ) {
                    (Some(prompt), Some(asker)) => {
                        let content = prompt.render();
                        let retry = Retry {
                            asker,
                            audience: *audience,
                            digest: Digest::of(&content),
                        };
                        Message::default()
                            .content(content)
                            .components(vec![retry_row(retry)])
                            .no_pings()
                            .ephemeral(audience.is_private())
                    }
                    _ => Message::default()
                        .content(render::MAINTENANCE)
                        .ephemeral(true),
                }
            }
            _ => Message::default()
                .content(render::MAINTENANCE)
                .ephemeral(true),
        };
        reply.send(message).await?;
        Ok(())
    }

    /// A Retry click: the question on the message asked again, by its asker,
    /// once the instance is open. Checked as a pick is (the message must be
    /// the very reply the button was made for, in the audience it names),
    /// then charged to the asker's window as the question it is, and claimed
    /// so a second click asks nothing.
    async fn on_retry(
        &self,
        http: &Http,
        c: &ComponentInteraction,
        ack: &Ack,
        retry: Retry,
    ) -> anyhow::Result<()> {
        if c.user.id.get() != retry.asker.get() {
            return ephemeral(http, c, ack, render::NOT_YOUR_RETRY).await;
        }
        let message = &c.message;
        let digest = Digest::of(&message.content);
        let prompt = (digest == retry.digest)
            .then(|| RetryPrompt::parse(&message.content))
            .flatten()
            .filter(|p| p.asker() == retry.asker.get());
        let Some(prompt) = prompt else {
            tracing::warn!(message = %message.id, "a retry's message does not hold the reply it was made for");
            return ephemeral(http, c, ack, render::EXPIRED).await;
        };
        let ephemeral_flag = message.flags.map(|f| f.contains(MessageFlags::EPHEMERAL));
        let Some(audience) = pick::audience(retry.audience, ephemeral_flag) else {
            return ephemeral(http, c, ack, render::EXPIRED).await;
        };
        if !self.availability.is_open() {
            return ephemeral(http, c, ack, render::STILL_UPGRADING).await;
        }
        let Some(_permit) = self.acquire().await else {
            return ephemeral(http, c, ack, render::BUSY).await;
        };
        let key = PickKey {
            message_id: message.id.get(),
            shown_ms: pick::shown_ms(
                message.id.get(),
                message.edited_timestamp.map(|t| t.timestamp_millis()),
            ),
            digest: digest.as_i64(),
        };
        let pool = self.library.pool();
        match claim_pick(pool, key).await {
            Ok(true) => {}
            Ok(false) => return ephemeral(http, c, ack, render::ALREADY_RETRIED).await,
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "retry: claim failed");
                return ephemeral(http, c, ack, render::PICK_FAILED).await;
            }
        }
        // The question was never charged: the maintenance reply cost nothing.
        if let Some(cooldowns) = &self.cooldowns
            && let Err(wait) = cooldowns.take(c.user.id.get(), Instant::now())
        {
            if let Err(released) = release_pick(pool, key).await {
                tracing::error!(
                    error = format_args!("{released:#}"),
                    "retry: could not give the claim back"
                );
            }
            let text = render::cooling_down(cooldowns.limit(), wait);
            return ephemeral(http, c, ack, &text).await;
        }
        let working = CreateInteractionResponseMessage::new()
            .content(render::RETRYING)
            .components(vec![])
            .embeds(vec![]);
        if let Err(e) = respond(
            http,
            c,
            ack,
            CreateInteractionResponse::UpdateMessage(working),
        )
        .await
        {
            if let Err(released) = release_pick(pool, key).await {
                tracing::error!(
                    error = format_args!("{released:#}"),
                    "retry: could not give the claim back"
                );
            }
            return Err(anyhow::Error::from(e).context("acknowledge retry"));
        }
        let q = Question {
            thread_id: c.channel_id.to_string(),
            text: prompt.question().to_owned(),
        };
        let out = self.answer(&q, &c.user, audience).await;
        if let Err(e) = c.edit_response(http, out.into_edit()).await {
            tracing::error!(error = %e, "edit retried reply failed; asking the user to re-ask");
            ephemeral_followup(http, c, render::EDIT_FAILED)
                .await
                .context("report failed edit after retry")?;
            return Err(anyhow::Error::from(e).context("edit retried reply"));
        }
        Ok(())
    }
}

fn retry_row(retry: Retry) -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(ButtonAction::Retry(retry).to_custom_id())
            .label(render::RETRY_LABEL)
            .style(ButtonStyle::Primary),
    ])
}

/// Why a "did you mean…?" goes out without buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoPick {
    /// The question does not fit the prompt message.
    TooLong,
    /// The span's byte offsets do not fit a pick's `custom_id`.
    SpanPastU16,
    /// No asker to give the pick to.
    NoAsker,
}

impl NoPick {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "the question does not fit the prompt message",
            Self::SpanPastU16 => "the span's byte offsets are past u16",
            Self::NoAsker => "the asker's id is zero",
        }
    }
}

/// A reply before it is shaped as a command's [`Message`] or a pick's
/// [`EditInteractionResponse`].
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

    /// The "did you mean…?" with buttons for the first ambiguous span, or
    /// why there can be none (the caller lists the choices without buttons
    /// instead).
    fn pick(
        asker: UserId,
        audience: Audience,
        question: &str,
        spans: &NonEmpty<Ambiguous>,
    ) -> Result<Self, NoPick> {
        let prompt = PickPrompt::new(asker.get(), question, spans).ok_or(NoPick::TooLong)?;
        let first = spans.first();
        // A span the question does not contain is appended on the pick, as
        // `question::pin_card` does; one past `u16` cannot occur under the
        // option's `max_length` and is offered no buttons.
        let span = match question::find_span(question, &first.query) {
            Some(r) => Some(ByteSpan::new(r).ok_or(NoPick::SpanPastU16)?),
            None => None,
        };
        let content = prompt.render();
        let base = Pick {
            // A `UserId` is never zero.
            asker: NonZeroU64::new(asker.get()).ok_or(NoPick::NoAsker)?,
            audience,
            span,
            digest: Digest::of(&content),
            card: first.candidates.first().id,
        };
        Ok(Self {
            content,
            embed: None,
            components: vec![pick_row(base, &first.candidates)],
        })
    }

    fn into_message(self) -> Message {
        let mut m = Message::default()
            .content(self.content)
            .components(self.components)
            .no_pings();
        if let Some(e) = self.embed {
            m = m.embed(e);
        }
        m
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

/// One button per candidate, the first [`render::MAX_CHOICES`]: `base` with
/// each one's card.
fn pick_row(base: Pick, candidates: &NonEmpty<Card>) -> CreateActionRow {
    let buttons = candidates
        .iter()
        .take(render::MAX_CHOICES)
        .map(|card| {
            let pick = Pick {
                card: card.id,
                ..base
            };
            CreateButton::new(ButtonAction::Pick(pick).to_custom_id())
                .label(render::button_label(&card.name))
                .style(ButtonStyle::Primary)
        })
        .collect();
    CreateActionRow::Buttons(buttons)
}

async fn ephemeral(
    http: &Http,
    c: &ComponentInteraction,
    ack: &Ack,
    text: &str,
) -> anyhow::Result<()> {
    let msg = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(true);
    respond(http, c, ack, CreateInteractionResponse::Message(msg))
        .await
        .context("respond to button")
}

/// A button's initial response, told to `ack` whether or not it was sent.
/// The only way a button handler sends one: the interactions endpoint
/// answers Discord's request when it hears of it.
async fn respond(
    http: &Http,
    c: &ComponentInteraction,
    ack: &Ack,
    response: CreateInteractionResponse,
) -> serenity::Result<()> {
    let sent = c.create_response(http, response).await;
    ack.sent();
    sent
}

/// An ephemeral follow-up after a deferred acknowledgement.
async fn ephemeral_followup(
    http: &Http,
    c: &ComponentInteraction,
    text: &str,
) -> anyhow::Result<()> {
    let msg = CreateInteractionResponseFollowup::new()
        .content(text)
        .ephemeral(true);
    c.create_followup(http, msg)
        .await
        .map(drop)
        .context("follow up on button")
}

/// Whether `member` holds a role named `role_name` in `guild`. Looked up over
/// HTTP each time (the gateway runs with no intents, so the cache is empty);
/// a lookup failure counts as "not a judge" and is logged.
async fn has_role(http: &Http, guild: GuildId, member: &Member, role_name: &str) -> bool {
    match guild.roles(http).await {
        Ok(roles) => roles
            .values()
            .any(|r| r.name == role_name && member.roles.contains(&r.id)),
        Err(e) => {
            tracing::warn!(%guild, error = %e, "could not fetch guild roles; treating rater as non-judge");
            false
        }
    }
}

impl Data {
    /// Answer an interaction, however it arrived: a slash command or a
    /// button. Other kinds (autocomplete, modals) are not registered, so
    /// none arrive. Never fails: a handler's error is logged here, and a
    /// command's is reported to the user.
    async fn dispatch(&self, http: &Http, interaction: &Interaction, ack: &Ack) {
        match interaction {
            Interaction::Command(c) => self.on_command(http, c, ack).await,
            Interaction::Component(c) => {
                if let Err(e) = self.on_component(http, c, ack).await {
                    tracing::error!(error = format_args!("{e:#}"), "button handler failed");
                }
            }
            other => {
                tracing::warn!(kind = ?other.kind(), "an interaction of a kind this bot does not register");
            }
        }
    }

    /// A slash command: parsed, run, and on an error answered with
    /// [`render::FAILED`].
    async fn on_command(&self, http: &Http, c: &CommandInteraction, ack: &Ack) {
        let command = match Command::parse(&c.data, c.guild_id.is_some()) {
            Ok(command) => command,
            Err(why) => {
                tracing::warn!(command = %c.data.name, error = %why, "a slash command this build cannot run");
                let text = match why {
                    Unparsed::NotInGuild => render::SERVER_ONLY,
                    Unparsed::Unknown(_) | Unparsed::BadOption { .. } => render::UNKNOWN_COMMAND,
                };
                if let Err(e) = Reply::new(http, c, true, ack)
                    .send(Message::text(text))
                    .await
                {
                    tracing::error!(error = %e, "could not answer an unusable command");
                }
                return;
            }
        };
        let name = command.name();
        let mut reply = Reply::new(http, c, command.ephemeral(), ack);
        if !self.availability.is_open() && !command.answers_in_maintenance() {
            if let Err(e) = self.maintenance(&mut reply, c, &command).await {
                tracing::error!(error = %e, "could not send the maintenance reply");
            }
            return;
        }
        let ran = match command {
            Command::Judge { question, audience } => {
                self.judge(&mut reply, c, question, audience).await
            }
            Command::Card { name, private } => self.card(&mut reply, &name, private).await,
            Command::Rule { id, private } => self.rule(&mut reply, &id, private).await,
            Command::Help => self.help(&mut reply).await,
            Command::License => self.license(&mut reply).await,
            Command::Forget => self.forget(&mut reply, c.user.id).await,
        };
        if let Err(error) = ran {
            tracing::error!(
                command = name,
                error = format_args!("{error:#}"),
                "command failed"
            );
            if let Err(e) = reply.send(Message::default().content(render::FAILED)).await {
                tracing::error!(error = %e, "could not report the failure to the user");
            }
        }
    }

    /// `/judge`: ask a Magic rules question; the answer cites the
    /// Comprehensive Rules.
    async fn judge(
        &self,
        reply: &mut Reply<'_>,
        c: &CommandInteraction,
        question: String,
        audience: Audience,
    ) -> anyhow::Result<()> {
        let Some(_permit) = self.acquire().await else {
            reply
                .send(Message::default().content(render::BUSY).ephemeral(true))
                .await?;
            return Ok(());
        };
        // After the slot, so a "busy" costs the asker none of their allowance.
        // Everything past here counts, answered or not: an ambiguous or unknown
        // card has already paid for an extraction call.
        if let Some(cooldowns) = &self.cooldowns
            && let Err(wait) = cooldowns.take(c.user.id.get(), Instant::now())
        {
            reply
                .send(
                    Message::default()
                        .content(render::cooling_down(cooldowns.limit(), wait))
                        .ephemeral(true),
                )
                .await?;
            return Ok(());
        }
        // Answers take 20–45 s; Discord wants an acknowledgement within 3 s. The
        // acknowledgement is also where Discord fixes who sees the reply.
        reply.defer(audience.is_private()).await?;
        let q = Question {
            thread_id: c.channel_id.to_string(),
            text: question,
        };
        let out = self.answer(&q, &c.user, audience).await;
        reply
            .send(out.into_message().ephemeral(audience.is_private()))
            .await?;
        Ok(())
    }

    /// `/card`: look up a card's Oracle text and rulings. No model is called.
    async fn card(&self, reply: &mut Reply<'_>, name: &str, private: bool) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > render::LOOKUP_INPUT_LIMIT {
            return say(reply, render::LOOKUP_BAD_NAME, true).await;
        }
        // The resolver's fuzzy step and a cold pool can outlast Discord's 3 s.
        // Discord fixes the reply's visibility here, so what follows (the
        // result, or "not found") is seen by whoever `private` says.
        reply.defer(private).await?;
        let resolution = match self.deps.resolver.resolve(name).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "/card resolve failed");
                return say(reply, render::LOOKUP_FAILED, true).await;
            }
        };
        match resolution {
            Resolution::Resolved { card, .. } => {
                let rulings = self.library.rulings(card.id).await.unwrap_or_else(|e| {
                    tracing::warn!(error = format_args!("{e:#}"), "/card rulings unavailable");
                    vec![]
                });
                let view = render::card(&card, &rulings, &self.symbols.get());
                let lookup = lookup_message(view).ephemeral(private);
                reply.send(lookup).await?;
                Ok(())
            }
            Resolution::Ambiguous { candidates, .. } => {
                let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
                say(reply, &render::card_ambiguous(name, &names), true).await
            }
            Resolution::NotFound { .. } => say(reply, &render::card_not_found(name), true).await,
        }
    }

    /// `/rule`: look up Comprehensive Rules text by number. No model is
    /// called.
    async fn rule(&self, reply: &mut Reply<'_>, id: &str, private: bool) -> anyhow::Result<()> {
        let Ok(rule) = RuleId::try_new(id.trim().trim_end_matches('.')) else {
            return say(reply, render::LOOKUP_BAD_RULE, true).await;
        };
        reply.defer(private).await?;
        let chunks = match self
            .deps
            .retriever
            .lookup_rules(std::slice::from_ref(&rule))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "/rule lookup failed");
                return say(reply, render::LOOKUP_FAILED, true).await;
            }
        };
        let Some(view) = render::rules(&rule, &chunks, &self.symbols.get()) else {
            return say(reply, &render::rule_not_found(&rule), true).await;
        };
        reply.send(lookup_message(view).ephemeral(private)).await?;
        Ok(())
    }

    /// `/help`: what the bot does, how to ask, what it stores, and where its
    /// source is.
    async fn help(&self, reply: &mut Reply<'_>) -> anyhow::Result<()> {
        // Deferred first: the data's freshness is a database read, and the reply
        // must not miss Discord's 3 s deadline waiting on it.
        reply.defer(true).await?;
        let freshness = self.library.freshness().await;
        // Two ephemeral messages: the second is a follow-up to the first.
        for part in render::help(&self.offer, &self.operator, freshness.as_ref()) {
            reply.send(Message::default().content(part)).await?;
        }
        Ok(())
    }

    /// `/license`: where this bot's source code is, at which commit, and
    /// under which licence.
    async fn license(&self, reply: &mut Reply<'_>) -> anyhow::Result<()> {
        let text = render::license(&self.offer, &self.operator);
        reply.send(Message::default().content(text)).await?;
        Ok(())
    }

    /// `/forget`: delete the caller's ratings and anonymize their failed
    /// calls.
    async fn forget(&self, reply: &mut Reply<'_>, user: UserId) -> anyhow::Result<()> {
        let user_id = user.to_string();
        // The id stays out of the log: the point of the command is to stop
        // keeping it.
        let text = match self.store.forget_user(&user_id).await {
            Ok(f) => {
                tracing::info!(
                    ratings = f.ratings,
                    calls = f.calls,
                    failures = f.failures,
                    "forgot a user"
                );
                render::forgotten(f)
            }
            Err(e) => {
                tracing::error!(error = format_args!("{e:#}"), "forget failed");
                render::FORGET_FAILED.to_owned()
            }
        };
        reply.send(Message::default().content(text)).await?;
        Ok(())
    }
}

/// A lookup's embed, pinging nobody.
fn lookup_message(view: render::Lookup) -> Message {
    let embed = CreateEmbed::new()
        .title(view.title)
        .url(view.url)
        .description(view.description);
    let embed = match view.footer {
        Some(f) => embed.footer(CreateEmbedFooter::new(f)),
        None => embed,
    };
    Message::default().embed(embed).no_pings()
}

/// A plain text reply, pinging nobody.
async fn say(reply: &mut Reply<'_>, text: &str, ephemeral: bool) -> anyhow::Result<()> {
    reply.send(Message::text(text).ephemeral(ephemeral)).await?;
    Ok(())
}

/// The `--discord` role: build the adapter's [`Data`] over the process's
/// shared composition and serve it as `cfg.interactions` says (D30).
/// `cfg` holds the token and `operator` is the contact the bot must name, so
/// a process that reaches this has passed both requirements.
///
/// - [`Delivery::Gateway`]: [`run`] while this process holds the gateway
///   lease ([`gateway::hold`]: it stands by while another process holds it).
/// - [`Delivery::Http`]: fetch the application's public key over the REST
///   API, register the commands, then open `interactions`, which the HTTP
///   adapter serves at [`endpoint::PATH`]. No gateway, no lease.
///
/// Either way it checks the application's Interactions Endpoint URL against
/// the mode, and reports a mismatch (ERROR, and the alert webhook) without
/// stopping: Discord saves a URL only once its endpoint answers, so an
/// `http` instance must run before the URL can be set.
///
/// On the process's requested shutdown it leaves the gateway (releasing the
/// lease once the connection is closed), or stops waiting for the HTTP
/// endpoint to open, and returns when every interaction in flight has been
/// answered, or the drain's limit has passed: an answer already deferred is
/// sent over the REST API, which needs no gateway.
///
/// # Errors
/// As [`run`], the gateway lease was lost, or the application's public key
/// is not one.
pub async fn serve(
    serving: &Serving,
    cfg: Config,
    operator: DiscordOperator,
    interactions: &endpoint::Interactions,
) -> anyhow::Result<()> {
    let judge = serving.judge();
    // One `Data` for the role, whichever connection serves it: the concurrency
    // slots, the per-user windows and the symbol table outlive a reconnect
    // after a lost lease. A "did you mean?" pick needs none of it: it is
    // answered from its message, by whichever process receives the click.
    let data = Arc::new(Data::new(serving, &cfg, operator));
    tracing::info!(
        guild = ?cfg.guild_id,
        judge_role = %cfg.judge_role,
        concurrency = cfg.max_concurrent,
        user_limit = ?cfg.user_limit,
        interactions = ?cfg.interactions,
        "starting Discord adapter"
    );
    let shutdown = serving.shutdown();
    let alert = judge.budget().alert.clone();
    let http = Arc::new(Http::new(cfg.token.expose()));
    match cfg.interactions {
        Delivery::Gateway => {
            // The URL check, on a task of its own: the gateway does not wait
            // for it, and it ends with the role.
            let _check = AbortTask(tokio::spawn({
                let (http, shutdown) = (Arc::clone(&http), shutdown.clone());
                async move {
                    if let Some(app) = application(&http, &shutdown).await {
                        let url = app.interactions_endpoint_url.as_deref();
                        report_delivery(Delivery::Gateway, url, alert.as_ref()).await;
                    }
                }
            }));
            gateway::hold(
                serving.pool(),
                serving.process(),
                gateway::Timing::DEFAULT,
                shutdown,
                |stop| run(cfg.clone(), Arc::clone(&data), shutdown.clone(), stop),
            )
            .await?;
            // A connection that ended by itself with no error: as before, the
            // role has stopped.
            if !shutdown.is_requested() {
                return Ok(());
            }
            // Left on the shutdown, with the gateway closed: no event is
            // dispatched after this point. An event dispatched before it holds
            // its `Work` from the first poll of the task serenity spawned for
            // it, which in practice has happened during the close and the
            // lease release's round trip; nothing can count it at the spawn
            // itself.
        }
        Delivery::Http => {
            // Nothing to drain if the shutdown came first.
            let Some(app) = application(&http, shutdown).await else {
                return Ok(());
            };
            http.set_application_id(app.id);
            let verifier = endpoint::verifier(&app.verify_key)?;
            let url = app.interactions_endpoint_url.clone();
            // Off the path to opening: an alert webhook may take its time.
            let _report = AbortTask(tokio::spawn(async move {
                report_delivery(Delivery::Http, url.as_deref(), alert.as_ref()).await;
            }));
            // These three for the life of the role: there is no connection
            // to start them again.
            let _register = AbortTask(tokio::spawn({
                let (http, data) = (Arc::clone(&http), Arc::clone(&data));
                async move { register_until_done(&http, cfg.guild_id, &data).await }
            }));
            let _watcher = AbortTask(tokio::spawn(symbols::watch(
                Arc::clone(&http),
                data.library.pool().clone(),
                Arc::clone(&data.symbols),
            )));
            interactions.open(
                verifier,
                Arc::clone(&data),
                Arc::clone(&http),
                shutdown.clone(),
                judge.budget().alert.clone(),
            );
            tracing::info!(
                path = endpoint::PATH,
                "receiving Discord interactions over HTTP; no gateway connection"
            );
            shutdown.requested().await;
            // The endpoint stays open while the listener drains: a request
            // that still arrives is Discord's only attempt, and is answered.
            // Its handler's work is counted only once the request is read,
            // so the drain starts when the listener has answered them all.
            tokio::select! {
                () = interactions.closed() => {}
                () = shutdown.limit_passed() => {}
            }
        }
    }
    match shutdown.drained().await {
        Drained::Finished => tracing::info!("Discord: every answer in flight was sent"),
        Drained::Abandoned(n) => tracing::warn!(
            in_flight = n,
            limit_secs = shutdown.limit().as_secs(),
            "Discord: interaction handlers still running at the drain's limit; they end with the process"
        ),
    }
    Ok(())
}

/// How long one request for the application's details may take.
const APPLICATION_TIMEOUT: Duration = Duration::from_secs(10);
/// The first pause after a failed request for them, doubling to
/// [`APPLICATION_RETRY_MAX`].
const APPLICATION_RETRY: Duration = Duration::from_secs(5);
/// The longest pause between requests for them.
const APPLICATION_RETRY_MAX: Duration = Duration::from_mins(1);

/// The application's details from the REST API (`GET /applications/@me`),
/// asked again with a growing pause until they come, or `None` once the
/// process's shutdown is requested.
async fn application(
    http: &Http,
    shutdown: &Shutdown,
) -> Option<serenity::all::CurrentApplicationInfo> {
    let mut pause = APPLICATION_RETRY;
    loop {
        let asked = tokio::time::timeout(APPLICATION_TIMEOUT, http.get_current_application_info());
        let failed = tokio::select! {
            biased;
            () = shutdown.requested() => return None,
            got = asked => match got {
                Ok(Ok(app)) => return Some(app),
                Ok(Err(e)) => e.to_string(),
                Err(_) => format!("no answer within {}s", APPLICATION_TIMEOUT.as_secs()),
            },
        };
        tracing::warn!(
            error = %failed,
            retry_secs = pause.as_secs(),
            "could not read the Discord application's details; asking again"
        );
        tokio::select! {
            biased;
            () = shutdown.requested() => return None,
            () = tokio::time::sleep(pause) => {}
        }
        pause = (pause * 2).min(APPLICATION_RETRY_MAX);
    }
}

/// Log (and post to the alert webhook) what [`endpoint::mismatch`] finds.
async fn report_delivery(
    mode: Delivery,
    url: Option<&str>,
    alert: Option<&crate::alert::AlertWebhook>,
) {
    let Some(problem) = endpoint::mismatch(mode, url) else {
        tracing::info!(
            ?mode,
            url,
            "Discord's delivery of interactions matches DISCORD_INTERACTIONS"
        );
        return;
    };
    tracing::error!("{problem}");
    if let Some(hook) = alert {
        crate::alert::post(
            &crate::alert::client(),
            hook,
            "Discord delivery alert",
            &problem,
        )
        .await;
    }
}

/// Aborts its task when dropped.
struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One gateway connection's event handler: interactions go to
/// [`Data::dispatch`], each counted as in-flight work for the process's
/// [`Shutdown`] (serenity dispatches every event on a task of its own, and
/// this holds the [`Work`](crate::shutdown::Work) for as long as that task
/// runs the handler, a `/judge` answer included).
struct Handler {
    data: Arc<Data>,
    shutdown: Shutdown,
    guild: Option<GuildId>,
    /// Where the first `Ready` hands the symbol watcher's abort handle. Taken
    /// by that `Ready`, so a later one (a new session on the same
    /// connection) starts no second watcher.
    watcher: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<tokio::task::AbortHandle>>>,
}

#[serenity::all::async_trait]
impl serenity::all::EventHandler for Handler {
    async fn ready(&self, ctx: serenity::all::Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, guilds = ready.guilds.len(), "connected to Discord");
        let Some(watcher_tx) = self.watcher.lock().ok().and_then(|mut w| w.take()) else {
            return;
        };
        // Nothing in here fails or waits long: registration is bounded and
        // only logged on failure, and the watcher is a task of its own.
        register(&ctx.http, self.guild, &self.data).await;
        // The application id arrives with `Ready`, so `ctx.http` can list the
        // emoji now. The watcher does it as a task of its own, so a slow
        // database or Discord never delays the bot's readiness: symbols
        // render as text until its first listing lands.
        let watcher = tokio::spawn(symbols::watch(
            Arc::clone(&ctx.http),
            self.data.library.pool().clone(),
            Arc::clone(&self.data.symbols),
        ));
        if watcher_tx.send(watcher.abort_handle()).is_err() {
            // The connection is already over.
            watcher.abort();
        }
    }

    async fn interaction_create(&self, ctx: serenity::all::Context, interaction: Interaction) {
        let _work = self.shutdown.work();
        dispatch_guarded(
            Arc::clone(&self.data),
            Arc::clone(&ctx.http),
            interaction,
            Ack::none(),
        )
        .await;
    }
}

/// [`Data::dispatch`] on a task of its own, so a panic in a handler is
/// caught: it is logged, and a command is told [`render::FAILED`], as it is
/// after an error.
async fn dispatch_guarded(data: Arc<Data>, http: Arc<Http>, interaction: Interaction, ack: Ack) {
    let command = match &interaction {
        Interaction::Command(c) => Some(c.clone()),
        _ => None,
    };
    let (h, a) = (Arc::clone(&http), ack.clone());
    let ran = tokio::spawn(async move { data.dispatch(&h, &interaction, &a).await }).await;
    let Err(e) = ran else { return };
    if !e.is_panic() {
        return;
    }
    tracing::error!(error = %e, "an interaction handler panicked");
    let Some(c) = command else { return };
    let ephemeral = Command::parse(&c.data, c.guild_id.is_some()).map_or(true, |c| c.ephemeral());
    let message = || Message::default().content(render::FAILED);
    // The initial response if the handler sent none, else a follow-up.
    if Reply::new(&http, &c, ephemeral, &ack)
        .send(message())
        .await
        .is_err()
    {
        let mut after = Reply::new(&http, &c, ephemeral, &ack);
        after.responded();
        if let Err(e) = after.send(message()).await {
            tracing::error!(error = %e, "could not report the panic to the user");
        }
    }
}

/// Connect to the gateway and serve until the connection ends, or until
/// `stop` resolves, which closes it. Called once per connection: the lease
/// hand-off ([`gateway::hold`]) calls it again after a lost lease.
///
/// On a stop it returns only once serenity says every shard is shut down.
/// The wait is not bounded here: [`gateway::hold`] gives it
/// [`gateway::Timing::shutdown_limit`], then ends the role, and the process
/// exits, which closes any shard still open.
///
/// Every interaction it dispatches is counted as in-flight work for
/// `shutdown` ([`Handler`]).
///
/// # Errors
/// Building the client (bad token) or a fatal gateway error.
pub async fn run(
    cfg: Config,
    data: Arc<Data>,
    shutdown: Shutdown,
    mut stop: gateway::Stop,
) -> anyhow::Result<()> {
    // The symbol watcher this connection starts, aborted when the connection
    // ends (however `run` returns, or is dropped), so reconnects never stack
    // watchers.
    let (watcher_tx, watcher_rx) = tokio::sync::oneshot::channel();
    let _watcher = AbortOnDrop(watcher_rx);
    let handler = Handler {
        data,
        shutdown,
        guild: cfg.guild_id,
        watcher: std::sync::Mutex::new(Some(watcher_tx)),
    };
    // Slash commands and button presses arrive without any gateway intent.
    let building = serenity::all::ClientBuilder::new(cfg.token.expose(), GatewayIntents::empty())
        .event_handler(handler);
    // `biased`: a stop that came first wins, so nothing starts after it.
    let mut client = tokio::select! {
        biased;
        () = stop.requested() => return Ok(()),
        built = building => built.context("build Discord client")?,
    };
    let shards = Arc::clone(&client.shard_manager);
    // One case the loop below cannot see: when a shard's shutdown reports a
    // stray id, serenity queues a restart, and `start` can return before the
    // queued start is processed. That costs at worst one IDENTIFY that
    // dispatches nothing: the stray runner reads the queued shutdown before
    // it dispatches an event. After a lost lease it falls inside the slack
    // between the shutdown limit and the grace; after a clean release at
    // shutdown the next holder may already be connected, and still nothing
    // is answered twice.
    let mut connected = std::pin::pin!(client.start());
    tokio::select! {
        biased;
        () = stop.requested() => {}
        ended = &mut connected => return ended.context("Discord gateway"),
    }
    tracing::info!("closing the Discord gateway connection");
    // `shutdown_all` closes the shards registered so far and then makes
    // `start` return. It returns at once, doing nothing, while none is
    // registered (the shard is still connecting), so it is asked again until
    // `start` returns: a shard that registers later is closed on the next
    // round.
    loop {
        shards.shutdown_all().await;
        tokio::select! {
            ended = &mut connected => return ended.context("Discord gateway"),
            () = tokio::time::sleep(SHUTDOWN_RETRY) => {}
        }
    }
}

/// How long registering the slash commands may take on a connection before
/// it goes on without them (the ones registered before stay in force).
const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// The first pause after a failed registration in `http` mode, doubling to
/// [`REGISTER_RETRY_MAX`].
const REGISTER_RETRY: Duration = Duration::from_secs(30);
/// The longest pause between registrations in `http` mode.
const REGISTER_RETRY_MAX: Duration = Duration::from_mins(10);

/// [`register`] until it succeeds, for `http` mode, where no next
/// connection would try again.
async fn register_until_done(http: &Http, guild: Option<GuildId>, data: &Data) {
    let mut pause = REGISTER_RETRY;
    while !register(http, guild, data).await {
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(REGISTER_RETRY_MAX);
    }
}

/// Register the slash commands ([`command::definitions`]), once per role: in
/// `guild`, or globally; `true` once registered. Never fails: Discord keeps
/// the commands registered earlier, so a failure (or no answer in
/// [`REGISTER_TIMEOUT`]) is a warning, and it is tried again (on the next
/// gateway connection, or after a pause over HTTP).
async fn register(http: &Http, guild: Option<GuildId>, data: &Data) -> bool {
    if data.registered.load(Ordering::Acquire) {
        tracing::debug!("slash commands already registered by this process");
        return true;
    }
    let commands = command::definitions();
    let done = match guild {
        Some(g) => tokio::time::timeout(REGISTER_TIMEOUT, g.set_commands(http, commands))
            .await
            .map(|r| r.map(drop)),
        None => tokio::time::timeout(
            REGISTER_TIMEOUT,
            serenity::all::Command::set_global_commands(http, commands),
        )
        .await
        .map(|r| r.map(drop)),
    };
    match (done, guild) {
        (Ok(Ok(())), Some(g)) => {
            data.registered.store(true, Ordering::Release);
            tracing::info!(guild = %g, "registered /judge, /card, /rule, /help, /license and /forget in one guild");
        }
        (Ok(Ok(())), None) => {
            data.registered.store(true, Ordering::Release);
            tracing::info!(
                "registered /judge, /card, /rule, /help, /license and /forget globally (propagation can take up to an hour)"
            );
        }
        (Ok(Err(e)), _) => tracing::warn!(
            error = %e,
            "registering the slash commands failed; those registered before stay, and it is tried again"
        ),
        (Err(_), _) => tracing::warn!(
            secs = REGISTER_TIMEOUT.as_secs(),
            "registering the slash commands got no answer in time; those registered before stay, and it is tried again"
        ),
    }
    data.registered.load(Ordering::Acquire)
}

/// How often [`run`] asks serenity again to shut its shards down while a
/// shard is still connecting.
const SHUTDOWN_RETRY: Duration = Duration::from_millis(50);

/// Aborts the task whose handle arrives on the channel, when dropped.
struct AbortOnDrop(tokio::sync::oneshot::Receiver<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        // Closed first, so a handle sent from now on fails to send and its
        // sender aborts the task itself.
        self.0.close();
        if let Ok(handle) = self.0.try_recv() {
            handle.abort();
        }
    }
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

    /// A private `/judge` is stored private and reads no channel history; a
    /// channel one reads only the channel's public calls.
    #[test]
    fn a_private_question_is_stored_private_and_reads_no_history() {
        let origin = |audience: Audience| Origin::Discord {
            asker: Asker::new(NonZeroU64::MIN, "urza"),
            visibility: audience.visibility(),
        };
        assert!(origin(Audience::Private).is_private());
        assert_eq!(origin(Audience::Private).history_scope(), None);
        assert!(!origin(Audience::Channel).is_private());
        assert_eq!(
            origin(Audience::Channel).history_scope(),
            Some(judge_core::HistoryScope::Public)
        );
    }

    /// While a major upgrade migrates only the commands that need no table
    /// answer, and `/judge`'s reply carries a Retry button for its asker.
    #[test]
    fn maintenance_answers_only_help_and_license_and_offers_a_retry() {
        let judge = Command::Judge {
            question: "q".into(),
            audience: Audience::Private,
        };
        assert!(!judge.answers_in_maintenance());
        assert!(Command::Help.answers_in_maintenance());
        assert!(Command::License.answers_in_maintenance());
        assert!(!Command::Forget.answers_in_maintenance());
        let retry = Retry {
            asker: NonZeroU64::MIN,
            audience: Audience::Private,
            digest: Digest::of("x"),
        };
        assert_eq!(
            custom_ids(retry_row(retry)),
            vec![ButtonAction::Retry(retry)]
        );
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
        assert_eq!(cfg.map(|c| c.pick_ttl), Some(pick::DEFAULT_TTL));
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

    fn card(n: u128, name: &str) -> Card {
        Card {
            id: judge_core::CardId::new(uuid::Uuid::from_u128(n)),
            name: name.into(),
            layout: judge_core::Layout::Normal,
            faces: NonEmpty::new(judge_core::Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: String::new(),
            }),
        }
    }

    fn custom_ids(row: CreateActionRow) -> Vec<ButtonAction> {
        let CreateActionRow::Buttons(buttons) = row else {
            unreachable!("the rows here are button rows");
        };
        serde_json::to_value(&buttons)
            .unwrap_or_default()
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b.get("custom_id").and_then(|s| s.as_str()))
            .filter_map(|s| ButtonAction::parse(s).ok())
            .collect()
    }

    #[test]
    fn rows_carry_typed_ids_and_at_most_five_picks() {
        let call = CallId::new(uuid::Uuid::from_u128(5));
        assert_eq!(custom_ids(rating_row(call)).len(), 3);
        let seven = (1..=7).map(|i| card(i, &format!("Card {i}")));
        let candidates = NonEmpty::collect(seven).unwrap_or_else(|| NonEmpty::new(card(1, "x")));
        let base = Pick {
            asker: NonZeroU64::MIN,
            audience: Audience::Private,
            span: ByteSpan::new(3..7),
            digest: Digest::of("prompt"),
            card: candidates.first().id,
        };
        let ids = custom_ids(pick_row(base, &candidates));
        assert_eq!(ids.len(), render::MAX_CHOICES);
        let picked: Vec<u128> = ids
            .iter()
            .filter_map(|a| match a {
                ButtonAction::Pick(p)
                    if Pick {
                        card: base.card,
                        ..*p
                    } == base =>
                {
                    Some(p.card.into_inner().as_u128())
                }
                _ => None,
            })
            .collect();
        assert_eq!(picked, [1, 2, 3, 4, 5]);
    }

    fn spans(query: &str) -> NonEmpty<Ambiguous> {
        NonEmpty::new(Ambiguous {
            query: query.into(),
            candidates: NonEmpty::from((card(1, "Dark Confidant"), vec![card(2, "Bob")])),
        })
    }

    #[test]
    fn a_pick_carries_the_span_the_click_pins() {
        let asker = UserId::new(42);
        let question = "Does BOB die to bob?";
        let out = Outgoing::pick(asker, Audience::Channel, question, &spans("bob"));
        let Ok(out) = out else {
            unreachable!("a short question fits");
        };
        let ids = custom_ids(
            out.components
                .into_iter()
                .next()
                .unwrap_or_else(|| CreateActionRow::Buttons(vec![])),
        );
        let Some(ButtonAction::Pick(p)) = ids.first().copied() else {
            unreachable!("the first button is a pick");
        };
        assert_eq!((p.asker.get(), p.audience), (42, Audience::Channel));
        // Every button names the prompt it is on.
        assert_eq!(p.digest, Digest::of(&out.content));
        let prompt = PickPrompt::parse(&out.content);
        assert!(
            prompt
                .as_ref()
                .is_some_and(|q| q.lists("Dark Confidant") && q.lists("Bob"))
        );
        assert!(prompt.as_ref().is_some_and(|q| !q.lists("Bob's Burgers")));
        assert_eq!(prompt.as_ref().map(PickPrompt::question), Some(question));
        // The click, from the message and the button alone, pins as
        // `pin_card` would have on the question it was asked with.
        let pinned = prompt.and_then(|q| {
            question::pin_at(q.question(), p.span.map(ByteSpan::range), "Dark Confidant")
        });
        assert_eq!(
            pinned,
            Some(question::pin_card(question, "bob", "Dark Confidant"))
        );
        // A span the question does not contain travels as "append".
        let out = Outgoing::pick(asker, Audience::Private, question, &spans("urza"));
        let ids = out
            .map(|o| {
                o.components
                    .into_iter()
                    .flat_map(custom_ids)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            matches!(ids.first(), Some(ButtonAction::Pick(p)) if p.span.is_none() && p.audience == Audience::Private),
            "{ids:?}"
        );
        // Too long for the message: no buttons, the caller lists the choices.
        let long = "q".repeat(render::CONTENT_LIMIT);
        assert_eq!(
            Outgoing::pick(asker, Audience::Channel, &long, &spans("q")).err(),
            Some(NoPick::TooLong)
        );
    }

    /// Every initial response goes through [`respond`] (or [`Reply`], in
    /// its own file), which tell the interactions endpoint's [`Ack`]: one
    /// sent any other way would leave Discord's request waiting until
    /// [`endpoint::ACK_WAIT`].
    #[test]
    fn initial_responses_go_through_respond() {
        let call = concat!(".create", "_response(");
        assert_eq!(include_str!("mod.rs").matches(call).count(), 1);
        assert_eq!(include_str!("reply.rs").matches(call).count(), 2);
    }
}
