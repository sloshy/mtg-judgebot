//! Everything the HTTP adapter reads from the environment. Which front doors
//! it opens comes from the command line instead ([`crate::interfaces`]);
//! [`ApiConfig::check`] is where the two have to agree.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Context as _;
use judge_llm::ApiKey;

use crate::interfaces::{Interface, Interfaces};

/// Configuration for the HTTP adapter.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// Listen address (`API_ADDR`).
    pub addr: SocketAddr,
    /// Directory holding the built web client (`WEB_DIST`).
    pub web_dist: PathBuf,
    /// Most `judge()` runs in flight at once (`JUDGE_CONCURRENCY`, shared
    /// default with the Discord adapter).
    pub max_concurrent: usize,
    /// Session Q&A pairs handed to `judge()` as history.
    pub history_len: usize,
    /// Questions allowed per IP per window (`API_RATE_LIMIT`).
    pub rate_limit: u32,
    /// The rate-limit window (`API_RATE_WINDOW_SECS`).
    pub rate_window: Duration,
    /// Where the rate-limit bucket key comes from (`API_CLIENT_IP`).
    pub client_ip: ClientIpSource,
    /// Bearer token gating the MCP transport at `/mcp` (`MCP_TOKEN`). The
    /// endpoint also needs [`Interface::Mcp`]; the token is the credential,
    /// not the switch. Redacted in `Debug`.
    pub mcp_token: Option<ApiKey>,
    /// Hostnames the MCP transport accepts in `Host` (`MCP_ALLOWED_HOSTS`,
    /// comma-separated); empty keeps rmcp's loopback-only default.
    pub mcp_hosts: Vec<String>,
    /// `judge` runs allowed through `/mcp` per window (`MCP_JUDGE_LIMIT`).
    pub mcp_judge_limit: u32,
    /// That window (`MCP_JUDGE_WINDOW_SECS`).
    pub mcp_judge_window: Duration,
}

/// Which address the per-IP rate limiter buckets on.
///
/// This is an enum rather than a "trust the proxy" flag because the obvious
/// flag encodes a state that is never safe: Cloudflare *appends* to a
/// client-supplied `X-Forwarded-For` rather than overwriting it, so the first
/// hop is attacker-chosen and any deployment trusting it hands every caller
/// its own rate-limit bucket — and `/api/judge` spends real money per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientIpSource {
    /// The socket peer address. Correct for direct connections, and the only
    /// safe default: an untrusted header can never influence it.
    PeerAddr,
    /// `CF-Connecting-IP`, which Cloudflare overwrites on every request and
    /// clients cannot forge. Correct only when Cloudflare is the sole ingress
    /// — as with a tunnel, where no other path to the origin exists.
    CloudflareConnectingIp,
}

impl ApiConfig {
    /// `API_ADDR` default.
    pub const DEFAULT_ADDR: &'static str = "0.0.0.0:8787";
    /// `WEB_DIST` default.
    pub const DEFAULT_WEB_DIST: &'static str = "web/dist";
    /// `JUDGE_CONCURRENCY` default (same as the Discord adapter).
    pub const DEFAULT_CONCURRENCY: usize = 2;
    /// Session history length.
    pub const DEFAULT_HISTORY: usize = 5;
    /// `API_RATE_LIMIT` default: questions per IP per window. Kept low because
    /// every question is real Anthropic spend (~$0.12 at gold-run prices).
    pub const DEFAULT_RATE_LIMIT: u32 = 4;
    /// `API_RATE_WINDOW_SECS` default.
    pub const DEFAULT_RATE_WINDOW: Duration = Duration::from_mins(5);
    /// Shortest `MCP_TOKEN` accepted, in bytes: the endpoint reaches paid
    /// tools, and a guessable token is worse than none.
    pub const MIN_MCP_TOKEN_BYTES: usize = 24;
    /// `MCP_JUDGE_LIMIT` default: `judge` runs per window through `/mcp`.
    /// The token is one identity, so this is the blast radius of a leak in
    /// pipeline runs (about $0.12 each), on top of `JUDGE_MAX_USD`.
    pub const DEFAULT_MCP_JUDGE_LIMIT: u32 = 20;
    /// `MCP_JUDGE_WINDOW_SECS` default.
    pub const DEFAULT_MCP_JUDGE_WINDOW: Duration = Duration::from_hours(1);

