//! The MCP server: every [`crate::ops`] operation as a tool, over stdio
//! ([`serve_stdio`]) or as a tower service for `judgebot --mcp` to mount
//! ([`http_service`], and [`admin_http_service`] for `MCP_ADMIN_TOKEN`).
//!
//! What a connection may call is its handler's type: [`JudgeMcp<Public>`]
//! has the judge, session and lookup tools and nothing else, and
//! [`JudgeMcp<Privileged>`] adds the admin tools, which exist only on that
//! type and need the [`Admin`] capability it holds. `MCP_TOKEN` reaches the
//! first; the second is built only by [`serve_stdio`] (the operator's own
//! process over the database) and [`admin_http_service`].
//!
//! The server keeps no per-connection state. A judging session is identified
//! by the id its tools take as an ordinary argument and lives in Postgres,
//! which is the shape MCP 2026-07-28 prescribes now that protocol sessions are
//! gone; over HTTP every protocol version is served statelessly.

use std::sync::Arc;

use judge_bot::admin::McpAdminToken;
use judge_core::{Operator, SourceOffer};
use rmcp::{
    Json, RoleServer, ServerHandler, ServiceExt as _,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService, stdio,
        streamable_http_server::session::local::LocalSessionManager,
    },
};

use crate::{
    Toolbox,
    admin::Admin,
    ops::{
        BeginInput, CallInput, CallsInput, CardInput, ExtractionInput, FailuresInput, IdsInput,
        JudgeInput, LookupInput, NameInput, SearchInput, SessionInput, StatsInput, TermInput,
        VerdictInput,
    },
};

/// What a client is told at initialization: the two ways to get an answer,
/// the order of the session steps, and (appended by [`instructions`]) the
/// source offer.
pub const INSTRUCTIONS: &str = "\
Magic: The Gathering rules judge over the Comprehensive Rules, Oracle text, Scryfall rulings and rated prior calls.

Two ways to answer a rules question:

1. `judge` runs the whole pipeline with the server's own model calls and returns a validated, cited answer. \
It spends the operator's API budget and is only available when the server has an API key; it replies \
`unavailable` otherwise.

2. A session runs the same pipeline but YOU are the model: no API call is made. Steps, in order:
   a. `begin_session` → session id + the extraction prompt (system, user, schema). Answer it yourself: \
produce JSON matching the schema (card-name spans exactly as written, rules concepts, primary/secondary \
category, source).
   b. `submit_extraction` → the synthesis prompt (system, material, question, schema), or `ambiguous` / \
`not_found` (fix the spans — write a card as [[Full Card Name]], which matches only that exact name — and submit again), or `out_of_scope`.
   c. Optionally `lookup_rules` ONCE with rule ids or subsections you need beyond the material (at most \
10); then re-read the prompt with `session_prompt`. A rejected verdict forfeits this round, so look up \
before you answer, not after.
   d. Answer the synthesis prompt yourself: JSON with `answer` (at most 4000 characters), `confidence`, \
`citations` (every quote copied verbatim from the material, curly apostrophes included), `category`. \
`submit_verdict` validates every citation against the material; `rejected` returns the prompt again \
under `retry` with the reason, once; `accepted` is the answer, closing the session. Set `persist: true` \
to store it as history for follow-ups in the same `thread` (it is never shown to other askers). \
`session_status` says where a session is; sessions expire after an hour idle.

Lookups that need no session: `resolve_card` (name → card, or candidates), `card_info` (Oracle text, \
rulings, notes by oracle id), `get_rules` (by id, at most 10), `search_rules` (free text), `glossary`. \
`about` returns the notice below as data, with the Comprehensive Rules version loaded and when the data \
was last refreshed.";

/// What a connection holding the admin capability is told besides
/// [`INSTRUCTIONS`].
pub const ADMIN_INSTRUCTIONS: &str = "\
Admin tools (this connection holds the operator's credential): `admin_calls` (every stored call, \
private ones included, newest first, filterable by interface, asker, model, private flag and text), \
`admin_call` (one call with its origin, ratings and their raters, runs and charges), \
`admin_failures` (failed calls: the question, the error, what the model sent) and `admin_usage` \
(questions, spend, runs by model and outcome, ratings, the lowest-rated calls, recent data \
refreshes). They return people's questions and Discord ids: keep them to the operator's own \
conversation.";

