//! The Discord gateway connection: one shard, zero intents, no cache and no
//! compression, acting on two dispatches only (`READY` and
//! `INTERACTION_CREATE`). [`run`] keeps it up until it is told to stop.
//!
//! **Connecting.** [`Connect::Discover`] asks `GET /gateway/bot` for the URL
//! and the session start limit before each fresh identify (never before a
//! resume); with no identifies left it waits the limit's `reset_after`.
//! [`Connect::Url`] connects straight to a URL. Either way the socket is
//! `{url}/?v=10&encoding=json`, JSON text frames, TLS through rustls with
//! the `ring` provider and the webpki roots ([`tls`]), never rustls's
//! process default: two providers are in the graph, and it would panic.
//!
//! **One connection.** Hello (op 10) sets the heartbeat: the first after a
//! random part of the interval, then every interval, each carrying the last
//! sequence number. Op 1 from Discord asks for one at once. A heartbeat due
//! with no ACK (op 11) since the one before means the connection is a zombie:
//! it is closed with 4000, which keeps the session resumable. After Hello the
//! shard identifies (op 2), or resumes (op 6) on READY's
//! `resume_gateway_url` when it holds a session.
//!
//! **Ending one.** What follows a connection's end:
//! * op 7 Reconnect, a transport error, EOF, or any close code not below:
//!   resume when there is a session, otherwise identify;
//! * op 9 Invalid Session: `true` resumes; `false` drops the session and
//!   identifies afresh after a random 1 to 5 s;
//! * close 4007 or 4009: drop the session and identify;
//! * close 4004, 4010 to 4014: fatal, [`run`] returns [`ShardError::Fatal`].
//!
//! Reconnects wait 1 s, doubling to 60 s, reset by a READY or RESUMED.
//!
//! **Stopping.** The stop future wins every race: once it resolves, the
//! connection is closed with 1000 (waiting at most [`CLOSE_WAIT`] for it to
//! complete) and no event is delivered after it.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use futures::{SinkExt as _, StreamExt as _};
use judge_llm::ApiKey;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        self, Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

/// Where to connect.
#[derive(Clone, Debug)]
pub enum Connect {
    /// Ask the REST API (`GET {api_base}/gateway/bot`, `Authorization: Bot
    /// <token>`) for the URL and the session start limit. Production passes
    /// [`super::rest::API_BASE`].
    Discover {
        /// The REST API's base, without a trailing `/gateway/bot`.
        api_base: String,
    },
    /// Connect straight to this `ws://` or `wss://` URL (tests).
    Url(String),
}

/// Discord's REST API, version 10: the production [`Connect::Discover`] base.
/// Failed connects in a row to a session's resume URL before the session is
/// dropped and the shard identifies afresh.
const RESUME_CONNECT_ATTEMPTS: u32 = 3;

/// What [`run`] connects with.
#[derive(Clone, Debug)]
pub struct Config {
    /// The bot token (its `Debug` is redacted).
    pub token: ApiKey,
    /// Where to connect.
    pub connect: Connect,
}

/// What the gateway tells the caller.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A READY: the bot's username and how many guilds it is in.
    Ready {
        /// `d.user.username`.
        user: String,
        /// `d.guilds.len()`.
        guilds: usize,
    },
    /// An `INTERACTION_CREATE`'s `d`, as Discord sent it.
    Interaction(Value),
}