    /// Read the process environment. See [`Self::from_vars`].
    ///
    /// # Errors
    /// As [`Self::from_vars`].
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Build from a variable lookup. Blank values count as unset; every
    /// variable has a default, so an empty environment is valid.
    ///
    /// # Errors
    /// A malformed `API_ADDR`, `JUDGE_CONCURRENCY`, `API_RATE_LIMIT`,
    /// `API_RATE_WINDOW_SECS` or `API_CLIENT_IP`, a short `MCP_TOKEN`, or
    /// the presence of the removed `API_TRUST_FORWARDED`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let addr = var("API_ADDR")
            .unwrap_or_else(|| Self::DEFAULT_ADDR.to_owned())
            .parse::<SocketAddr>()
            .context("API_ADDR must be a socket address such as 0.0.0.0:8787")?;
        let web_dist =
            PathBuf::from(var("WEB_DIST").unwrap_or_else(|| Self::DEFAULT_WEB_DIST.to_owned()));
        let max_concurrent = parse_min(var("JUDGE_CONCURRENCY"), "JUDGE_CONCURRENCY", 1usize)?
            .unwrap_or(Self::DEFAULT_CONCURRENCY);
        let rate_limit = parse_min(var("API_RATE_LIMIT"), "API_RATE_LIMIT", 1u32)?
            .unwrap_or(Self::DEFAULT_RATE_LIMIT);
        let rate_window = parse_min(var("API_RATE_WINDOW_SECS"), "API_RATE_WINDOW_SECS", 1u64)?
            .map_or(Self::DEFAULT_RATE_WINDOW, Duration::from_secs);
        // Refuse to start rather than silently ignore the removed variable: a
        // deployment carrying API_TRUST_FORWARDED=true was trusting a header
        // clients control, and quietly falling back would hide that.
        anyhow::ensure!(
            var("API_TRUST_FORWARDED").is_none(),
            "API_TRUST_FORWARDED was removed because Cloudflare appends to \
             X-Forwarded-For rather than overwriting it, so its first hop is \
             client-controlled. Use API_CLIENT_IP=cloudflare (reads \
             CF-Connecting-IP) behind a Cloudflare tunnel, or drop the variable."
        );
        let client_ip = match var("API_CLIENT_IP").as_deref() {
            None | Some("peer") => ClientIpSource::PeerAddr,
            Some("cloudflare") => ClientIpSource::CloudflareConnectingIp,
            Some(v) => anyhow::bail!("API_CLIENT_IP must be peer or cloudflare, got {v:?}"),
        };
        let mcp_token = var("MCP_TOKEN");
        if let Some(t) = &mcp_token {
            anyhow::ensure!(
                t.len() >= Self::MIN_MCP_TOKEN_BYTES,
                "MCP_TOKEN must be at least {} bytes (try `openssl rand -base64 32`)",
                Self::MIN_MCP_TOKEN_BYTES
            );
            // It travels in an HTTP header: anything a client cannot send
            // would mount an endpoint that is 401 forever.
            anyhow::ensure!(
                t.bytes().all(|b| b.is_ascii_graphic()),
                "MCP_TOKEN must be printable ASCII without spaces (try `openssl rand -base64 32`)"
            );
        }
        let mcp_token = mcp_token.map(ApiKey::from);
        let mcp_judge_limit = parse_min(var("MCP_JUDGE_LIMIT"), "MCP_JUDGE_LIMIT", 1u32)?
            .unwrap_or(Self::DEFAULT_MCP_JUDGE_LIMIT);
        let mcp_judge_window =
            parse_min(var("MCP_JUDGE_WINDOW_SECS"), "MCP_JUDGE_WINDOW_SECS", 1u64)?
                .map_or(Self::DEFAULT_MCP_JUDGE_WINDOW, Duration::from_secs);
        let mcp_hosts = var("MCP_ALLOWED_HOSTS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            addr,
            web_dist,
            max_concurrent,
            history_len: Self::DEFAULT_HISTORY,
            rate_limit,
            rate_window,
            client_ip,
            mcp_token,
            mcp_hosts,
            mcp_judge_limit,
            mcp_judge_window,
        })
    }

    /// Refuse a launch the environment cannot satisfy, before anything binds
    /// a port.
    ///
    /// Only the interfaces that *cannot work* are refused — a door the
    /// operator named that has no credential or nothing to serve. The mirror
    /// cases (an `MCP_TOKEN` with no `--mcp`) are startup warnings in the
    /// binary instead: refusing there would take a working web page down over
    /// a variable that exposes nothing, which is not the bargain
    /// `API_TRUST_FORWARDED` struck — that variable had been *removed*, so any
    /// value meant a live unsafe configuration.
    ///
    /// # Errors
    /// `--mcp` without `MCP_TOKEN`, or `--web` pointed at a directory holding
    /// no `index.html`.
    pub fn check(&self, interfaces: &Interfaces) -> anyhow::Result<()> {
        anyhow::ensure!(
            !interfaces.mcp() || self.mcp_token.is_some(),
            "{} was given but MCP_TOKEN is not set. The MCP tools reach the \
             judge pipeline and the agent sessions, so there is no anonymous \
             mode: set MCP_TOKEN (`openssl rand -base64 32`, at least {} \
             characters) or drop {}.",
            Interface::Mcp.flag(),
            Self::MIN_MCP_TOKEN_BYTES,
            Interface::Mcp.flag()
        );
        // ServeDir is lazy, so without this a --web launch that cannot find
        // the build starts cleanly and 404s every page.
        if interfaces.web() {
            let index = self.web_dist.join("index.html");
            anyhow::ensure!(
                index.is_file(),
                "{} was given but {} does not exist. Build the page \
                 (`npm --prefix web run build`) or point WEB_DIST at the \
                 built directory.",
                Interface::Web.flag(),
                index.display()
            );
        }
        Ok(())
    }
}

