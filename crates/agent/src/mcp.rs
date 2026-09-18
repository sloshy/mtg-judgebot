//! The MCP server: every [`crate::ops`] operation as a tool, over stdio
//! ([`serve_stdio`]) or as a tower service for `judge-api` to mount
//! ([`http_service`]).
//!
//! The server keeps no per-connection state. A judging session is identified
//! by the id its tools take as an ordinary argument and lives in Postgres,
//! which is the shape MCP 2026-07-28 prescribes now that protocol sessions are
//! gone; over HTTP every protocol version is served statelessly.

use std::sync::Arc;

use judge_core::{Operator, SourceOffer};
use rmcp::{
    Json, ServerHandler, ServiceExt as _,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService, stdio,
        streamable_http_server::session::local::LocalSessionManager,
    },
};

use crate::{
    Toolbox,
    ops::{
        BeginInput, CardInput, ExtractionInput, IdsInput, JudgeInput, LookupInput, NameInput,
        SearchInput, SessionInput, TermInput, VerdictInput,
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
`about` returns the notice below as data.";

/// [`INSTRUCTIONS`] with the source offer and the operator's contact
/// appended: the notice reaches every MCP client at initialization, whatever
/// tools it goes on to call.
#[must_use]
pub fn instructions(offer: &SourceOffer, operator: &Operator) -> String {
    format!("{INSTRUCTIONS}\n\n{}", offer.notice_with(operator))
}

/// The MCP handler.
#[derive(Clone)]
pub struct JudgeMcp {
    toolbox: Arc<Toolbox>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for JudgeMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeMcp")
            .field("toolbox", &self.toolbox)
            .finish_non_exhaustive()
    }
}

/// `{e:#}`: the whole cause chain on one line, as the tool's error text.
fn msg(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

#[tool_router(router = tool_router)]
impl JudgeMcp {
    /// Over a shared toolbox.
    #[must_use]
    pub fn new(toolbox: Arc<Toolbox>) -> Self {
        Self {
            toolbox,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "judge",
        description = "Answer a Magic rules question with the built-in pipeline (the server's own model calls; costs the operator API budget). Returns a validated, cited answer, or `ambiguous` (re-ask with `pins`), `not_found`, `out_of_scope`, `busy`, `rate_limited` (this client's quota for the window), `unavailable` (no API key on the server: use begin_session instead) or `error`. Pass `thread` back for follow-up questions."
    )]
    async fn judge(
        &self,
        Parameters(input): Parameters<JudgeInput>,
    ) -> Result<Json<crate::ops::JudgeReply>, String> {
        self.toolbox
            .judge(input)
            .await
            .map(Json)
            .map_err(|e| msg(&e))
    }

    #[tool(
        name = "begin_session",
        description = "Start answering a rules question yourself, without any server-side model call. Returns the session id and the extraction prompt: produce JSON matching its schema and pass it to submit_extraction. Use `thread` to give a follow-up question the history of earlier ones."
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
        description = "Resolve a card name or nickname the way the pipeline does (aliases, printed names, fuzzy; a name in [[brackets]] matches only that exact card name, offering near spellings as `ambiguous`). Returns the card with its faces and Oracle text, or `ambiguous` with candidates, or `not_found`. Never guesses."
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
        description = "Where this server's source code is (repository and commit), its licence (AGPL-3.0-or-later) and copyright, and how to contact whoever runs it; the same notice the initialization instructions carry. No database access."
    )]
    fn about(&self) -> Json<judge_core::About> {
        Json(self.toolbox.about())
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

#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp's #[tool_handler] expands to async fns that forward without awaiting"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for JudgeMcp {
    fn get_info(&self) -> ServerConfig {
        // The version carries the commit as build metadata (`0.1.0+a37d495`)
        // so a client's server listing already identifies the build.
        let version = match self.toolbox.offer().commit().hash() {
            Some(h) => format!("{}+{}", env!("CARGO_PKG_VERSION"), h.short()),
            None => env!("CARGO_PKG_VERSION").to_owned(),
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mtg-judgebot", version))
            .with_instructions(instructions(self.toolbox.offer(), self.toolbox.operator()))
    }
}

/// Serve on stdin/stdout until the client hangs up. Logging must go to
/// stderr: stdout is the protocol.
///
/// # Errors
/// Transport failures.
pub async fn serve_stdio(toolbox: Arc<Toolbox>) -> anyhow::Result<()> {
    let running = JudgeMcp::new(toolbox).serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// The Streamable HTTP transport as a tower service, to be mounted at a path
/// (`judge-api` puts it at `/mcp`). `allowed_hosts` is what the `Host`
/// header must match — rmcp defaults to loopback only, against DNS
/// rebinding, so a public hostname must be listed.
///
/// Served statelessly for every protocol version: the handler keeps nothing
/// per connection, and a legacy-mode session per `initialize` would live
/// until the client sent `DELETE`, which a token holder need never do.
#[must_use]
pub fn http_service(
    toolbox: Arc<Toolbox>,
    allowed_hosts: Vec<String>,
) -> StreamableHttpService<JudgeMcp, LocalSessionManager> {
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    if !allowed_hosts.is_empty() {
        config.allowed_hosts = allowed_hosts;
    }
    let handler = JudgeMcp::new(toolbox);
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
