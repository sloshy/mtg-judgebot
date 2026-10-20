//! Discord's interactions endpoint (D30). With `DISCORD_INTERACTIONS=http`,
//! Discord POSTs each slash command and button click to the application's
//! Interactions Endpoint URL instead of sending it over the gateway; the URL
//! reaches [`PATH`] on the process's HTTP listener, and the HTTP adapter
//! hands each request to [`Interactions::receive`].
//!
//! A request is answered in this order:
//! 1. Not yet open (the role has not fetched the application's public key):
//!    it waits up to [`OPEN_WAIT`] for the endpoint to open, then `503`. A
//!    new container listens a moment before it opens, and a request the
//!    proxy hands it then is Discord's only attempt. `/api/health` fails
//!    while it is closed, so a deploy waits for it.
//! 2. No valid Ed25519 signature over the timestamp and the body, or a
//!    timestamp more than [`FRESHNESS`] from this host's clock: `401`.
//!    Discord sends invalid signatures on purpose, and removes the URL from
//!    an application whose endpoint accepts one.
//! 3. A body that is no interaction: `400`.
//! 4. A PING: `200 {"type":1}`, in the body, as Discord requires.
//! 5. Anything else is dispatched on a task of its own, counted as in-flight
//!    work for the shutdown drain, and the request is answered `202` with no
//!    body once the handler has sent its initial response over the REST
//!    callback (or has ended, or at [`ACK_WAIT`]). Discord documents the
//!    `202` for an interaction answered through the callback; the follow-ups
//!    go over REST as they do for the gateway. [`ACK_WAIT`] counts from the
//!    request's arrival, the wait for the endpoint to open included.
//!
//! A replayed request (one copied from the tunnel, say) gets nowhere after
//! [`FRESHNESS`]. Within it, its token has been used, so the handler's first
//! call to Discord fails and no model is called; what a handler does before
//! that call (a `/judge` charges the asker's window, `/forget` forgets) does
//! happen again.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::Duration,
};

use judge_core::Locale;
use serenity::all::{Http, Interaction, Verifier};

use super::{Data, Delivery, reply::Ack};
use crate::{alert::catalog as alert, shutdown::Shutdown};

/// Where the endpoint is served.
pub const PATH: &str = "/discord/interactions";
/// The header holding the hex signature.
pub const SIGNATURE_HEADER: &str = "x-signature-ed25519";
/// The header holding the signed timestamp.
pub const TIMESTAMP_HEADER: &str = "x-signature-timestamp";
/// The longest a request waits, from its arrival, for its handler's initial
/// response before it is answered anyway. Discord's deadline is 3 s from
/// sending; a `/judge` waits up to [`super::ACQUIRE_WAIT`] for a slot before
/// it answers "busy".
pub const ACK_WAIT: Duration = Duration::from_millis(2800);
const _: () = assert!(ACK_WAIT.as_millis() > super::ACQUIRE_WAIT.as_millis());
const _: () = assert!(ACK_WAIT.as_millis() < 3000);
/// How far a request's signed timestamp may be from this host's clock.
/// Discord gives no window (its deadline is 3 s); this one bounds a replay
/// while leaving room for a host clock that has drifted.
pub const FRESHNESS: Duration = Duration::from_mins(5);

/// The longest a request waits for a closed endpoint to open. Opening takes
/// one REST call to Discord after the listener binds.
pub const OPEN_WAIT: Duration = Duration::from_millis(500);
const _: () = assert!(OPEN_WAIT.as_millis() < ACK_WAIT.as_millis());

/// How a request to the endpoint is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// `503`: the endpoint is not open yet.
    NotReady,
    /// `401`: no valid signature.
    Unauthorized,
    /// `400`: signed, but no interaction this bot can read.
    Malformed,
    /// `200` with [`Received::body`]: a PING.
    Pong,
    /// `202`, no body: dispatched, its initial response sent over REST.
    Accepted,
}

impl Received {
    /// The HTTP status.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::NotReady => 503,
            Self::Unauthorized => 401,
            Self::Malformed => 400,
            Self::Pong => 200,
            Self::Accepted => 202,
        }
    }

    /// The JSON body, if the answer has one.
    #[must_use]
    pub const fn body(self) -> Option<&'static str> {
        match self {
            Self::Pong => Some(r#"{"type":1}"#),
            Self::NotReady | Self::Unauthorized | Self::Malformed | Self::Accepted => None,
        }
    }
}