/// Why [`run`] gave up. No variant carries the token.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    /// Discord closed the connection with a code that retrying cannot fix.
    #[error("Discord closed the gateway with code {code} ({meaning}); not reconnecting")]
    Fatal {
        /// The close code.
        code: u16,
        /// What Discord documents it as.
        meaning: &'static str,
    },
    /// `GET /gateway/bot` answered 401: the token is not valid.
    #[error("Discord refused the bot token (401 from /gateway/bot)")]
    Unauthorized,
    /// The TLS configuration could not be built.
    #[error("building the gateway's TLS configuration: {0}")]
    Tls(#[source] rustls::Error),
    /// The HTTP client for `GET /gateway/bot` could not be built.
    #[error("building the gateway's HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// How long a close (ours) may take to complete before the socket is
/// dropped.
pub const CLOSE_WAIT: Duration = Duration::from_millis(500);

/// The waits [`run`] uses, injectable so the tests take milliseconds.
#[derive(Clone, Copy, Debug)]
struct Timing {
    /// The first reconnect's wait, doubled per reconnect up to `backoff_max`.
    backoff_first: Duration,
    backoff_max: Duration,
    /// After op 9 `false`: a random wait in `[invalid_min, invalid_max)`.
    invalid_min: Duration,
    invalid_max: Duration,
    /// The most a close of ours may take.
    close_wait: Duration,
    /// How long a new connection may go without Hello.
    hello_wait: Duration,
    /// How long opening a connection (TCP, TLS, upgrade) may take.
    connect_timeout: Duration,
    /// The whole `GET /gateway/bot` request.
    discover_timeout: Duration,
}

/// Discord's documented waits.
const DISCORD: Timing = Timing {
    backoff_first: Duration::from_secs(1),
    backoff_max: Duration::from_secs(60),
    invalid_min: Duration::from_secs(1),
    invalid_max: Duration::from_secs(5),
    close_wait: CLOSE_WAIT,
    hello_wait: Duration::from_secs(20),
    connect_timeout: Duration::from_secs(30),
    discover_timeout: Duration::from_secs(15),
};

/// Connect and serve until `stop` resolves (then close the connection with
/// code 1000 and return `Ok(())`) or a fatal close code arrives. Every
/// transient failure is handled inside: resumes and re-identifies with
/// backoff, never returning for them.
///
/// `on_event` is called synchronously in dispatch order and must not block
/// (the caller spawns tasks). After `stop` has resolved it is never called
/// again.
///
/// # Errors
/// [`ShardError::Fatal`] on a fatal close code, [`ShardError::Unauthorized`]
/// when discovery refuses the token, and the two build errors at start.
pub async fn run(
    cfg: Config,
    on_event: impl FnMut(Event) + Send,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), ShardError> {
    run_with(cfg, DISCORD, on_event, stop).await
}

/// The rustls client configuration: `ring` (which sqlx, always built here,
/// already enables) and the webpki roots, so nothing depends on a process
/// default provider.
fn tls() -> Result<rustls::ClientConfig, ShardError> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(ShardError::Tls)?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// The session a resume continues.
#[derive(Debug)]
struct Resumable {
    id: String,
    url: String,
}

/// What survives from one connection to the next.
#[derive(Debug, Default)]
struct State {
    session: Option<Resumable>,
    seq: Option<u64>,
}

impl State {
    fn drop_session(&mut self) {
        self.session = None;
        self.seq = None;
    }
}

/// A doubling wait, reset by a successful handshake.
#[derive(Debug)]
struct Backoff {
    first: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    fn new(timing: &Timing) -> Self {
        Self {
            first: timing.backoff_first,
            max: timing.backoff_max,
            next: timing.backoff_first,
        }
    }

    fn next(&mut self) -> Duration {
        let wait = self.next;
        self.next = wait.saturating_mul(2).min(self.max);
        wait
    }

    fn reset(&mut self) {
        self.next = self.first;
    }
}

async fn run_with<S: Future<Output = ()> + Send>(
    cfg: Config,
    timing: Timing,
    mut on_event: impl FnMut(Event) + Send,
    stop: S,
) -> Result<(), ShardError> {
    let tls = Arc::new(tls()?);
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timing.discover_timeout)
        .build()
        .map_err(ShardError::Client)?;
    let mut stop = std::pin::pin!(stop);
    let mut state = State::default();
    let mut backoff = Backoff::new(&timing);
    let mut discover_backoff = Backoff::new(&timing);
    let mut wait = Duration::ZERO;
    // Failed connects to the resume URL in a row; at
    // [`RESUME_CONNECT_ATTEMPTS`] the session is dropped for a fresh identify
    // on the discovered URL, so a retired resume host is not retried forever.
    let mut failed_resumes = 0u32;
    loop {
        if !wait.is_zero() && pause(wait, stop.as_mut()).await {
            return Ok(());
        }
        let url = if let Some(session) = &state.session {
            session.url.clone()
        } else {
            state.seq = None;
            match &cfg.connect {
                Connect::Url(url) => url.clone(),
                Connect::Discover { api_base } => {
                    let found = tokio::select! {
                        biased;
                        () = stop.as_mut() => return Ok(()),
                        found = discover(&http, api_base, &cfg.token) => found,
                    };
                    match found {
                        Ok(gateway) => {
                            discover_backoff.reset();
                            let limit = gateway.session_start_limit;
                            if limit.remaining == 0 {
                                let reset = Duration::from_millis(limit.reset_after);
                                tracing::warn!(
                                    wait_ms = limit.reset_after,
                                    "Discord gateway: no session starts left, waiting for the limit to reset"
                                );
                                if pause(reset, stop.as_mut()).await {
                                    return Ok(());
                                }
                            }
                            gateway.url
                        }
                        Err(Discovery::Unauthorized) => return Err(ShardError::Unauthorized),
                        Err(Discovery::Failed(why)) => {
                            wait = discover_backoff.next();
                            tracing::warn!(
                                error = %why,
                                retry_in_ms = wait.as_millis(),
                                "Discord gateway: GET /gateway/bot failed"
                            );
                            continue;
                        }
                    }
                }
            }
        };
        let opened = tokio::select! {
            biased;
            () = stop.as_mut() => return Ok(()),
            opened = timeout(timing.connect_timeout, open(&url, &tls)) => opened,
        };
        let failed = match opened {
            Ok(Ok(socket)) => Ok(socket),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("no connection within the connect timeout".to_owned()),
        };
        let socket = match failed {
            Ok(socket) => {
                failed_resumes = 0;
                socket
            }
            Err(error) => {
                wait = backoff.next();
                tracing::warn!(%error, retry_in_ms = wait.as_millis(), "Discord gateway: connecting failed");
                if state.session.is_some() {
                    failed_resumes += 1;
                    if failed_resumes >= RESUME_CONNECT_ATTEMPTS {
                        tracing::warn!(
                            attempts = failed_resumes,
                            "Discord gateway: the resume URL does not answer; identifying afresh"
                        );
                        state.drop_session();
                        failed_resumes = 0;
                    }
                }
                continue;
            }
        };
        let end = connection(
            socket,
            &cfg.token,
            &mut state,
            &mut backoff,
            &timing,
            &mut on_event,
            stop.as_mut(),
        )
        .await;
        match end {
            End::Stopped => return Ok(()),
            End::Fatal(code) => {
                return Err(ShardError::Fatal {
                    code,
                    meaning: fatal_meaning(code),
                });
            }
            End::Resume => wait = backoff.next(),
            End::Reidentify => {
                state.drop_session();
                wait = backoff.next();
            }
            End::InvalidSession => {
                state.drop_session();
                wait = timing.invalid_min
                    + random_below(timing.invalid_max.saturating_sub(timing.invalid_min));
            }
        }
    }
}

