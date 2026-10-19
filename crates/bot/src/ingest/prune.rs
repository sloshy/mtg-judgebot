//! Call retention: `JUDGE_CALL_RETENTION_DAYS`, the refresh's `prune` step
//! and `judgebot ingest prune`.
//!
//! A call older than the retention is deleted with its ratings (`ON DELETE
//! CASCADE`). Its `judge_runs` row stays, unlinked (`ON DELETE SET NULL`),
//! so spend and outcome figures survive it. A deleted call also leaves the
//! prior-call pool and the thread history. A newer call that cited it as a
//! prior call keeps its citation: the retirement pass treats a cited call
//! that no longer exists as supported, since its quote was checked when it
//! was stored (`db::retire`). `failed_calls` keeps its own bound (30 days,
//! 500 rows). Unset or `0` keeps every call, the default.

use std::num::NonZeroU16;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::RefreshLease;

/// `JUDGE_CALL_RETENTION_DAYS`.
pub const RETENTION_ENV: &str = "JUDGE_CALL_RETENTION_DAYS";
/// The longest retention the variable takes: ten years. A larger number is
/// far more likely a typo than a policy; unset keeps everything.
pub const MAX_RETENTION_DAYS: u16 = 3650;
/// Calls deleted per statement, so a first prune of a long history holds
/// its row locks briefly, a batch at a time.
const BATCH: i64 = 1_000;
/// Tries of one batch that Postgres aborts as a deadlock: `/forget` deletes
/// ratings, then updates calls, the opposite order to a batch's cascade.
const DEADLOCK_TRIES: u32 = 3;

/// A retention: a whole number of days, 1 to [`MAX_RETENTION_DAYS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Days(NonZeroU16);

impl Days {
    /// `days` if it is in range.
    #[must_use]
    pub const fn new(days: u16) -> Option<Self> {
        match NonZeroU16::new(days) {
            Some(d) if days <= MAX_RETENTION_DAYS => Some(Self(d)),
            _ => None,
        }
    }

    /// The number of days.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// How long calls are kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retention {
    /// Every call, however old: the default.
    #[default]
    Keep,
    /// Calls younger than this many days.
    Days(Days),
}

impl Retention {
    /// Parse `JUDGE_CALL_RETENTION_DAYS`: blank or `0` keeps every call.
    ///
    /// # Errors
    /// The value, when it is not a whole number from 0 to
    /// [`MAX_RETENTION_DAYS`].
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::Keep);
        };
        match v.parse::<u16>() {
            Ok(0) => Ok(Self::Keep),
            Ok(n) => Days::new(n).map(Self::Days).ok_or_else(|| v.to_owned()),
            Err(_) => Err(v.to_owned()),
        }
    }

    /// The retention this process's environment sets.
    ///
    /// # Errors
    /// A value [`Self::parse`] refuses, naming the variable.
    pub fn from_env() -> Result<Self> {
        Self::parse(std::env::var(RETENTION_ENV).ok().as_deref()).map_err(|value| {
            anyhow::anyhow!(
                "{RETENTION_ENV}={value:?} is not a whole number of days from 0 to {MAX_RETENTION_DAYS}"
            )
        })
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keep => f.write_str("kept"),
            Self::Days(d) => write!(f, "{} days", d.get()),
        }
    }
}

/// What a prune deleted. Stored in a refresh run's record (`ingest::runs`),
/// so its field names are a stored shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    /// The retention it applied, in days.
    pub days: u16,
    /// Calls deleted.
    pub calls: u64,
    /// Their ratings, deleted with them.
    pub ratings: u64,
}

/// Delete every call older than `days`, with its ratings, a batch at a
/// time, until a batch finds nothing to delete. A call locked by a writer
/// in progress (a rating, `/forget`) is skipped and goes with the next
/// refresh.
///
/// # Errors
/// A database failure; the batches before it stay deleted.
pub async fn run(lease: &mut RefreshLease, days: Days) -> Result<Pruned> {
    let mut pruned = Pruned {
        days: days.get(),
        ..Pruned::default()
    };
    loop {
        let (calls, ratings) = batch_retrying(lease, days).await?;
        pruned.calls += calls;
        pruned.ratings += ratings;
        if calls == 0 {
            break;
        }
    }
    tracing::info!(
        days = days.get(),
        calls = pruned.calls,
        ratings = pruned.ratings,
        "calls past the retention deleted"
    );
    Ok(pruned)
}

/// [`batch`], tried again when Postgres aborts it as a deadlock.
async fn batch_retrying(lease: &mut RefreshLease, days: Days) -> Result<(u64, u64)> {
    let mut tries = 1;
    loop {
        match batch(lease, days).await {
            Err(sqlx::Error::Database(e))
                if e.code().as_deref() == Some("40P01") && tries < DEADLOCK_TRIES =>
            {
                tracing::info!(
                    tries,
                    "prune batch deadlocked with another writer; trying again"
                );
                tries += 1;
            }
            other => return other.context("deleting calls past the retention"),
        }
    }
}

/// One batch: up to [`BATCH`] of the oldest calls past the retention, and
/// how many ratings went with them.
async fn batch(lease: &mut RefreshLease, days: Days) -> Result<(u64, u64), sqlx::Error> {
    // One snapshot for all three: the ratings counted are the ones the
    // cascade deletes.
    let row = sqlx::query!(
        r#"
            WITH doomed AS (
                SELECT id FROM calls
                WHERE created_at < now() - make_interval(days => $1)
                ORDER BY created_at
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            ),
            rated AS (
                SELECT count(*) AS n FROM ratings WHERE call_id IN (SELECT id FROM doomed)
            ),
            gone AS (
                DELETE FROM calls WHERE id IN (SELECT id FROM doomed) RETURNING 1
            )
            SELECT (SELECT count(*) FROM gone) AS "calls!", (SELECT n FROM rated) AS "ratings!"
            "#,
        i32::from(days.get()),
        BATCH,
    )
    .fetch_one(lease.pool())
    .await?;
    Ok((
        u64::try_from(row.calls).unwrap_or(0),
        u64::try_from(row.ratings).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_and_zero_keep_and_anything_out_of_range_is_refused() {
        assert_eq!(Retention::parse(None), Ok(Retention::Keep));
        assert_eq!(Retention::parse(Some(" ")), Ok(Retention::Keep));
        assert_eq!(Retention::parse(Some("0")), Ok(Retention::Keep));
        assert_eq!(
            Retention::parse(Some(" 90 ")).map(|r| r.to_string()),
            Ok("90 days".to_owned())
        );
        assert_eq!(
            Retention::parse(Some("3650")).map(|r| r.to_string()),
            Ok("3650 days".to_owned())
        );
        for bad in ["3651", "-1", "90d", "1.5", "65536"] {
            assert_eq!(Retention::parse(Some(bad)), Err(bad.to_owned()), "{bad}");
        }
    }
}
