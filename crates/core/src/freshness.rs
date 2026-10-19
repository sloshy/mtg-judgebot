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

use crate::{CrVersion, Locale};

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
    pub fn lines(&self, locale: Locale) -> Vec<String> {
        let m = locale.messages();
        let date = self.cr_version.as_ref().map(CrVersion::date);
        let mut lines = vec![
            (m.rules_line)(date.as_deref()),
            (m.refreshed_line)(self.refreshed_secs_ago, self.last_refresh_failed),
        ];
        if self.last_refresh_failed {
            lines.push(m.refresh_failed.to_owned());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = anyhow::Result<()>;

    #[test]
    fn the_lines_name_every_fact_and_say_when_one_is_missing() -> R {
        let fresh = Freshness {
            cr_version: Some(CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(3 * 3600),
            last_refresh_failed: false,
        };
        assert_eq!(
            fresh.lines(Locale::En),
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
            failing.lines(Locale::En).last().map(String::as_str),
            Some("Latest refresh: failed")
        );
        assert_eq!(
            Freshness::default().lines(Locale::En),
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
            never_ok.lines(Locale::En),
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