/// Discord's required `User-Agent` form for REST calls.
const USER_AGENT: &str = concat!(
    "DiscordBot (",
    env!("CARGO_PKG_REPOSITORY"),
    ", ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// Wait `wait` unless `stop` resolves first; `true` when it did.
async fn pause<S: Future<Output = ()>>(wait: Duration, stop: Pin<&mut S>) -> bool {
    tokio::select! {
        biased;
        () = stop => true,
        () = sleep(wait) => false,
    }
}

/// A uniformly random duration in `[0, max)`, in whole milliseconds. A v4
/// UUID is the randomness the crate already has (as `jobs::jitter`).
fn random_below(max: Duration) -> Duration {
    let max = max.as_millis().max(1);
    let ms = uuid::Uuid::new_v4().as_u128() % max;
    Duration::from_millis(u64::try_from(ms).unwrap_or_default())
}

/// `GET /gateway/bot`'s answer, the parts used.
#[derive(Debug, Deserialize)]
struct GatewayBot {
    url: String,
    session_start_limit: StartLimit,
}

/// The identify budget.
#[derive(Debug, Deserialize)]
struct StartLimit {
    remaining: u32,
    /// Milliseconds until it resets.
    reset_after: u64,
}

/// Why discovery failed.
#[derive(Debug)]
enum Discovery {
    /// 401: the token is bad, retrying cannot help.
    Unauthorized,
    /// Anything else, retried with backoff. The text names no token.
    Failed(String),
}

async fn discover(
    http: &reqwest::Client,
    api_base: &str,
    token: &ApiKey,
) -> Result<GatewayBot, Discovery> {
    let url = format!("{}/gateway/bot", api_base.trim_end_matches('/'));
    let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bot {}", token.expose()))
        .map_err(|_| Discovery::Failed("the token is not a valid header value".to_owned()))?;
    auth.set_sensitive(true);
    let response = http
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(|e| Discovery::Failed(e.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(Discovery::Unauthorized);
    }
    if !status.is_success() {
        return Err(Discovery::Failed(format!("HTTP {status}")));
    }
    response
        .json::<GatewayBot>()
        .await
        .map_err(|e| Discovery::Failed(format!("reading the answer: {e}")))
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn open(url: &str, tls: &Arc<rustls::ClientConfig>) -> Result<Socket, tungstenite::Error> {
    let url = format!("{}/?v=10&encoding=json", url.trim_end_matches('/'));
    let (socket, _) = connect_async_tls_with_config(
        url.as_str(),
        None,
        true,
        Some(Connector::Rustls(Arc::clone(tls))),
    )
    .await?;
    Ok(socket)
}

/// How one connection ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// `stop` resolved; closed with 1000.
    Stopped,
    /// A fatal close code.
    Fatal(u16),
    /// Reconnect: resume when there is a session, else identify.
    Resume,
    /// Reconnect after dropping the session.
    Reidentify,
    /// Op 9 `false`: drop the session, wait 1 to 5 s, identify.
    InvalidSession,
}

/// A gateway payload, the parts read.
#[derive(Debug, Deserialize)]
struct Payload {
    op: u8,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

/// READY's `d`, the parts used.
#[derive(Debug, Deserialize)]
struct Ready {
    session_id: String,
    resume_gateway_url: String,
    user: ReadyUser,
    guilds: Vec<serde::de::IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct ReadyUser {
    username: String,
}

const DISPATCH: u8 = 0;
const HEARTBEAT: u8 = 1;
const IDENTIFY: u8 = 2;
const RESUME: u8 = 6;
const RECONNECT: u8 = 7;
const INVALID_SESSION: u8 = 9;
const HELLO: u8 = 10;
const HEARTBEAT_ACK: u8 = 11;

/// The close code of ours that keeps the session resumable.
const RESUMABLE_CLOSE: u16 = 4000;

fn heartbeat(seq: Option<u64>) -> Message {
    Message::text(json!({ "op": HEARTBEAT, "d": seq }).to_string())
}

fn identify(token: &ApiKey) -> Message {
    Message::text(
        json!({
            "op": IDENTIFY,
            "d": {
                "token": token.expose(),
                "intents": 0,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "judgebot",
                    "device": "judgebot",
                },
            },
        })
        .to_string(),
    )
}

fn resume(token: &ApiKey, session: &Resumable, seq: Option<u64>) -> Message {
    Message::text(
        json!({
            "op": RESUME,
            "d": { "token": token.expose(), "session_id": session.id, "seq": seq },
        })
        .to_string(),
    )
}

/// Send a close frame with `code` and wait (at most `wait`) for the other
/// side's.
async fn close(socket: &mut Socket, code: u16, wait: Duration) {
    let closing = async {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        if socket.close(Some(frame)).await.is_err() {
            return;
        }
        while let Some(Ok(_)) = socket.next().await {}
    };
    if timeout(wait, closing).await.is_err() {
        tracing::debug!(code, "Discord gateway: close did not complete in time");
    }
}

/// What a close code from Discord means for the next connection.
fn closed_by_discord(frame: Option<&CloseFrame>) -> End {
    let Some(frame) = frame else {
        tracing::info!("Discord gateway: closed with no code");
        return End::Resume;
    };
    let code = u16::from(frame.code);
    let end = match code {
        4004 | 4010..=4014 => End::Fatal(code),
        4007 | 4009 => End::Reidentify,
        _ => End::Resume,
    };
    tracing::warn!(code, reason = %frame.reason, next = ?end, "Discord gateway: closed by Discord");
    end
}

fn fatal_meaning(code: u16) -> &'static str {
    match code {
        4004 => "authentication failed",
        4010 => "invalid shard",
        4011 => "sharding required",
        4012 => "invalid API version",
        4013 => "invalid intents",
        4014 => "disallowed intents",
        _ => "fatal",
    }
}

/// Serve one connection until it ends.
async fn connection<S: Future<Output = ()>>(
    mut socket: Socket,
    token: &ApiKey,
    state: &mut State,
    backoff: &mut Backoff,
    timing: &Timing,
    on_event: &mut impl FnMut(Event),
    mut stop: Pin<&mut S>,
) -> End {
    let mut interval: Option<Duration> = None;
    let mut next_beat = Instant::now() + timing.hello_wait;
    let mut acked = true;
    loop {
        tokio::select! {
            biased;
            () = stop.as_mut() => {
                close(&mut socket, 1000, timing.close_wait).await;
                return End::Stopped;
            }
            () = sleep_until(next_beat) => {
                let Some(every) = interval else {
                    tracing::warn!("Discord gateway: no Hello in time, reconnecting");
                    close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                    return End::Resume;
                };
                if !acked {
                    tracing::warn!("Discord gateway: heartbeat not acknowledged, reconnecting");
                    close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                    return End::Resume;
                }
                if let Err(e) = socket.send(heartbeat(state.seq)).await {
                    tracing::warn!(error = %e, "Discord gateway: sending a heartbeat failed");
                    return End::Resume;
                }
                acked = false;
                next_beat = Instant::now() + every;
            }
            message = socket.next() => {
                let text = match message {
                    None => {
                        tracing::info!("Discord gateway: connection ended");
                        return End::Resume;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "Discord gateway: connection failed");
                        return End::Resume;
                    }
                    Some(Ok(Message::Close(frame))) => return closed_by_discord(frame.as_ref()),
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                };
                let payload = match serde_json::from_str::<Payload>(text.as_str()) {
                    Ok(payload) => payload,
                    Err(e) => {
                        tracing::warn!(error = %e, "Discord gateway: unreadable payload, ignored");
                        continue;
                    }
                };
                match payload.op {
                    HELLO => {
                        let Some(ms) = payload.d.get("heartbeat_interval").and_then(Value::as_u64) else {
                            tracing::warn!("Discord gateway: Hello without a heartbeat interval");
                            close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                            return End::Resume;
                        };
                        let every = Duration::from_millis(ms.max(1));
                        interval = Some(every);
                        next_beat = Instant::now() + random_below(every);
                        let handshake = match &state.session {
                            Some(session) => resume(token, session, state.seq),
                            None => identify(token),
                        };
                        if let Err(e) = socket.send(handshake).await {
                            tracing::warn!(error = %e, "Discord gateway: sending the handshake failed");
                            return End::Resume;
                        }
                    }
                    HEARTBEAT_ACK => acked = true,
                    HEARTBEAT => {
                        if let Err(e) = socket.send(heartbeat(state.seq)).await {
                            tracing::warn!(error = %e, "Discord gateway: sending a heartbeat failed");
                            return End::Resume;
                        }
                    }
                    DISPATCH => {
                        if let Some(seq) = payload.s {
                            state.seq = Some(seq);
                        }
                        match payload.t.as_deref() {
                            Some("READY") => match serde_json::from_value::<Ready>(payload.d) {
                                Ok(ready) => {
                                    state.session = Some(Resumable {
                                        id: ready.session_id,
                                        url: ready.resume_gateway_url,
                                    });
                                    backoff.reset();
                                    tracing::info!(user = %ready.user.username, guilds = ready.guilds.len(), "Discord gateway: ready");
                                    on_event(Event::Ready {
                                        user: ready.user.username,
                                        guilds: ready.guilds.len(),
                                    });
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Discord gateway: unreadable READY, identifying again");
                                    close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                                    return End::Reidentify;
                                }
                            },
                            Some("RESUMED") => {
                                backoff.reset();
                                tracing::info!("Discord gateway: resumed");
                            }
                            Some("INTERACTION_CREATE") => on_event(Event::Interaction(payload.d)),
                            _ => {}
                        }
                    }
                    RECONNECT => {
                        tracing::info!("Discord gateway: asked to reconnect");
                        close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                        return End::Resume;
                    }
                    INVALID_SESSION => {
                        let resumable = payload.d.as_bool().unwrap_or(false);
                        tracing::warn!(resumable, "Discord gateway: invalid session");
                        close(&mut socket, RESUMABLE_CLOSE, timing.close_wait).await;
                        return if resumable { End::Resume } else { End::InvalidSession };
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, Result, bail};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot},
        task::JoinHandle,
    };

    const TOKEN: &str = "secret-token-xyz";

    const FAST: Timing = Timing {
        backoff_first: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        invalid_min: Duration::from_millis(10),
        invalid_max: Duration::from_millis(20),
        close_wait: CLOSE_WAIT,
        hello_wait: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(2),
        discover_timeout: Duration::from_secs(2),
    };

    /// The longest any one step of a test waits.
    const STEP: Duration = Duration::from_secs(5);

    type Server = WebSocketStream<TcpStream>;

    struct Client {
        task: JoinHandle<Result<(), ShardError>>,
        events: mpsc::UnboundedReceiver<Event>,
        stop: Option<oneshot::Sender<()>>,
    }

    impl Client {
        fn stop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
        }

        async fn event(&mut self) -> Result<Event> {
            timeout(STEP, self.events.recv())
                .await
                .context("no event in time")?
                .context("the shard ended")
        }

        async fn ended(self) -> Result<Result<(), ShardError>> {
            Ok(timeout(STEP, self.task)
                .await
                .context("run did not return")??)
        }
    }

    fn start(connect: Connect) -> Client {
        let (tx, events) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let cfg = Config {
            token: ApiKey::from(TOKEN),
            connect,
        };
        let task = tokio::spawn(run_with(
            cfg,
            FAST,
            move |event| {
                let _ = tx.send(event);
            },
            async move {
                let _ = stopped.await;
            },
        ));
        Client {
            task,
            events,
            stop: Some(stop),
        }
    }

    async fn listener() -> Result<(TcpListener, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        Ok((listener, url))
    }

    /// Accept one connection, checking the query string.
    async fn accept(listener: &TcpListener) -> Result<Server> {
        let (stream, _) = timeout(STEP, listener.accept())
            .await
            .context("no connection in time")??;
        let (path, socket) = {
            let seen = Arc::new(std::sync::Mutex::new(String::new()));
            let into = Arc::clone(&seen);
            let socket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &tungstenite::handshake::server::Request, response| {
                    if let Ok(mut path) = into.lock() {
                        *path = request.uri().to_string();
                    }
                    Ok(response)
                },
            )
            .await?;
            let path = seen.lock().map(|p| p.clone()).unwrap_or_default();
            (path, socket)
        };
        if path != "/?v=10&encoding=json" {
            bail!("connected on {path}");
        }
        Ok(socket)
    }

    async fn send(socket: &mut Server, payload: Value) -> Result<()> {
        socket.send(Message::text(payload.to_string())).await?;
        Ok(())
    }

    async fn hello(socket: &mut Server, interval_ms: u64) -> Result<()> {
        send(
            socket,
            json!({ "op": 10, "d": { "heartbeat_interval": interval_ms } }),
        )
        .await
    }

    /// The next message: a payload, or the close code.
    async fn next(socket: &mut Server) -> Result<std::result::Result<Value, Option<u16>>> {
        loop {
            let message = timeout(STEP, socket.next())
                .await
                .context("nothing in time")?
                .context("connection ended")??;
            match message {
                Message::Text(text) => return Ok(Ok(serde_json::from_str(text.as_str())?)),
                Message::Close(frame) => return Ok(Err(frame.map(|f| u16::from(f.code)))),
                _ => {}
            }
        }
    }

    /// The next payload with `op`, skipping heartbeats (unless `op` is one).
    async fn expect_op(socket: &mut Server, op: u64) -> Result<Value> {
        loop {
            match next(socket).await? {
                Ok(payload) if payload.get("op").and_then(Value::as_u64) == Some(op) => {
                    return Ok(payload);
                }
                Ok(payload) if payload.get("op").and_then(Value::as_u64) == Some(1) => {}
                Ok(payload) => bail!("expected op {op}, got {payload}"),
                Err(code) => bail!("expected op {op}, got close {code:?}"),
            }
        }
    }

    /// The close code the client sends, skipping payloads.
    async fn expect_close(socket: &mut Server) -> Result<Option<u16>> {
        loop {
            if let Err(code) = next(socket).await? {
                return Ok(code);
            }
        }
    }

    async fn ready(socket: &mut Server, seq: u64, resume_url: &str) -> Result<()> {
        send(
            socket,
            json!({
                "op": 0, "s": seq, "t": "READY",
                "d": {
                    "session_id": "sess-1",
                    "resume_gateway_url": resume_url,
                    "user": { "username": "judgebot", "id": "1" },
                    "guilds": [{ "id": "10", "unavailable": true }, { "id": "11", "unavailable": true }],
                },
            }),
        )
        .await
    }

    async fn close_with(socket: &mut Server, code: u16) -> Result<()> {
        socket
            .close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "test".into(),
            }))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn identifies_then_delivers_ready_and_interactions() -> Result<()> {
        let (listener, url) = listener().await?;
        let mut client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        hello(&mut server, 60_000).await?;
        let identify = expect_op(&mut server, 2).await?;
        assert_eq!(identify.pointer("/d/token"), Some(&json!(TOKEN)));
        assert_eq!(identify.pointer("/d/intents"), Some(&json!(0)));
        assert_eq!(
            identify.pointer("/d/properties/browser"),
            Some(&json!("judgebot"))
        );
        ready(&mut server, 1, &url).await?;
        let interaction =
            json!({ "id": "99", "type": 2, "data": { "name": "judge" }, "token": "itoken" });
        send(
            &mut server,
            json!({ "op": 0, "s": 2, "t": "GUILD_CREATE", "d": { "id": "10" } }),
        )
        .await?;
        send(
            &mut server,
            json!({ "op": 0, "s": 3, "t": "INTERACTION_CREATE", "d": interaction }),
        )
        .await?;
        assert_eq!(
            client.event().await?,
            Event::Ready {
                user: "judgebot".to_owned(),
                guilds: 2
            }
        );
        assert_eq!(client.event().await?, Event::Interaction(interaction));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn heartbeats_carry_the_last_sequence() -> Result<()> {
        let (listener, url) = listener().await?;
        let client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        hello(&mut server, 30).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 5, &url).await?;
        // Heartbeats sent before READY carry null; one after it carries 5.
        let mut beats = 0;
        loop {
            let beat = expect_op(&mut server, 1).await?;
            send(&mut server, json!({ "op": 11 })).await?;
            beats += 1;
            if beat.get("d") == Some(&json!(5)) {
                break;
            }
            if beats > 20 {
                bail!("no heartbeat carried seq 5: {beat}");
            }
        }
        let mut client = client;
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn op_1_asks_for_a_heartbeat_at_once() -> Result<()> {
        let (listener, url) = listener().await?;
        let mut client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        // A minute's interval: the scheduled beats are unlikely within the test.
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 7, &url).await?;
        let started = Instant::now();
        send(&mut server, json!({ "op": 1, "d": null })).await?;
        let beat = expect_op(&mut server, 1).await?;
        assert_eq!(beat.get("d"), Some(&json!(7)));
        assert!(started.elapsed() < Duration::from_secs(1));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn op_7_resumes_on_the_resume_url() -> Result<()> {
        let (first, url) = listener().await?;
        let (second, resume_url) = listener().await?;
        let mut client = start(Connect::Url(url));
        let mut server = accept(&first).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 3, &resume_url).await?;
        send(&mut server, json!({ "op": 7, "d": null })).await?;
        let code = expect_close(&mut server).await?;
        assert_ne!(code, Some(1000), "a 1000 would end the session");
        let mut resumed = accept(&second).await?;
        hello(&mut resumed, 60_000).await?;
        let resume = expect_op(&mut resumed, 6).await?;
        assert_eq!(resume.pointer("/d/session_id"), Some(&json!("sess-1")));
        assert_eq!(resume.pointer("/d/seq"), Some(&json!(3)));
        assert_eq!(resume.pointer("/d/token"), Some(&json!(TOKEN)));
        send(
            &mut resumed,
            json!({ "op": 0, "s": 4, "t": "RESUMED", "d": null }),
        )
        .await?;
        assert!(matches!(client.event().await?, Event::Ready { .. }));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn a_resume_url_that_never_answers_gives_way_to_a_fresh_identify() -> Result<()> {
        let (first, url) = listener().await?;
        // A port nothing listens on: every connect is refused.
        let (gone, dead_url) = listener().await?;
        drop(gone);
        let mut client = start(Connect::Url(url));
        let mut server = accept(&first).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 3, &dead_url).await?;
        send(&mut server, json!({ "op": 7, "d": null })).await?;
        expect_close(&mut server).await?;
        let mut again = accept(&first).await?;
        hello(&mut again, 60_000).await?;
        let identify = expect_op(&mut again, 2).await?;
        assert_eq!(identify.pointer("/d/token"), Some(&json!(TOKEN)));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn op_9_false_identifies_afresh_on_the_configured_url() -> Result<()> {
        let (first, url) = listener().await?;
        let (_unused, resume_url) = listener().await?;
        let mut client = start(Connect::Url(url));
        let mut server = accept(&first).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 3, &resume_url).await?;
        send(&mut server, json!({ "op": 9, "d": false })).await?;
        expect_close(&mut server).await?;
        let mut again = accept(&first).await?;
        hello(&mut again, 60_000).await?;
        let identify = expect_op(&mut again, 2).await?;
        assert_eq!(identify.pointer("/d/token"), Some(&json!(TOKEN)));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn a_fatal_close_code_ends_the_run() -> Result<()> {
        let (listener, url) = listener().await?;
        let client = start(Connect::Url(url));
        let mut server = accept(&listener).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        close_with(&mut server, 4004).await?;
        match client.ended().await? {
            Err(e @ ShardError::Fatal { code: 4004, .. }) => {
                let shown = format!("{e} {e:?}");
                assert!(!shown.contains(TOKEN), "{shown}");
                Ok(())
            }
            other => bail!("expected a fatal 4004, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_4000_close_resumes() -> Result<()> {
        let (listener, url) = listener().await?;
        let mut client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 8, &url).await?;
        close_with(&mut server, 4000).await?;
        let mut again = accept(&listener).await?;
        hello(&mut again, 60_000).await?;
        let resume = expect_op(&mut again, 6).await?;
        assert_eq!(resume.pointer("/d/seq"), Some(&json!(8)));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn a_missed_ack_closes_and_resumes() -> Result<()> {
        let (listener, url) = listener().await?;
        let mut client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        hello(&mut server, 40).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 2, &url).await?;
        // Never acknowledge: the next beat due finds none.
        let code = expect_close(&mut server).await?;
        assert_eq!(code, Some(RESUMABLE_CLOSE));
        let mut again = accept(&listener).await?;
        hello(&mut again, 60_000).await?;
        let resume = expect_op(&mut again, 6).await?;
        assert_eq!(resume.pointer("/d/session_id"), Some(&json!("sess-1")));
        client.stop();
        client.ended().await?.map_err(anyhow::Error::from)
    }

    #[tokio::test]
    async fn stop_closes_with_1000_and_delivers_nothing_after() -> Result<()> {
        let (listener, url) = listener().await?;
        let mut client = start(Connect::Url(url.clone()));
        let mut server = accept(&listener).await?;
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        ready(&mut server, 1, &url).await?;
        assert!(matches!(client.event().await?, Event::Ready { .. }));
        client.stop();
        send(
            &mut server,
            json!({ "op": 0, "s": 2, "t": "INTERACTION_CREATE", "d": { "id": "1" } }),
        )
        .await?;
        let code = expect_close(&mut server).await?;
        assert_eq!(code, Some(1000));
        // Drain so the server's close reply is flushed.
        drop(timeout(Duration::from_millis(100), server.next()).await);
        let mut events = client.events;
        let ended = timeout(STEP, client.task).await??;
        assert!(ended.is_ok(), "{ended:?}");
        assert!(events.try_recv().is_err(), "an event after stop");
        Ok(())
    }

    #[tokio::test]
    async fn stop_while_backing_off_returns_promptly() -> Result<()> {
        // A port nothing listens on: every connect fails and backs off.
        let (listener, url) = listener().await?;
        drop(listener);
        let mut client = start(Connect::Url(url));
        sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        client.stop();
        client.ended().await??;
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[tokio::test]
    async fn discovery_waits_out_the_start_limit() -> Result<()> {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };
        let (listener, url) = listener().await?;
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gateway/bot"))
            .and(header("authorization", format!("Bot {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "url": url,
                "shards": 1,
                "session_start_limit": {
                    "total": 1000, "remaining": 0, "reset_after": 300, "max_concurrency": 1,
                },
            })))
            .expect(1)
            .mount(&api)
            .await;
        let started = Instant::now();
        let mut client = start(Connect::Discover {
            api_base: api.uri(),
        });
        let mut server = accept(&listener).await?;
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        hello(&mut server, 60_000).await?;
        expect_op(&mut server, 2).await?;
        client.stop();
        client.ended().await??;
        Ok(())
    }

    #[tokio::test]
    async fn discovery_refusing_the_token_is_fatal() -> Result<()> {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gateway/bot"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&api)
            .await;
        let client = start(Connect::Discover {
            api_base: api.uri(),
        });
        match client.ended().await? {
            Err(ShardError::Unauthorized) => Ok(()),
            other => bail!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn close_codes_are_classified() {
        let frame = |code: u16| CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert_eq!(closed_by_discord(Some(&frame(code))), End::Fatal(code));
        }
        for code in [4007, 4009] {
            assert_eq!(closed_by_discord(Some(&frame(code))), End::Reidentify);
        }
        for code in [1000, 1001, 4000, 4001, 4008] {
            assert_eq!(closed_by_discord(Some(&frame(code))), End::Resume);
        }
        assert_eq!(closed_by_discord(None), End::Resume);
    }

    #[test]
    fn backoff_doubles_to_its_cap_and_resets() {
        let mut backoff = Backoff::new(&DISCORD);
        let waits: Vec<u64> = (0..8).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
    }

    #[test]
    fn the_tls_configuration_builds_without_a_process_default() {
        assert!(tls().is_ok());
    }

    #[test]
    fn debug_never_shows_the_token() {
        let cfg = Config {
            token: ApiKey::from(TOKEN),
            connect: Connect::Url("ws://x".to_owned()),
        };
        assert!(!format!("{cfg:?}").contains(TOKEN));
    }
}
