//! Discord's REST API, the calls the bot makes ([`wire`] types in and out).
//!
//! Two kinds of route:
//! * An interaction's own: the initial response
//!   (`POST /interactions/{id}/{token}/callback`) and the follow-ups and edits
//!   (`/webhooks/{application}/{token}…`). The token in the path is the
//!   credential, so these send no bot token, and Discord counts them apart
//!   from the bot's global limit.
//! * The bot's: the application, its emoji, the command registration and a
//!   server's roles, sent with `Authorization: Bot …`.
//!
//! A 429 is waited out and sent again ([`RETRIES`], each wait at most
//! [`MAX_RETRY_WAIT`]): Discord refused it unprocessed. Nothing else is
//! retried here, because a request that may have been processed must not be
//! sent twice (a reply, an emoji), and each caller decides what a failure
//! means. No error or log line holds a token: reqwest's errors are stripped
//! of their URL.

use std::{sync::OnceLock, time::Duration};

use judge_llm::ApiKey;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::wire::{
    ApplicationId, Body, CommandDef, EmojiId, GuildId, InteractionId, Response, RoleId, Token,
};

/// Discord's API, version 10.
pub const API_BASE: &str = "https://discord.com/api/v10";

/// How often a 429 is sent again before it is an error.
pub const RETRIES: u32 = 3;
/// The longest a 429 is waited out; a longer `retry_after` is an error.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);
/// The same for a [`Rest::patient`] client (a batch, not a reply).
pub const PATIENT_RETRY_WAIT: Duration = Duration::from_secs(90);
/// How long a connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request may take, start to finish.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A failed call.
#[derive(Debug, thiserror::Error)]
pub enum RestError {
    /// No answer: the connection, the timeout, or a body that did not read.
    /// The URL is stripped (it may hold an interaction token).
    #[error("Discord could not be reached: {0}")]
    Transport(reqwest::Error),
    /// Discord answered with an error.
    #[error("Discord answered {status}: {message}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// Discord's `message` and `code`, or the start of the body.
        message: String,
    },
    /// Discord's answer is not what the route returns.
    #[error("Discord's answer did not read: {0}")]
    Decode(serde_json::Error),
    /// The bot token cannot be a header value (a control character in it).
    #[error("the bot token holds a character a header cannot carry")]
    BadToken,
    /// A bot route was called before the application's id is known.
    #[error("the application's id is not known yet")]
    NoApplication,
}

impl RestError {
    fn transport(e: reqwest::Error) -> Self {
        Self::Transport(e.without_url())
    }
}

/// The application, as `GET /applications/@me` returns it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Application {
    /// Its id.
    pub id: ApplicationId,
    /// Its name.
    pub name: String,
    /// The Ed25519 key interactions posted to the endpoint are signed with
    /// (64 hex digits).
    pub verify_key: String,
    /// Its Interactions Endpoint URL, when one is set.
    #[serde(default)]
    pub interactions_endpoint_url: Option<String>,
}

/// An application emoji.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Emoji {
    /// Its id.
    pub id: EmojiId,
    /// Its name.
    pub name: String,
}

/// A server role.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Role {
    /// Its id.
    pub id: RoleId,
    /// Its name.
    pub name: String,
}

/// Who a request is sent as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth {
    /// The bot's token.
    Bot,
    /// Nobody: the path holds an interaction's token.
    Interaction,
}

/// The client: the bot's token, and its application's id once known.
pub struct Rest {
    client: reqwest::Client,
    base: String,
    token: ApiKey,
    application: OnceLock<ApplicationId>,
    /// The longest 429 waited out.
    max_wait: Duration,
}

impl std::fmt::Debug for Rest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest")
            .field("base", &self.base)
            .field("application", &self.application.get())
            .finish_non_exhaustive()
    }
}