/// [`INSTRUCTIONS`] with the source offer and the operator's contact
/// appended: the notice reaches every MCP client at initialization, whatever
/// tools it goes on to call.
#[must_use]
pub fn instructions(offer: &SourceOffer, operator: &Operator) -> String {
    format!(
        "{INSTRUCTIONS}\n\n{}",
        offer.notice_with(operator, crate::ops::AGENT_LOCALE)
    )
}

/// What a connection may call. Sealed: [`Public`] and [`Privileged`] are the
/// only two.
pub trait Access: Clone + Send + Sync + 'static + sealed::Sealed {
    /// Whether the admin section is added to the instructions.
    fn is_privileged(&self) -> bool;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Public {}
    impl Sealed for super::Privileged {}
}

/// The judge, session and lookup tools: what `MCP_TOKEN` reaches.
#[derive(Clone, Copy, Debug)]
pub struct Public;

impl Access for Public {
    fn is_privileged(&self) -> bool {
        false
    }
}

/// Those and the admin tools, with the capability they take.
#[derive(Clone, Debug)]
pub struct Privileged(Admin);

impl Access for Privileged {
    fn is_privileged(&self) -> bool {
        true
    }
}

/// The MCP handler for a connection with access `A`.
#[derive(Clone)]
pub struct JudgeMcp<A: Access> {
    toolbox: Arc<Toolbox>,
    access: A,
    tool_router: ToolRouter<Self>,
}

impl<A: Access> std::fmt::Debug for JudgeMcp<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeMcp")
            .field("toolbox", &self.toolbox)
            .field("operator", &self.access.is_privileged())
            .finish_non_exhaustive()
    }
}