/// Parse an optional integer variable, requiring at least `min`.
fn parse_min<T>(value: Option<String>, name: &str, min: T) -> anyhow::Result<Option<T>>
where
    T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
{
    value
        .map(|v| {
            v.parse::<T>()
                .ok()
                .filter(|n| *n >= min)
                .with_context(|| format!("{name} must be an integer >= {min}, got {v:?}"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonempty::nonempty;
    use std::collections::HashMap;

    fn serving(interfaces: &nonempty::NonEmpty<Interface>) -> Interfaces {
        Interfaces::of(interfaces)
    }

    fn cfg_with(pairs: &[(&str, &str)]) -> Option<ApiConfig> {
        ApiConfig::from_vars(vars(pairs)).ok()
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn empty_environment_gets_defaults() {
        let cfg = ApiConfig::from_vars(vars(&[])).ok();
        let cfg = cfg.as_ref();
        assert_eq!(
            cfg.map(|c| c.addr.to_string()),
            Some(ApiConfig::DEFAULT_ADDR.to_owned())
        );
        assert_eq!(
            cfg.map(|c| c.web_dist.clone()),
            Some(PathBuf::from("web/dist"))
        );
        assert_eq!(
            cfg.map(|c| c.max_concurrent),
            Some(ApiConfig::DEFAULT_CONCURRENCY)
        );
        assert_eq!(
            cfg.map(|c| c.rate_limit),
            Some(ApiConfig::DEFAULT_RATE_LIMIT)
        );
        assert_eq!(
            cfg.map(|c| c.rate_window),
            Some(ApiConfig::DEFAULT_RATE_WINDOW)
        );
        assert_eq!(cfg.map(|c| c.client_ip), Some(ClientIpSource::PeerAddr));
        assert_eq!(cfg.map(|c| c.mcp_token.is_none()), Some(true));
        assert_eq!(cfg.map(|c| c.mcp_hosts.clone()), Some(vec![]));
    }

    #[test]
    fn the_mcp_token_must_be_long_and_header_safe_and_hosts_are_a_list() {
        for bad in [
            "short",
            "0123456789abcdef0123456789 abcdef",
            "0123456789abcdef0123456789abcdé",
        ] {
            let r = ApiConfig::from_vars(vars(&[("MCP_TOKEN", bad)]));
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("MCP_TOKEN")),
                "{bad:?}: {r:?}"
            );
        }
        let cfg = ApiConfig::from_vars(vars(&[
            ("MCP_TOKEN", "0123456789abcdef0123456789abcdef"),
            ("MCP_ALLOWED_HOSTS", "judge.example.com, localhost,"),
        ]))
        .ok();
        assert_eq!(
            cfg.as_ref()
                .map(|c| c.mcp_token.as_ref().map(ApiKey::expose)),
            Some(Some("0123456789abcdef0123456789abcdef"))
        );
        assert!(
            cfg.as_ref()
                .is_some_and(|c| !format!("{c:?}").contains("0123456789abcdef")),
            "the token must not reach a Debug rendering of the config"
        );
        assert_eq!(
            cfg.as_ref().map(|c| c.mcp_hosts.clone()),
            Some(vec!["judge.example.com".to_owned(), "localhost".to_owned()])
        );
        assert_eq!(
            cfg.as_ref().map(|c| c.mcp_judge_limit),
            Some(ApiConfig::DEFAULT_MCP_JUDGE_LIMIT)
        );
        assert_eq!(
            cfg.map(|c| c.mcp_judge_window),
            Some(ApiConfig::DEFAULT_MCP_JUDGE_WINDOW)
        );
        let r = ApiConfig::from_vars(vars(&[("MCP_JUDGE_LIMIT", "0")]));
        assert!(
            r.as_ref()
                .is_err_and(|e| format!("{e:#}").contains("MCP_JUDGE_LIMIT")),
            "{r:?}"
        );
    }

    #[test]
    fn the_removed_trust_forwarded_variable_is_refused() {
        // Silently ignoring it would leave an operator believing a header they
        // no longer trust is still being honoured.
        for value in ["true", "false"] {
            let r = ApiConfig::from_vars(vars(&[("API_TRUST_FORWARDED", value)]));
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("API_CLIENT_IP")),
                "{value}: {r:?}"
            );
        }
    }

    #[test]
    fn explicit_values_parse_and_blank_counts_as_unset() {
        let cfg = ApiConfig::from_vars(vars(&[
            ("API_ADDR", "127.0.0.1:9000"),
            ("WEB_DIST", "/srv/web"),
            ("JUDGE_CONCURRENCY", "4"),
            ("API_RATE_LIMIT", "10"),
            ("API_RATE_WINDOW_SECS", "60"),
            ("API_CLIENT_IP", "cloudflare"),
        ]))
        .ok();
        let cfg = cfg.as_ref();
        assert_eq!(
            cfg.map(|c| c.addr.to_string()),
            Some("127.0.0.1:9000".to_owned())
        );
        assert_eq!(
            cfg.map(|c| c.web_dist.clone()),
            Some(PathBuf::from("/srv/web"))
        );
        assert_eq!(cfg.map(|c| c.max_concurrent), Some(4));
        assert_eq!(cfg.map(|c| c.rate_limit), Some(10));
        assert_eq!(cfg.map(|c| c.rate_window), Some(Duration::from_mins(1)));
        assert_eq!(
            cfg.map(|c| c.client_ip),
            Some(ClientIpSource::CloudflareConnectingIp)
        );

        let cfg = ApiConfig::from_vars(vars(&[("API_RATE_LIMIT", "  ")])).ok();
        assert_eq!(
            cfg.map(|c| c.rate_limit),
            Some(ApiConfig::DEFAULT_RATE_LIMIT)
        );
    }

    #[test]
    fn malformed_values_are_rejected_with_the_variable_name() {
        for bad in [
            ("API_ADDR", "not-an-addr"),
            ("JUDGE_CONCURRENCY", "0"),
            ("API_RATE_LIMIT", "-1"),
            ("API_RATE_WINDOW_SECS", "soon"),
            ("API_CLIENT_IP", "maybe"),
        ] {
            let r = ApiConfig::from_vars(vars(&[bad]));
            assert!(
                r.as_ref().is_err_and(|e| format!("{e:#}").contains(bad.0)),
                "{bad:?}: {r:?}"
            );
        }
    }

    const A_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// `--mcp` names a door that cannot open without a token, so it is
    /// refused. The mirror case — a token with no `--mcp` — is deliberately
    /// *not* an error: it serves nothing and exposes nothing, and refusing
    /// would take the web page down with it. The binary warns instead.
    #[test]
    fn the_mcp_flag_needs_a_token_but_a_token_alone_is_allowed() {
        let with_token = cfg_with(&[("MCP_TOKEN", A_TOKEN)]);
        let without = cfg_with(&[]);

        assert!(
            with_token
                .as_ref()
                .is_some_and(|c| c.check(&serving(&nonempty![Interface::Api])).is_ok()),
            "a token with no --mcp must start"
        );

        let r = without
            .as_ref()
            .map(|c| c.check(&serving(&nonempty![Interface::Mcp])));
        assert!(
            r.as_ref().is_some_and(|r| r
                .as_ref()
                .is_err_and(|e| format!("{e:#}").contains("MCP_TOKEN"))),
            "--mcp without MCP_TOKEN: {r:?}"
        );

        assert!(with_token.as_ref().is_some_and(|c| {
            c.check(&serving(&nonempty![Interface::Api, Interface::Mcp]))
                .is_ok()
        }));
        assert!(
            without
                .as_ref()
                .is_some_and(|c| c.check(&serving(&nonempty![Interface::Api])).is_ok())
        );
    }

    /// `ServeDir` is lazy, so without this check a `--web` launch pointed at
    /// nothing starts cleanly and 404s every page.
    #[test]
    fn web_without_a_built_page_is_refused_and_only_when_web_is_on() {
        let missing = cfg_with(&[("WEB_DIST", "definitely-not-a-directory")]);
        let r = missing
            .as_ref()
            .map(|c| c.check(&serving(&nonempty![Interface::Web])));
        assert!(
            r.as_ref().is_some_and(|r| r.as_ref().is_err_and(|e| {
                let text = format!("{e:#}");
                text.contains("--web") && text.contains("index.html")
            })),
            "{r:?}"
        );
        // The same configuration is fine when nobody asked for the page: the
        // image sets WEB_DIST unconditionally, so reading it either way would
        // make `judge-api` refuse to serve the API alone.
        assert!(
            missing
                .as_ref()
                .is_some_and(|c| c.check(&serving(&nonempty![Interface::Api])).is_ok())
        );

        let dist = std::env::temp_dir().join(format!("judge-api-cfg-{}", std::process::id()));
        let built = std::fs::create_dir_all(&dist)
            .and_then(|()| std::fs::write(dist.join("index.html"), "<!doctype html>"))
            .is_ok();
        assert!(built, "could not stage a web build in {}", dist.display());
        let cfg = cfg_with(&[("WEB_DIST", &dist.to_string_lossy())]);
        assert!(
            cfg.as_ref()
                .is_some_and(|c| c.check(&serving(&nonempty![Interface::Web])).is_ok())
        );
        let _ = std::fs::remove_dir_all(&dist);
    }
}