type Task = Pin<Box<dyn Future<Output = ()> + Send>>;
/// The handler: the interaction, a submitted form's raw `data.components`
/// (serenity drops what a Label holds, [`super::feedback`]), and the
/// acknowledgement to tell.
type Run = Arc<dyn Fn(Interaction, Option<serde_json::Value>, Ack) -> Task + Send + Sync>;

struct Endpoint {
    verifier: Verifier,
    run: Run,
    shutdown: Shutdown,
}

/// The endpoint, shared by the Discord role, which opens it once it can
/// verify requests, and the HTTP adapter, which serves it.
#[derive(Clone, Default)]
pub struct Interactions(Arc<Shared>);

#[derive(Default)]
struct Shared {
    endpoint: OnceLock<Endpoint>,
    /// Set once `endpoint` is, for a request waiting for it.
    opened: tokio::sync::watch::Sender<bool>,
    /// Set once the listener serving it has stopped and answered every
    /// request it accepted: no interaction arrives after that.
    closed: tokio::sync::watch::Sender<bool>,
    /// Whether the last signed request was refused for its timestamp: the
    /// first of a streak is an ERROR and an alert, the rest a WARN.
    skewed: std::sync::atomic::AtomicBool,
    /// Where that alert goes.
    alert: OnceLock<crate::alert::AlertWebhook>,
}

impl std::fmt::Debug for Interactions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Interactions")
            .field(&if self.is_open() { "open" } else { "not open" })
            .finish()
    }
}