/// `{e:#}`: the whole cause chain on one line, as the tool's error text.
fn msg(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

impl JudgeMcp<Public> {
    /// The public tools over a shared toolbox.
    #[must_use]
    pub fn new(toolbox: Arc<Toolbox>) -> Self {
        Self {
            toolbox,
            access: Public,
            tool_router: Self::tool_router(),
        }
    }
}

impl JudgeMcp<Privileged> {
    /// The public and the admin tools over a shared toolbox.
    #[must_use]
    pub fn privileged(toolbox: Arc<Toolbox>, admin: Admin) -> Self {
        Self {
            toolbox,
            access: Privileged(admin),
            tool_router: Self::tool_router() + Self::admin_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl<A: Access> JudgeMcp<A> {
    /// [`instructions`], led by what this server says about the instance's
    /// release when it has something to say (a standalone `judge-mcp` of
    /// another release; `release_notice` warms it before serving), and
    /// followed by the admin section on an operator's connection.
    fn instructions(&self) -> String {
        let base = instructions(self.toolbox.offer(), self.toolbox.operator());
        let base = if self.access.is_privileged() {
            format!("{base}\n\n{ADMIN_INSTRUCTIONS}")
        } else {
            base
        };
        match self.toolbox.release_notice_cached() {
            Some(crate::release::Notice::Stop(why)) => format!(
                "THIS SERVER CANNOT ANSWER: {why} Every tool but `about` returns this error until it is updated. \
                 Tell the user.\n\n{base}"
            ),
            Some(crate::release::Notice::Info(note)) => format!("Note: {note}\n\n{base}"),
            None => base,
        }
    }

    #[tool(
        name = "judge",
        description = "Answer a Magic rules question with the built-in pipeline (the server's own model calls; costs the operator API budget). Returns a validated, cited answer, or `ambiguous` (re-ask with `pins`), `not_found`, `out_of_scope`, `busy`, `rate_limited` (this client's quota for the window), `unavailable` (no API key on the server: use begin_session instead) or `error`. Pass `thread` back for follow-up questions. Set `private` to keep the call off everything public: only the operator sees it."
    )]
    async fn judge(
        &self,
        Parameters(input): Parameters<JudgeInput>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<crate::ops::JudgeReply>, String> {
        // rmcp keeps a cancelled request's handler running and only cancels
        // its token (on `notifications/cancelled`, or when the session ends).
        // Dropping the judge future on it is what tells the run its caller
        // went away (`judge_bot::run`), so no further model call is sent.
        tokio::select! {
            reply = self.toolbox.judge(input) => reply.map(Json).map_err(|e| msg(&e)),
            () = ctx.ct.cancelled() => Err("cancelled".to_owned()),
        }
    }

    #[tool(
        name = "begin_session",
        description = "Start answering a rules question yourself, without any server-side model call. Returns the session id and the extraction prompt: produce JSON matching its schema and pass it to submit_extraction. Use `thread` to give a follow-up question the history of earlier ones. Set `private` to store the session's call private."
    )]
    async fn begin_session(
        &self,
        Parameters(input): Parameters<BeginInput>,
    ) -> Result<Json<judge_bot::session::Begun>, String> {
        self.toolbox
            .begin_session(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "session_prompt",
        description = "Re-read the prompt for the session's current step (the extraction prompt, or the synthesis prompt including anything lookup_rules fetched)."
    )]
    async fn session_prompt(
        &self,
        Parameters(input): Parameters<SessionInput>,
    ) -> Result<Json<judge_bot::session::Prompt>, String> {
        self.toolbox
            .session_prompt(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "session_status",
        description = "Where a session is: its stage, whether the lookup round is still available, how it ended."
    )]
    async fn session_status(
        &self,
        Parameters(input): Parameters<SessionInput>,
    ) -> Result<Json<crate::ops::SessionStatus>, String> {
        self.toolbox
            .session_status(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "submit_extraction",
        description = "Hand in the extraction JSON for a session. Resolves the card spans and retrieves the material; returns the synthesis prompt (`ready`), or `ambiguous` / `not_found` (the session is unchanged: rewrite the span as [[Full Card Name]], which matches only that exact name, and submit again), or `out_of_scope` (closed)."
    )]
    async fn submit_extraction(
        &self,
        Parameters(input): Parameters<ExtractionInput>,
    ) -> Result<Json<crate::ops::ExtractionReply>, String> {
        self.toolbox
            .submit_extraction(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "lookup_rules",
        description = "The session's one chance to fetch CR text beyond the material: rule ids (`702.19`, `613.7b`) or whole subsections (`613`). Ask for everything at once; a second call is refused. The chunks are added to the session's material and become citable."
    )]
    async fn lookup_rules(
        &self,
        Parameters(input): Parameters<LookupInput>,
    ) -> Result<Json<crate::ops::Rules>, String> {
        self.toolbox
            .lookup_rules(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "submit_verdict",
        description = "Hand in the verdict JSON for a session (`answer`, `confidence`, `citations`, `category`). Every citation is checked against the session's material: the id must be shown there and the quote must be a verbatim substring. Returns `accepted` (with `persist: true`, also stored as a call), `rejected` (one retry: answer the returned prompt) or `exhausted` (closed)."
    )]
    async fn submit_verdict(
        &self,
        Parameters(input): Parameters<VerdictInput>,
    ) -> Result<Json<crate::ops::VerdictReply>, String> {
        self.toolbox
            .submit_verdict(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "persist_session",
        description = "Store an accepted session verdict as a call (idempotent)."
    )]
    async fn persist_session(
        &self,
        Parameters(input): Parameters<SessionInput>,
    ) -> Result<Json<crate::ops::Persisted>, String> {
        self.toolbox
            .persist_session(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "resolve_card",
        description = "Resolve a card name or nickname the way the pipeline does (aliases, printed names, fuzzy; a name in [[brackets]] matches only that exact card name or a listed nickname, offering near spellings as `ambiguous`). Returns the card with its faces and Oracle text, or `ambiguous` with candidates, or `not_found`. Never guesses."
    )]
    async fn resolve_card(
        &self,
        Parameters(input): Parameters<NameInput>,
    ) -> Result<Json<judge_core::Resolution>, String> {
        self.toolbox
            .resolve_card(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "card_info",
        description = "A card by oracle id (from resolve_card): faces with Oracle text, all Scryfall rulings, and any hand-written notes."
    )]
    async fn card_info(
        &self,
        Parameters(input): Parameters<CardInput>,
    ) -> Result<Json<crate::ops::CardInfo>, String> {
        self.toolbox
            .card_info(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "get_rules",
        description = "Comprehensive Rules chunks by id (`702.19`, `613.7b`) or whole subsection (`613`), with the CR effective date."
    )]
    async fn get_rules(
        &self,
        Parameters(input): Parameters<IdsInput>,
    ) -> Result<Json<crate::ops::Rules>, String> {
        self.toolbox
            .get_rules(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "search_rules",
        description = "Search the Comprehensive Rules by text (full-text, plus vector similarity when configured). Rules vocabulary works best."
    )]
    async fn search_rules(
        &self,
        Parameters(input): Parameters<SearchInput>,
    ) -> Result<Json<crate::ops::Rules>, String> {
        self.toolbox
            .search_rules(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "about",
        description = "Where this server's source code is (repository and commit), its licence (AGPL-3.0-or-later) and copyright, and how to contact whoever runs it (the same notice the initialization instructions carry), plus what its data is: the Comprehensive Rules version loaded and how long ago the data was last refreshed (`freshness`, null when the database does not answer)."
    )]
    async fn about(&self) -> Json<judge_core::About> {
        Json(self.toolbox.about().await)
    }

    #[tool(
        name = "glossary",
        description = "Comprehensive Rules glossary entries for a term (exact matches first, then containing)."
    )]
    async fn glossary(
        &self,
        Parameters(input): Parameters<TermInput>,
    ) -> Result<Json<crate::ops::Glossary>, String> {
        self.toolbox
            .glossary(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }
}