impl Rest {
    /// A client for Discord's API, as the bot whose token is `token`.
    ///
    /// # Errors
    /// The HTTP client could not be built.
    pub fn new(token: &ApiKey) -> anyhow::Result<Self> {
        Self::at(API_BASE, token)
    }

    /// A client for the API at `base` (a test server).
    ///
    /// # Errors
    /// The HTTP client could not be built.
    pub fn at(base: &str, token: &ApiKey) -> anyhow::Result<Self> {
        let agent = format!(
            "DiscordBot ({}, {})",
            env!("CARGO_PKG_REPOSITORY"),
            env!("CARGO_PKG_VERSION")
        );
        let client = reqwest::Client::builder()
            .user_agent(agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // Discord does not redirect its API; a redirect's URL (which may
            // hold a token) would only reach reqwest's own logs.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token: token.clone(),
            application: OnceLock::new(),
            max_wait: MAX_RETRY_WAIT,
        })
    }

    /// The client, waiting out a 429 of up to [`PATIENT_RETRY_WAIT`]: for a
    /// batch (the emoji upload), where no one waits on an answer.
    #[must_use]
    pub fn patient(mut self) -> Self {
        self.max_wait = PATIENT_RETRY_WAIT;
        self
    }

    /// The application's id, once [`Self::application`] has read it (or it
    /// was set).
    #[must_use]
    pub fn application_id(&self) -> Option<ApplicationId> {
        self.application.get().copied()
    }

    /// Say which application the token belongs to, for the bot routes.
    pub fn set_application_id(&self, id: ApplicationId) {
        let _ = self.application.set(id);
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        auth: Auth,
        body: Option<&B>,
    ) -> Result<Vec<u8>, RestError> {
        let url = format!("{}{path}", self.base);
        let mut tries = 0;
        loop {
            let mut req = self.client.request(method.clone(), &url);
            if auth == Auth::Bot {
                // Sensitive: left out of any `Debug` of the request.
                let mut value =
                    reqwest::header::HeaderValue::try_from(format!("Bot {}", self.token.expose()))
                        .map_err(|_| RestError::BadToken)?;
                value.set_sensitive(true);
                req = req.header(reqwest::header::AUTHORIZATION, value);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(RestError::transport)?;
            let status = resp.status();
            let header_wait = retry_after_header(&resp);
            let bytes = resp.bytes().await.map_err(RestError::transport)?;
            if status.is_success() {
                return Ok(bytes.to_vec());
            }
            if status == StatusCode::TOO_MANY_REQUESTS && tries < RETRIES {
                let wait = retry_after_body(&bytes).or(header_wait);
                if let Some(wait) = wait.filter(|w| *w <= self.max_wait) {
                    tries += 1;
                    tracing::warn!(
                        route = route(path),
                        wait_ms = wait.as_millis(),
                        "Discord rate-limited a request; sending it again after the wait"
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            return Err(RestError::Status {
                status: status.as_u16(),
                message: error_message(&bytes),
            });
        }
    }

    async fn json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        auth: Auth,
        body: Option<&B>,
    ) -> Result<T, RestError> {
        let bytes = self.send(method, path, auth, body).await?;
        serde_json::from_slice(&bytes).map_err(RestError::Decode)
    }

    fn app(&self) -> Result<ApplicationId, RestError> {
        self.application_id().ok_or(RestError::NoApplication)
    }

    /// An interaction's initial response.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn respond(
        &self,
        id: InteractionId,
        token: &Token,
        response: &Response,
    ) -> Result<(), RestError> {
        let path = format!("/interactions/{id}/{}/callback", token.expose());
        self.send(Method::POST, &path, Auth::Interaction, Some(response))
            .await
            .map(drop)
    }

    /// A follow-up message to an interaction.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn followup(
        &self,
        application: ApplicationId,
        token: &Token,
        body: &Body,
    ) -> Result<(), RestError> {
        let path = format!("/webhooks/{application}/{}", token.expose());
        self.send(Method::POST, &path, Auth::Interaction, Some(body))
            .await
            .map(drop)
    }

    /// Edit an interaction's initial response (the deferred "thinking…", or
    /// the message a click updated).
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn edit_original(
        &self,
        application: ApplicationId,
        token: &Token,
        body: &Body,
    ) -> Result<(), RestError> {
        let path = format!(
            "/webhooks/{application}/{}/messages/@original",
            token.expose()
        );
        self.send(Method::PATCH, &path, Auth::Interaction, Some(body))
            .await
            .map(drop)
    }

    /// The application the token belongs to; its id is kept for the bot
    /// routes.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn application(&self) -> Result<Application, RestError> {
        let app: Application = self
            .json(Method::GET, "/applications/@me", Auth::Bot, None::<&()>)
            .await?;
        self.set_application_id(app.id);
        Ok(app)
    }

    /// Read the application's id if it is not known yet.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn ensure_application_id(&self) -> Result<ApplicationId, RestError> {
        match self.application_id() {
            Some(id) => Ok(id),
            None => self.application().await.map(|a| a.id),
        }
    }

    /// The application's emoji.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn application_emojis(&self) -> Result<Vec<Emoji>, RestError> {
        #[derive(Deserialize)]
        struct Listed {
            items: Vec<Emoji>,
        }
        let path = format!("/applications/{}/emojis", self.app()?);
        let listed: Listed = self
            .json(Method::GET, &path, Auth::Bot, None::<&()>)
            .await?;
        Ok(listed.items)
    }

    /// Upload an application emoji: `image` is a `data:image/png;base64,…` URI.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn create_application_emoji(&self, name: &str, image: &str) -> Result<(), RestError> {
        let path = format!("/applications/{}/emojis", self.app()?);
        let body = serde_json::json!({ "name": name, "image": image });
        self.send(Method::POST, &path, Auth::Bot, Some(&body))
            .await
            .map(drop)
    }

    /// Replace the registered slash commands with `commands`: in `guild`, or
    /// globally.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn set_commands(
        &self,
        guild: Option<GuildId>,
        commands: &[CommandDef],
    ) -> Result<(), RestError> {
        let app = self.app()?;
        let path = match guild {
            Some(g) => format!("/applications/{app}/guilds/{g}/commands"),
            None => format!("/applications/{app}/commands"),
        };
        self.send(Method::PUT, &path, Auth::Bot, Some(commands))
            .await
            .map(drop)
    }

    /// A server's roles.
    ///
    /// # Errors
    /// [`RestError`].
    pub async fn guild_roles(&self, guild: GuildId) -> Result<Vec<Role>, RestError> {
        self.json(
            Method::GET,
            &format!("/guilds/{guild}/roles"),
            Auth::Bot,
            None::<&()>,
        )
        .await
    }
}

