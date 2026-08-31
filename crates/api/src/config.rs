//! Everything the HTTP adapter reads from the environment.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Context as _;

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
    /// Trust the first `X-Forwarded-For` hop for rate limiting
    /// (`API_TRUST_FORWARDED`). Only set this behind a reverse proxy that
    /// overwrites the header; otherwise clients pick their own limit bucket.
    pub trust_forwarded: bool,
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
    /// `API_RATE_WINDOW_SECS` or `API_TRUST_FORWARDED`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |k: &str| get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let addr = var("API_ADDR")
            .unwrap_or_else(|| Self::DEFAULT_ADDR.to_owned())
            .parse::<SocketAddr>()
            .context("API_ADDR must be a socket address such as 0.0.0.0:8787")?;
        let web_dist = PathBuf::from(var("WEB_DIST").unwrap_or_else(|| Self::DEFAULT_WEB_DIST.to_owned()));
        let max_concurrent = parse_min(var("JUDGE_CONCURRENCY"), "JUDGE_CONCURRENCY", 1usize)?
            .unwrap_or(Self::DEFAULT_CONCURRENCY);
        let rate_limit =
            parse_min(var("API_RATE_LIMIT"), "API_RATE_LIMIT", 1u32)?.unwrap_or(Self::DEFAULT_RATE_LIMIT);
        let rate_window = parse_min(var("API_RATE_WINDOW_SECS"), "API_RATE_WINDOW_SECS", 1u64)?
            .map_or(Self::DEFAULT_RATE_WINDOW, Duration::from_secs);
        let trust_forwarded = match var("API_TRUST_FORWARDED").as_deref() {
            Some("true" | "1") => true,
            None | Some("false" | "0") => false,
            Some(v) => anyhow::bail!("API_TRUST_FORWARDED must be true/false, got {v:?}"),
        };
        Ok(Self {
            addr,
            web_dist,
            max_concurrent,
            history_len: Self::DEFAULT_HISTORY,
            rate_limit,
            rate_window,
            trust_forwarded,
        })
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
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn empty_environment_gets_defaults() {
        let cfg = ApiConfig::from_vars(vars(&[])).ok();
        let cfg = cfg.as_ref();
        assert_eq!(cfg.map(|c| c.addr.to_string()), Some(ApiConfig::DEFAULT_ADDR.to_owned()));
        assert_eq!(cfg.map(|c| c.web_dist.clone()), Some(PathBuf::from("web/dist")));
        assert_eq!(cfg.map(|c| c.max_concurrent), Some(ApiConfig::DEFAULT_CONCURRENCY));
        assert_eq!(cfg.map(|c| c.rate_limit), Some(ApiConfig::DEFAULT_RATE_LIMIT));
        assert_eq!(cfg.map(|c| c.rate_window), Some(ApiConfig::DEFAULT_RATE_WINDOW));
        assert_eq!(cfg.map(|c| c.trust_forwarded), Some(false));
    }

    #[test]
    fn explicit_values_parse_and_blank_counts_as_unset() {
        let cfg = ApiConfig::from_vars(vars(&[
            ("API_ADDR", "127.0.0.1:9000"),
            ("WEB_DIST", "/srv/web"),
            ("JUDGE_CONCURRENCY", "4"),
            ("API_RATE_LIMIT", "10"),
            ("API_RATE_WINDOW_SECS", "60"),
            ("API_TRUST_FORWARDED", "true"),
        ]))
        .ok();
        let cfg = cfg.as_ref();
        assert_eq!(cfg.map(|c| c.addr.to_string()), Some("127.0.0.1:9000".to_owned()));
        assert_eq!(cfg.map(|c| c.web_dist.clone()), Some(PathBuf::from("/srv/web")));
        assert_eq!(cfg.map(|c| c.max_concurrent), Some(4));
        assert_eq!(cfg.map(|c| c.rate_limit), Some(10));
        assert_eq!(cfg.map(|c| c.rate_window), Some(Duration::from_mins(1)));
        assert_eq!(cfg.map(|c| c.trust_forwarded), Some(true));

        let cfg = ApiConfig::from_vars(vars(&[("API_RATE_LIMIT", "  ")])).ok();
        assert_eq!(cfg.map(|c| c.rate_limit), Some(ApiConfig::DEFAULT_RATE_LIMIT));
    }

    #[test]
    fn malformed_values_are_rejected_with_the_variable_name() {
        for bad in [
            ("API_ADDR", "not-an-addr"),
            ("JUDGE_CONCURRENCY", "0"),
            ("API_RATE_LIMIT", "-1"),
            ("API_RATE_WINDOW_SECS", "soon"),
            ("API_TRUST_FORWARDED", "maybe"),
        ] {
            let r = ApiConfig::from_vars(vars(&[bad]));
            assert!(r.as_ref().is_err_and(|e| format!("{e:#}").contains(bad.0)), "{bad:?}: {r:?}");
        }
    }
}