#[tool_router(router = admin_router)]
impl JudgeMcp<Privileged> {
    #[tool(
        name = "admin_calls",
        description = "Admin: every stored call, private ones included, newest first, 50 a page (pass `next` back as `cursor`). Filter by `interface` (discord, web, mcp, cli, agent_session, agent), `asker` (a Discord username or user id), `model` (one of the `models` listed), `private` (true: only private; false: only public) and `q` (text in the question, answer or asker's username). Each row has the asker, the private flag, the models its run billed and its cost."
    )]
    async fn admin_calls(
        &self,
        Parameters(input): Parameters<CallsInput>,
    ) -> Result<Json<judge_bot::admin::CallsPage>, String> {
        tracing::info!(tool = "admin_calls", "admin tool called");
        self.toolbox
            .admin_calls(&self.access.0, input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "admin_call",
        description = "Admin: everything stored about one call by id: who asked and where (Discord username and id, private flag, thread), the question and answer verbatim, citations, cards, every rating with its rater, and the run that answered it with each billed request's model, tokens and cost. `call` is null for an id with no call."
    )]
    async fn admin_call(
        &self,
        Parameters(input): Parameters<CallInput>,
    ) -> Result<Json<crate::ops::CallReply>, String> {
        tracing::info!(tool = "admin_call", "admin tool called");
        self.toolbox
            .admin_call(&self.access.0, input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "admin_failures",
        description = "Admin: the newest failed calls (default 10, at most 100), kept 30 days: the question, the error, the first rejection and what the model sent on each attempt."
    )]
    async fn admin_failures(
        &self,
        Parameters(input): Parameters<FailuresInput>,
    ) -> Result<Json<crate::ops::Failures>, String> {
        tracing::info!(tool = "admin_failures", "admin tool called");
        self.toolbox
            .failures(&self.access.0, input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "admin_usage",
        description = "Admin: usage over the last `days` UTC days (default 30, at most 366): stored calls per day and interface, spend per day, the runs by model (tokens, cost) and by outcome, ratings, the lowest-rated calls and the latest data refreshes."
    )]
    async fn admin_usage(
        &self,
        Parameters(input): Parameters<StatsInput>,
    ) -> Result<Json<judge_bot::db::stats::Usage>, String> {
        tracing::info!(tool = "admin_usage", "admin tool called");
        self.toolbox
            .stats(&self.access.0, input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }
}

#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp's #[tool_handler] expands to async fns that forward without awaiting"
)]
#[tool_handler(router = self.tool_router)]
impl<A: Access> ServerHandler for JudgeMcp<A> {
    fn get_info(&self) -> ServerConfig {
        // The version carries the commit as build metadata (`1.0.0+a37d495`)
        // so a client's server listing already identifies the build.
        let version = match self.toolbox.offer().commit().hash() {
            Some(h) => format!("{}+{}", env!("CARGO_PKG_VERSION"), h.short()),
            None => env!("CARGO_PKG_VERSION").to_owned(),
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mtg-judgebot", version))
            .with_instructions(self.instructions())
    }
}

/// Serve on stdin/stdout until the client hangs up. Logging must go to
/// stderr: stdout is the protocol. A standalone toolbox (its own process
/// over the database, [`crate::Local`]) serves the admin tools too: whoever
/// runs it holds `DATABASE_URL`.
///
/// # Errors
/// Transport failures.
pub async fn serve_stdio(local: crate::Local) -> anyhow::Result<()> {
    let crate::Local { toolbox, admin } = local;
    if let Some(notice) = toolbox.release_notice().await {
        tracing::warn!("{}", notice.text());
    }
    let running = JudgeMcp::privileged(Arc::new(toolbox), admin)
        .serve(stdio())
        .await?;
    running.waiting().await?;
    Ok(())
}

/// The Streamable HTTP config for `judgebot --mcp`: stateless for every
/// protocol version, JSON replies, and `allowed_hosts` as what the `Host`
/// header must match (rmcp defaults to loopback only, against DNS
/// rebinding, so a public hostname must be listed).
fn http_config(allowed_hosts: Vec<String>) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    if !allowed_hosts.is_empty() {
        config.allowed_hosts = allowed_hosts;
    }
    config
}