impl Interactions {
    /// Not open yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether requests are verified and dispatched.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.endpoint.get().is_some()
    }

    /// Open it: requests signed by `verifier`'s key go to `data`, answered
    /// over `http`, counted as work for `shutdown`. A second open is ignored.
    pub(crate) fn open(
        &self,
        verifier: Verifier,
        data: Arc<Data>,
        http: Arc<Http>,
        shutdown: Shutdown,
        alert: Option<crate::alert::AlertWebhook>,
    ) {
        if let Some(hook) = alert {
            let _ = self.0.alert.set(hook);
        }
        let run: Run = Arc::new(move |interaction, form, ack| {
            Box::pin(super::dispatch_guarded(
                Arc::clone(&data),
                Arc::clone(&http),
                interaction,
                form,
                ack,
            ))
        });
        self.open_with(verifier, run, shutdown);
    }

    fn open_with(&self, verifier: Verifier, run: Run, shutdown: Shutdown) {
        let opened = self.0.endpoint.set(Endpoint {
            verifier,
            run,
            shutdown,
        });
        if opened.is_err() {
            tracing::warn!("the interactions endpoint was opened twice; kept the first");
        }
        self.0.opened.send_replace(true);
    }

    /// The listener serving it has stopped, every request it accepted
    /// answered: called by the HTTP adapter.
    pub fn close(&self) {
        self.0.closed.send_replace(true);
    }

    /// Until [`Self::close`].
    pub async fn closed(&self) {
        let mut closed = self.0.closed.subscribe();
        // The sender lives as long as `self`.
        let _ = closed.wait_for(|c| *c).await;
    }

    /// Until it opens.
    pub async fn opened(&self) {
        let mut opened = self.0.opened.subscribe();
        // The sender lives as long as `self`.
        let _ = opened.wait_for(|open| *open).await;
    }

    /// A signed request refused for its timestamp. A replay is one request;
    /// a host clock that has drifted refuses every one, so the first of a
    /// streak says so loudly.
    fn refuse_skewed(&self, skew: Option<u64>) {
        let already = self
            .0
            .skewed
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if already {
            tracing::warn!(
                skew_secs = skew,
                "a signed interaction refused for its timestamp"
            );
            return;
        }
        let text = |locale| {
            (crate::alert::catalog::messages(locale).discord_clock)(skew, FRESHNESS.as_secs())
        };
        // Logs stay English; the webhook hears its own language.
        tracing::error!("{}", text(Locale::En));
        if let Some(hook) = self.0.alert.get().cloned() {
            let text = text(hook.locale());
            tokio::spawn(async move {
                crate::alert::post(&crate::alert::client(), &hook, "Discord clock alert", &text)
                    .await;
            });
        }
    }

    /// The endpoint, waiting up to [`OPEN_WAIT`] for it to open.
    async fn endpoint(&self) -> Option<&Endpoint> {
        if let Some(e) = self.0.endpoint.get() {
            return Some(e);
        }
        let mut opened = self.0.opened.subscribe();
        // A timeout leaves it closed; `get` says so either way.
        let _ = tokio::time::timeout(OPEN_WAIT, opened.wait_for(|open| *open)).await;
        self.0.endpoint.get()
    }

    /// Answer one request: its two signature headers (if present) and its
    /// raw body. See the module docs for the order.
    pub async fn receive(
        &self,
        signature: Option<&str>,
        timestamp: Option<&str>,
        body: &[u8],
    ) -> Received {
        let arrived = tokio::time::Instant::now();
        let Some(endpoint) = self.endpoint().await else {
            return Received::NotReady;
        };
        let (Some(signature), Some(timestamp)) = (signature, timestamp) else {
            return Received::Unauthorized;
        };
        if endpoint
            .verifier
            .verify(signature, timestamp, body)
            .is_err()
        {
            return Received::Unauthorized;
        }
        match skew(timestamp, unix_now()) {
            Some(skew) if skew <= FRESHNESS.as_secs() => {
                self.0
                    .skewed
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            skew => {
                self.refuse_skewed(skew);
                return Received::Unauthorized;
            }
        }
        let interaction = match serde_json::from_slice::<Interaction>(body) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "a signed request to the interactions endpoint is no interaction this bot reads");
                return Received::Malformed;
            }
        };
        if matches!(interaction, Interaction::Ping(_)) {
            return Received::Pong;
        }
        let form = match interaction {
            Interaction::Modal(_) => super::feedback::submitted(body),
            _ => None,
        };
        let ack = Ack::arrived_at(arrived);
        // Taken here, before the spawn, so the drain counts the interaction
        // from the moment it was accepted.
        let work = endpoint.shutdown.work();
        let handler = (endpoint.run)(interaction, form, ack.clone());
        let mut task = tokio::spawn(async move {
            let _work = work;
            handler.await;
        });
        tokio::select! {
            () = ack.wait() => {}
            _ = &mut task => {}
            () = tokio::time::sleep_until(arrived + ACK_WAIT) => tracing::warn!(
                wait_ms = ACK_WAIT.as_millis(),
                "an interaction's handler sent no initial response in time; answering Discord's request anyway"
            ),
        }
        Received::Accepted
    }
}

/// Seconds since the Unix epoch on this host's clock.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How far a signed timestamp (Unix seconds) is from `now`, either way;
/// `None` when it is no timestamp.
fn skew(timestamp: &str, now: u64) -> Option<u64> {
    timestamp
        .trim()
        .parse::<u64>()
        .ok()
        .map(|t| t.abs_diff(now))
}

/// The verifier for an application's public key (`verify_key`: 64 hex
/// digits).
///
/// # Errors
/// Anything that is not an Ed25519 public key in hex.
pub fn verifier(verify_key: &str) -> anyhow::Result<Verifier> {
    let digits = verify_key.trim().as_bytes();
    let mut key = [0u8; 32];
    let pairs = digits.chunks(2);
    anyhow::ensure!(
        digits.len() == key.len() * 2,
        "the application's public key is not 64 hex digits"
    );
    for (byte, pair) in key.iter_mut().zip(pairs) {
        let pair = std::str::from_utf8(pair)?;
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| anyhow::anyhow!("the application's public key is not hex"))?;
    }
    Verifier::try_new(key).map_err(|e| anyhow::anyhow!("the application's public key: {e}"))
}