/// A path with its ids and tokens left out, for a log line.
fn route(path: &str) -> String {
    path.split('/')
        .map(|part| {
            if part.len() > 16 || part.chars().any(|c| c.is_ascii_digit()) {
                "…"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The wait a 429's JSON names (`retry_after`, seconds).
fn retry_after_body(body: &[u8]) -> Option<Duration> {
    #[derive(Deserialize)]
    struct Limited {
        retry_after: f64,
    }
    let l: Limited = serde_json::from_slice(body).ok()?;
    Duration::try_from_secs_f64(l.retry_after).ok()
}

/// The wait a 429's `Retry-After` header names (seconds).
fn retry_after_header(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let secs: f64 = v.to_str().ok()?.trim().parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

/// What an error body says: Discord's `message` (and `code`), or its start.
fn error_message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Failed {
        message: String,
        #[serde(default)]
        code: Option<u64>,
    }
    match serde_json::from_slice::<Failed>(body) {
        Ok(Failed {
            message,
            code: Some(code),
        }) => format!("{message} ({code})"),
        Ok(Failed {
            message,
            code: None,
        }) => message,
        Err(_) => String::from_utf8_lossy(body).chars().take(200).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::wire::Body;
    use serde_json::json;
    use std::num::NonZeroU64;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, header_exists, method, path},
    };

    fn id<T: From<NonZeroU64>>(n: u64) -> T {
        T::from(NonZeroU64::new(n).unwrap_or(NonZeroU64::MIN))
    }

    fn token() -> Token {
        serde_json::from_value(json!("itok")).unwrap_or_else(|_| unreachable!("a string"))
    }

    fn rest(server: &MockServer) -> Rest {
        Rest::at(&server.uri(), &ApiKey::from("bot-secret".to_owned()))
            .unwrap_or_else(|e| unreachable!("{e}"))
    }

    #[tokio::test]
    async fn interaction_routes_send_no_bot_token() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/interactions/7/itok/callback"))
            .and(body_json(json!({"type": 5, "data": {"flags": 64}})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/webhooks/8/itok/messages/@original"))
            .and(body_json(json!({"content": "done"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(418))
            .expect(0)
            .mount(&server)
            .await;
        let r = rest(&server);
        r.respond(id(7), &token(), &Response::Defer { ephemeral: true })
            .await?;
        r.edit_original(id(8), &token(), &Body::default().content("done"))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn bot_routes_learn_the_application_and_send_the_token() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/applications/@me"))
            .and(header("authorization", "Bot bot-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "42", "name": "judge", "verify_key": "ab", "flags": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/applications/42/emojis"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"id": "5", "name": "mana_w", "animated": false}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/applications/42/guilds/9/commands"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let r = rest(&server);
        assert!(matches!(
            r.application_emojis().await,
            Err(RestError::NoApplication)
        ));
        let app = r.application().await?;
        assert_eq!((app.id.get(), app.interactions_endpoint_url), (42, None));
        let emoji = r.application_emojis().await?;
        assert_eq!(
            emoji,
            [Emoji {
                id: id(5),
                name: "mana_w".into()
            }]
        );
        r.set_commands(Some(id(9)), &[]).await?;
        assert_eq!(r.ensure_application_id().await?.get(), 42);
        Ok(())
    }

    #[tokio::test]
    async fn a_429_is_waited_out_and_an_error_says_what_discord_said() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(path("/guilds/3/roles"))
            .respond_with(ResponseTemplate::new(429).set_body_json(
                json!({"message": "slow down", "retry_after": 0.05, "global": false}),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/guilds/3/roles"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"id": "6", "name": "Judge", "color": 0}])),
            )
            .mount(&server)
            .await;
        Mock::given(path("/interactions/7/itok/callback"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"message": "Unknown interaction", "code": 10062})),
            )
            .mount(&server)
            .await;
        let r = rest(&server);
        let roles = r.guild_roles(id(3)).await?;
        assert_eq!(roles.first().map(|r| r.name.as_str()), Some("Judge"));
        let err = r
            .respond(id(7), &token(), &Response::Acknowledge)
            .await
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            err.as_deref(),
            Some("Discord answered 404: Unknown interaction (10062)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_error_holds_a_token() {
        // Nothing listens on port 9: the request fails before it is sent.
        let r = Rest::at("http://127.0.0.1:9", &ApiKey::from("bot-secret".to_owned()))
            .unwrap_or_else(|e| unreachable!("{e}"));
        let err = r
            .respond(id(7), &token(), &Response::Acknowledge)
            .await
            .err()
            .map(|e| format!("{e} {e:?}"))
            .unwrap_or_default();
        assert!(!err.is_empty());
        assert!(
            !err.contains("itok") && !err.contains("bot-secret"),
            "{err}"
        );
        assert_eq!(
            route("/webhooks/123/aVeryLongInteractionToken/messages/@original"),
            "/webhooks/…/…/messages/@original"
        );
    }
}