/// The Streamable HTTP transport as a tower service with the public tools,
/// to be mounted at a path (`judgebot --mcp` puts it at `/mcp`, behind
/// `MCP_TOKEN`).
///
/// Served statelessly for every protocol version: the handler keeps nothing
/// per connection, and a legacy-mode session per `initialize` would live
/// until the client sent `DELETE`, which a token holder need never do.
#[must_use]
pub fn http_service(toolbox: Arc<Toolbox>, allowed_hosts: Vec<String>) -> PublicHttp {
    let handler = JudgeMcp::new(toolbox);
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config(allowed_hosts),
    )
}

/// The Streamable HTTP service whose handler has the public tools only.
pub type PublicHttp = StreamableHttpService<JudgeMcp<Public>, LocalSessionManager>;

/// The Streamable HTTP service whose handler has the admin tools.
pub type AdminHttp = StreamableHttpService<JudgeMcp<Privileged>, LocalSessionManager>;

/// The `/mcp` service with the admin tools, and the token it must be gated
/// behind. Made only by [`admin_http_service`].
pub struct AdminService {
    token: McpAdminToken,
    service: AdminHttp,
}

impl std::fmt::Debug for AdminService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminService").finish_non_exhaustive()
    }
}

impl AdminService {
    /// The bearer token a request must carry to reach [`Self::service`]:
    /// `MCP_ADMIN_TOKEN`, never `MCP_TOKEN` (they are checked to differ).
    #[must_use]
    pub fn token(&self) -> &str {
        self.token.expose()
    }

    /// The service.
    #[must_use]
    pub fn service(&self) -> AdminHttp {
        self.service.clone()
    }
}

/// [`http_service`] with the admin tools as well, for the requests that
/// carry `MCP_ADMIN_TOKEN`: the one place a remote connection gets the
/// [`Admin`] capability, and only from a checked [`McpAdminToken`].
#[must_use]
pub fn admin_http_service(
    toolbox: Arc<Toolbox>,
    allowed_hosts: Vec<String>,
    token: McpAdminToken,
) -> AdminService {
    let handler = JudgeMcp::privileged(toolbox, Admin::grant());
    AdminService {
        token,
        service: StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            http_config(allowed_hosts),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<A: Access>(h: &JudgeMcp<A>) -> Vec<String> {
        h.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    const ADMIN_TOOLS: [&str; 4] = ["admin_calls", "admin_call", "admin_failures", "admin_usage"];

    /// The public handler has no admin tool, and the operator's has every
    /// public tool and the admin ones.
    #[tokio::test]
    async fn only_the_operator_handler_lists_the_admin_tools() -> anyhow::Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nowhere/none")?;
        let toolbox = Arc::new(Toolbox::new(pool, crate::tests::options(None)));
        let public = names(&JudgeMcp::new(Arc::clone(&toolbox)));
        let operator = names(&JudgeMcp::privileged(Arc::clone(&toolbox), Admin::grant()));
        for t in ADMIN_TOOLS {
            assert!(!public.contains(&t.to_owned()), "{t} on the public handler");
            assert!(
                operator.contains(&t.to_owned()),
                "{t} missing for the operator"
            );
        }
        for t in &public {
            assert!(operator.contains(t), "{t} missing for the operator");
        }
        assert!(public.contains(&"judge".to_owned()));
        assert!(
            !JudgeMcp::new(Arc::clone(&toolbox))
                .instructions()
                .contains("admin_calls")
        );
        assert!(
            JudgeMcp::privileged(toolbox, Admin::grant())
                .instructions()
                .contains("admin_calls")
        );
        Ok(())
    }
}
