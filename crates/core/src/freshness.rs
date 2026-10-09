//! What an instance's data is: the Comprehensive Rules release it holds and
//! how long ago its data was last refreshed. [`About`](crate::About) carries
//! it, so every interface that shows the source offer shows this beside it.
//!
//! This module is pure: the read is `judge_bot::ingest::runs::freshness`
//! (`max(rules.cr_version)` and the `refresh_runs` record, ages on the
//! database's clock), and each interface renders the facts its own way from
//! [`Freshness::lines`] or the fields.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::CrVersion;

/// The data an instance answers from, as of the moment it was read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Freshness {
    /// The Comprehensive Rules release loaded (`max(rules.cr_version)`);
    /// `None` when no rules are loaded.
    pub cr_version: Option<CrVersion>,
    /// Seconds since the latest refresh run that succeeded finished (`init`
    /// records one too); `None` when no run has succeeded, or none is
    /// recorded (including a schema without the run table).
    pub refreshed_secs_ago: Option<u64>,
    /// Whether the latest refresh run with a verdict failed (a run whose
    /// process died counts, once it is old enough). `false` when no run is
    /// recorded.
    pub last_refresh_failed: bool,
}

impl Freshness {
    /// The facts as short `label: value` lines, for a list: the CR release,
    /// the last successful refresh, and a failed latest run when there was
    /// one.
    ///
    /// ```text
    /// Comprehensive Rules: 2026-09-25
    /// Last successful refresh: 3 hours ago
    /// Latest refresh: failed
    /// ```
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let rules = self
            .cr_version
            .as_ref()
            .map_or_else(|| "none loaded".to_owned(), CrVersion::date);
        let refreshed = match (self.refreshed_secs_ago, self.last_refresh_failed) {
            (Some(secs), _) => ago(secs),
            (None, false) => "no refresh run recorded yet".to_owned(),
            (None, true) => "none yet".to_owned(),
        };
        let mut lines = vec![
            format!("Comprehensive Rules: {rules}"),
            format!("Last successful refresh: {refreshed}"),
        ];
        if self.last_refresh_failed {
            lines.push("Latest refresh: failed".to_owned());
        }
        lines
    }
}

/// An age as words: `under a minute ago`, `1 minute ago`, `5 hours ago`,
/// `3 days ago`. Minutes below an hour, hours below two days, days after.
#[must_use]
pub fn ago(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let (n, unit) = match secs {
        s if s < MINUTE => return "under a minute ago".to_owned(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < 2 * DAY => (s / HOUR, "hour"),
        s => (s / DAY, "day"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = anyhow::Result<()>;

    #[test]
    fn ages_read_in_the_largest_whole_unit_that_stays_precise() {
        for (secs, want) in [
            (0, "under a minute ago"),
            (59, "under a minute ago"),
            (60, "1 minute ago"),
            (3599, "59 minutes ago"),
            (3600, "1 hour ago"),
            (3 * 3600 + 59 * 60, "3 hours ago"),
            (47 * 3600, "47 hours ago"),
            (48 * 3600, "2 days ago"),
            (400 * 86_400, "400 days ago"),
        ] {
            assert_eq!(ago(secs), want, "{secs}");
        }
    }

    #[test]
    fn the_lines_name_every_fact_and_say_when_one_is_missing() -> R {
        let fresh = Freshness {
            cr_version: Some(CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(3 * 3600),
            last_refresh_failed: false,
        };
        assert_eq!(
            fresh.lines(),
            [
                "Comprehensive Rules: 2026-09-25",
                "Last successful refresh: 3 hours ago",
            ]
        );
        let failing = Freshness {
            last_refresh_failed: true,
            ..fresh
        };
        assert_eq!(
            failing.lines().last().map(String::as_str),
            Some("Latest refresh: failed")
        );
        assert_eq!(
            Freshness::default().lines(),
            [
                "Comprehensive Rules: none loaded",
                "Last successful refresh: no refresh run recorded yet",
            ]
        );
        let never_ok = Freshness {
            last_refresh_failed: true,
            ..Freshness::default()
        };
        assert_eq!(
            never_ok.lines(),
            [
                "Comprehensive Rules: none loaded",
                "Last successful refresh: none yet",
                "Latest refresh: failed",
            ]
        );
        Ok(())
    }

    /// `GET /api/about` serves these names; a client reads them.
    #[test]
    fn the_json_shape_is_pinned() -> R {
        let f = Freshness {
            cr_version: Some(CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(10_800),
            last_refresh_failed: true,
        };
        let json = serde_json::to_value(&f)?;
        assert_eq!(
            json,
            serde_json::json!({
                "cr_version": "20260925",
                "refreshed_secs_ago": 10_800,
                "last_refresh_failed": true,
            })
        );
        assert_eq!(serde_json::from_value::<Freshness>(json)?, f);
        assert_eq!(
            serde_json::to_value(Freshness::default())?,
            serde_json::json!({
                "cr_version": null,
                "refreshed_secs_ago": null,
                "last_refresh_failed": false,
            })
        );
        Ok(())
    }
}
