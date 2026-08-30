//! The Discord adapter (build-order step 6): the `/judge` slash command,
//! rating buttons and the "did you mean…?" flow, on poise 0.6 / serenity 0.12.
//!
//! Everything that does not need the gateway lives in a pure submodule:
//! [`render`] (verdict / error → text), [`ids`] (typed `custom_id`s),
//! [`pending`] (questions waiting on a card pick), [`question`] (span
//! replacement) and [`capture`] (keeps the retrieval `Context` so a call can
//! be persisted). This file is the glue: serenity types in, those modules out.
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

pub mod capture;
pub mod ids;
pub mod pending;
pub mod question;
pub mod render;

use std::{
    num::NonZeroU64,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use judge_core::{
    CallId, CallStore, Deps, JudgeError, Question, Retriever, Score, Validated, Verdict, judge,
};
use poise::serenity_prelude as serenity;
use serenity::{
    ButtonStyle, ComponentInteraction, CreateActionRow, CreateButton, CreateEmbed,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, EditInteractionResponse, FullEvent, GatewayIntents,
    GuildId, Interaction, Member, UserId,
};
use tokio::sync::{Semaphore, SemaphorePermit};

use capture::CapturingRetriever;
use ids::ButtonAction;
use pending::{Pending, PendingId, PendingSpan, PendingStore, TakeError};

/// How long a `/judge` (or a card pick) waits for a free slot before replying
/// "busy". Under Discord's 3 s initial-response deadline, so short bursts queue
/// instead of being refused outright.
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(2);

/// Everything the adapter reads from the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bot token (`DISCORD_TOKEN`).
    pub token: String,
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
}

impl Config {
    /// `JUDGE_ROLE` default.
    pub const DEFAULT_JUDGE_ROLE: &'static str = "Judge";
    /// `JUDGE_CONCURRENCY` default.
    pub const DEFAULT_CONCURRENCY: usize = 2;
    /// Thread history length.
    pub const DEFAULT_HISTORY: usize = 5;

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
    /// [`Self::DEFAULT_CONCURRENCY`], at least 1). Blank values count as unset.
    ///
    /// # Errors
    /// A missing token, or a malformed `GUILD_ID` / `JUDGE_CONCURRENCY`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let token = var("DISCORD_TOKEN").ok_or_else(|| {
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
        Ok(Self {
            token,
            guild_id,
            judge_role,
            max_concurrent,
            history_len: Self::DEFAULT_HISTORY,
            pending_ttl: pending::DEFAULT_TTL,
        })
    }
}