/// What is wrong when `mode` meets the application's Interactions Endpoint
/// URL (`url`), as the operator should read it in `locale`; `None` when they
/// agree.
#[must_use]
pub fn mismatch(locale: Locale, mode: Delivery, url: Option<&str>) -> Option<String> {
    let found = match (mode, url.map(str::trim).filter(|u| !u.is_empty())) {
        (Delivery::Gateway, Some(url)) => alert::Delivery::GatewayButUrl { url },
        (Delivery::Http, None) => alert::Delivery::HttpNoUrl { path: PATH },
        (Delivery::Http, Some(url)) if !url.trim_end_matches('/').ends_with(PATH) => {
            alert::Delivery::WrongPath { url, path: PATH }
        }
        (Delivery::Gateway, None) | (Delivery::Http, Some(_)) => return None,
    };
    Some((alert::messages(locale).discord_delivery)(found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    /// This host's clock as a signed timestamp.
    fn now() -> String {
        unix_now().to_string()
    }

    fn sign(timestamp: &str, body: &[u8]) -> String {
        let message = [timestamp.as_bytes(), body].concat();
        hex(&key().sign(&message).to_bytes())
    }

    fn test_verifier() -> Option<Verifier> {
        Verifier::try_new(key().verifying_key().to_bytes()).ok()
    }

    fn command_body() -> Vec<u8> {
        json!({
            "id": "1", "application_id": "2", "type": 2, "token": "t", "version": 1,
            "channel_id": "3", "locale": "en-US", "entitlements": [],
            "authorizing_integration_owners": {}, "app_permissions": "0", "attachment_size_limit": 8_388_608,
            "user": {"id": "4", "username": "u", "discriminator": "0", "global_name": null, "avatar": null},
            "data": {"id": "5", "name": "card", "type": 1}
        })
        .to_string()
        .into_bytes()
    }

    /// An endpoint whose handler runs `then` after counting the call, and
    /// acknowledges when `acks`.
    fn open(
        acks: bool,
        then: Duration,
        shutdown: &Shutdown,
    ) -> Option<(Interactions, Arc<AtomicUsize>)> {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let run: Run = Arc::new(move |_, _, ack: Ack| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if acks {
                    ack.sent();
                }
                tokio::time::sleep(then).await;
            })
        });
        let endpoint = Interactions::new();
        endpoint.open_with(test_verifier()?, run, shutdown.clone());
        Some((endpoint, calls))
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_read_before_it_opens() {
        let body = command_body();
        let start = tokio::time::Instant::now();
        let got = Interactions::new()
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(got, Received::NotReady);
        assert_eq!(got.status(), 503);
        assert_eq!(
            start.elapsed(),
            OPEN_WAIT,
            "it waited for the endpoint to open"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_arrives_just_before_it_opens_is_served() {
        let shutdown = Shutdown::new(Duration::from_secs(1));
        let Some((donor, calls)) = open(true, Duration::ZERO, &shutdown) else {
            unreachable!("the test key is valid");
        };
        let Some(endpoint) = donor.0.endpoint.get() else {
            unreachable!("just opened");
        };
        let run = Arc::clone(&endpoint.run);
        let closed = Interactions::new();
        let opener = closed.clone();
        tokio::spawn(async move {
            tokio::time::sleep(OPEN_WAIT / 2).await;
            if let Some(v) = test_verifier() {
                opener.open_with(v, run, Shutdown::new(Duration::from_secs(1)));
            }
        });
        let body = command_body();
        let got = closed
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(got, Received::Accepted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unsigned_or_missigned_request_is_refused_unread() {
        let shutdown = Shutdown::new(Duration::from_secs(1));
        let Some((endpoint, calls)) = open(true, Duration::ZERO, &shutdown) else {
            unreachable!("the test key is valid");
        };
        let body = command_body();
        let ts = now();
        let good = sign(&ts, &body);
        let other = (unix_now() + 1).to_string();
        // Signed, but from outside the window: a replay.
        let old = (unix_now() - FRESHNESS.as_secs() - 2).to_string();
        let stale = sign(&old, &body);
        for (signature, timestamp, sent) in [
            (None, Some(ts.as_str()), &body[..]),
            (Some(good.as_str()), None, &body[..]),
            (Some("not hex"), Some(ts.as_str()), &body[..]),
            // The signature covers the timestamp and the body.
            (Some(good.as_str()), Some(other.as_str()), &body[..]),
            (Some(good.as_str()), Some(ts.as_str()), b"{}"),
            (Some(stale.as_str()), Some(old.as_str()), &body[..]),
        ] {
            let got = endpoint.receive(signature, timestamp, sent).await;
            assert_eq!(got, Received::Unauthorized);
            assert_eq!(got.status(), 401);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_ping_is_answered_in_the_body() {
        let shutdown = Shutdown::new(Duration::from_secs(1));
        let Some((endpoint, calls)) = open(true, Duration::ZERO, &shutdown) else {
            unreachable!("the test key is valid");
        };
        let body = json!({"id": "1", "application_id": "2", "type": 1, "token": "t", "version": 1})
            .to_string()
            .into_bytes();
        let got = endpoint
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(got, Received::Pong);
        assert_eq!((got.status(), got.body()), (200, Some(r#"{"type":1}"#)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let junk = b"not json";
        let got = endpoint
            .receive(Some(&sign(&now(), junk)), Some(&now()), junk)
            .await;
        assert_eq!(got, Received::Malformed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_is_answered_once_its_initial_response_is_sent() {
        let shutdown = Shutdown::new(Duration::from_mins(2));
        let Some((endpoint, calls)) = open(true, Duration::from_secs(40), &shutdown) else {
            unreachable!("the test key is valid");
        };
        let body = command_body();
        let start = tokio::time::Instant::now();
        let got = endpoint
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(
            (got, got.status(), got.body()),
            (Received::Accepted, 202, None)
        );
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "answered at the acknowledgement"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The handler goes on after the answer, and the drain waits for it.
        shutdown.request("test");
        let drained = tokio::time::timeout(Duration::from_secs(39), shutdown.drained()).await;
        assert!(drained.is_err(), "the drain waited for the handler");
        assert_eq!(shutdown.drained().await, crate::shutdown::Drained::Finished);
    }

    #[tokio::test(start_paused = true)]
    async fn a_handler_that_never_acknowledges_is_answered_at_the_wait() {
        let shutdown = Shutdown::new(Duration::from_mins(2));
        let Some((endpoint, _)) = open(false, Duration::from_secs(40), &shutdown) else {
            unreachable!("the test key is valid");
        };
        let body = command_body();
        let start = tokio::time::Instant::now();
        let got = endpoint
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(got, Received::Accepted);
        assert_eq!(start.elapsed(), ACK_WAIT);
        // One that ends without acknowledging is answered when it ends.
        let Some((quick, _)) = open(false, Duration::from_millis(5), &shutdown) else {
            unreachable!("the test key is valid");
        };
        let start = tokio::time::Instant::now();
        let got = quick
            .receive(Some(&sign(&now(), &body)), Some(&now()), &body)
            .await;
        assert_eq!(got, Received::Accepted);
        assert_eq!(start.elapsed(), Duration::from_millis(5));
    }

    #[test]
    fn the_public_key_is_read_from_hex() {
        let hex_key = hex(&key().verifying_key().to_bytes());
        assert!(verifier(&hex_key).is_ok());
        assert!(verifier(&format!(" {hex_key}\n")).is_ok());
        for bad in ["", "abc", &hex_key[..62], &format!("{}zz", &hex_key[..62])] {
            assert!(verifier(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_mode_is_checked_against_the_url() {
        let url = "https://judge.example.com/discord/interactions";
        assert_eq!(mismatch(Locale::En, Delivery::Gateway, None), None);
        assert_eq!(mismatch(Locale::En, Delivery::Gateway, Some("  ")), None);
        assert_eq!(mismatch(Locale::En, Delivery::Http, Some(url)), None);
        assert_eq!(
            mismatch(Locale::En, Delivery::Http, Some(&format!("{url}/"))),
            None
        );
        let wrong = [
            (Delivery::Gateway, Some(url), "DISCORD_INTERACTIONS=http"),
            (Delivery::Http, None, "no Interactions Endpoint URL"),
            (
                Delivery::Http,
                Some("https://judge.example.com/"),
                "does not end in /discord/interactions",
            ),
        ];
        for (mode, url, says) in wrong {
            let problem = mismatch(Locale::En, mode, url);
            assert!(
                problem.as_ref().is_some_and(|p| p.contains(says)),
                "{problem:?}"
            );
        }
    }
}