/// Shared state behind every command and button (poise's user data).
pub struct Data {
    deps: Deps,
    capture: Arc<CapturingRetriever>,
    store: Arc<dyn CallStore>,
    client: judge_anthropic::Client,
    pending: PendingStore,
    permits: Semaphore,
    judge_role: String,
    history_len: usize,
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
    /// Wire the shared state. `client` must be (a clone of) the client inside
    /// `deps`, so its spend counters reflect the judge runs.
    #[must_use]
    pub fn new(
        mut deps: Deps,
        store: Arc<dyn CallStore>,
        client: judge_anthropic::Client,
        cfg: &Config,
    ) -> Self {
        let capture = Arc::new(CapturingRetriever::new(Arc::clone(&deps.retriever)));
        deps.retriever = Arc::clone(&capture) as Arc<dyn Retriever>;
        Self {
            deps,
            capture,
            store,
            client,
            pending: PendingStore::new(cfg.pending_ttl),
            permits: Semaphore::new(cfg.max_concurrent),
            judge_role: cfg.judge_role.clone(),
            history_len: cfg.history_len,
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
    async fn answer(&self, q: &Question, asker: UserId) -> Outgoing {
        let history = match self.store.history(&q.thread_id, self.history_len).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    error = format_args!("{e:#}"),
                    "thread history unavailable; judging without it"
                );
                vec![]
            }
        };
        let (t0, usd0, calls0) = (Instant::now(), self.client.spent_usd(), self.client.calls());
        let result = judge(&self.deps, q, &history).await;
        let captured = self.capture.take(q);
        tracing::info!(
            user = %asker,
            thread = %q.thread_id,
            elapsed_ms = t0.elapsed().as_millis(),
            usd = format_args!("{:.4}", self.client.spent_usd() - usd0),
            llm_calls = self.client.calls() - calls0,
            outcome = outcome(&result),
            "/judge"
        );
        match result {
            Ok(v) => {
                let call = if let Some(ctx) = captured {
                    self.store
                        .persist(q, &v, &ctx)
                        .await
                        .map_err(|e| {
                            tracing::error!(
                                error = format_args!("{e:#}"),
                                "persist failed; answering without rating buttons"
                            );
                        })
                        .ok()
                } else {
                    tracing::warn!("no captured context for the question; call not persisted");
                    None
                };
                Outgoing::answer(&v, call)
            }
            Err(JudgeError::AmbiguousCards(spans)) => {
                let dym = render::did_you_mean(&spans);
                let token = self.pending.insert(Pending {
                    thread_id: q.thread_id.clone(),
                    user_id: asker.to_string(),
                    text: q.text.clone(),
                    spans: spans.map(PendingSpan::from),
                });
                Outgoing {
                    content: dym.content,
                    embed: None,
                    components: vec![pick_row(token, &dym.choices)],
                }
            }
            Err(e) => {
                tracing::warn!(error = format_args!("{e:#}"), "judge failed");
                Outgoing {
                    content: render::error(&e),
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
                render::rated(score, is_judge)
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
        let out = self.answer(&q, c.user.id).await;
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

impl Outgoing {
    fn answer(v: &Verdict<Validated>, call: Option<CallId>) -> Self {
        let a = render::answer(v);
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
            .components(self.components);
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
#[poise::command(slash_command, rename = "judge")]
async fn judge_command(
    ctx: Ctx<'_>,
    #[description = "Your rules question (write a card as [[Full Name]] to pin it)"]
    question: String,
) -> Result<(), Error> {
    let data = ctx.data();
    let Some(_permit) = data.acquire().await else {
        ctx.send(
            poise::CreateReply::default()
                .content(render::BUSY)
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };
    // Answers take 20–45 s; Discord wants an acknowledgement within 3 s.
    ctx.defer().await?;
    let q = Question {
        thread_id: ctx.channel_id().to_string(),
        text: question,
    };
    let out = data.answer(&q, ctx.author().id).await;
    ctx.send(out.into_reply()).await?;
    Ok(())
}

async fn event_handler(
    ctx: &serenity::Context,
    event: &FullEvent,
    _framework: poise::FrameworkContext<'_, Data, Error>,
    data: &Data,
) -> Result<(), Error> {
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
                command = ctx.command().name,
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

/// Connect to the gateway and serve until the connection ends.
///
/// # Errors
/// Building the client (bad token) or a fatal gateway error.
pub async fn run(cfg: Config, data: Data) -> anyhow::Result<()> {
    let guild = cfg.guild_id;
    let options = poise::FrameworkOptions {
        commands: vec![judge_command()],
        event_handler: |ctx, event, framework, data| {
            Box::pin(event_handler(ctx, event, framework, data))
        },
        on_error: |e| Box::pin(on_error(e)),
        ..Default::default()
    };
    let framework = poise::Framework::builder()
        .options(options)
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                let commands = &framework.options().commands;
                if let Some(g) = guild {
                    poise::builtins::register_in_guild(ctx, commands, g).await?;
                    tracing::info!(guild = %g, "registered /judge in one guild");
                } else {
                    poise::builtins::register_globally(ctx, commands).await?;
                    tracing::info!(
                        "registered /judge globally (propagation can take up to an hour)"
                    );
                }
                Ok(data)
            })
        })
        .build();
    // Slash commands and button presses arrive without any gateway intent.
    let mut client = serenity::ClientBuilder::new(&cfg.token, GatewayIntents::empty())
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

        let cfg = Config::from_vars(vars(&[("DISCORD_TOKEN", "tok")])).ok();
        let cfg = cfg.as_ref();
        assert_eq!(cfg.map(|c| c.token.as_str()), Some("tok"));
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
