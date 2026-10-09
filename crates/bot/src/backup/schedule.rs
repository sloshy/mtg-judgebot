//! When the service takes a backup, which old ones a run prunes, and what
//! the webhook is told: pure, so each rule is tested without a clock, a
//! database or a bucket.
//!
//! **The schedule is the bucket.** A backup is due when the newest object
//! named like one ([`BackupName`]) is at least `BACKUP_EVERY_DAYS` old, or
//! there is none. A restart does not reset it, and a backup the cron'd
//! script took counts the same as the service's own.
//!
//! **A failure backs off** in memory: an hour after the first, doubling with
//! each further one in a row, never more than [`MAX_BACKOFF`]. The bucket is
//! not listed again until then, so a store that refuses every request is
//! asked a few times a day, not every check. The webhook hears a streak's
//! first failure, a failure at another step, and the recovery ([`Streak`]).
//!
//! **Pruning keeps a floor**: past `BACKUP_KEEP_DAYS`, but never the
//! [`KEEP_NEWEST`] newest ([`to_prune`]).

use std::time::Duration;

use super::{
    name::{BackupName, Stamp},
    settings::Days,
};

/// The wait after one failure.
pub const RETRY_AFTER: Duration = Duration::from_hours(1);
/// The longest wait between attempts while they keep failing.
pub const MAX_BACKOFF: Duration = Duration::from_hours(24);
/// A backup stamped further than this ahead of the clock is not believed:
/// it cannot have been taken yet, and trusting it would put the schedule off
/// until then.
pub const FUTURE_SLACK_SECS: i64 = 3600;

/// Whether a backup should be taken now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Take one.
    Now,
    /// Not for this many more seconds.
    NotYet {
        /// Seconds until the newest backup is `every` old.
        in_secs: u64,
    },
}

/// When the next backup is due, at `now` (Unix seconds), given the backups
/// in the bucket. A stamp more than [`FUTURE_SLACK_SECS`] ahead of `now` is
/// ignored.
#[must_use]
pub fn due(now: i64, backups: &[BackupName], every: Days) -> Due {
    match newest(now, backups) {
        None => Due::Now,
        Some(n) => {
            let wait = n
                .stamp()
                .unix()
                .saturating_add(every.secs())
                .saturating_sub(now);
            match u64::try_from(wait) {
                Ok(in_secs) if in_secs > 0 => Due::NotYet { in_secs },
                _ => Due::Now,
            }
        }
    }
}

/// The newest believable backup at `now`.
#[must_use]
pub fn newest(now: i64, backups: &[BackupName]) -> Option<BackupName> {
    backups
        .iter()
        .copied()
        .filter(|b| b.stamp().unix() <= now.saturating_add(FUTURE_SLACK_SECS))
        .max()
}

/// The least time between two attempts after `failures` in a row.
#[must_use]
pub fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_AFTER.saturating_mul(1 << doublings).min(MAX_BACKOFF)
}

/// However old they are, a run never prunes the newest this many backups
/// (the one it just uploaded among them). After a lapse longer than
/// `BACKUP_KEEP_DAYS`, the first success would otherwise delete every
/// earlier backup, leaving one restore point, perhaps of a database that had
/// just been emptied or re-initialised. The script's `rclone delete
/// --min-age` has no such floor.
pub const KEEP_NEWEST: usize = 2;

/// The backups a run that just uploaded `kept` deletes: every one older than
/// `keep` days at `now`, except `kept` and the [`KEEP_NEWEST`] newest. Only
/// names of the backup shape are candidates, so nothing else under the
/// prefix is ever deleted.
#[must_use]
pub fn to_prune(now: i64, backups: &[BackupName], keep: Days, kept: BackupName) -> Vec<BackupName> {
    let mut all: Vec<BackupName> = backups.to_vec();
    all.push(kept);
    all.sort_unstable();
    all.dedup();
    let older = all.len().saturating_sub(KEEP_NEWEST);
    all.into_iter()
        .take(older)
        .filter(|b| *b != kept && now.saturating_sub(b.stamp().unix()) > keep.secs())
        .collect()
}

/// The step of a run that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Reading the bucket, before anything was dumped.
    List,
    /// `pg_dump`, for any reason but its version.
    Dump,
    /// `pg_dump` is older than the server.
    Version {
        /// The server's version.
        server: String,
        /// `pg_dump`'s.
        client: String,
    },
    /// The dump was under `BACKUP_MIN_BYTES`.
    TooSmall {
        /// Its size.
        bytes: u64,
        /// The floor.
        min: u64,
    },
    /// The upload.
    Upload,
    /// Pruning, after the upload succeeded.
    Prune,
    /// The local copy, after the upload succeeded.
    KeepLocal,
    /// The check itself crashed (a panic).
    Crashed,
}

impl Stage {
    /// Whether the dump reached the bucket before this failed.
    #[must_use]
    pub const fn uploaded(&self) -> bool {
        matches!(self, Self::Prune | Self::KeepLocal)
    }

    /// What was being done, as a clause after "failed while": the text an
    /// alert and the command line name it by.
    #[must_use]
    pub fn clause(&self) -> String {
        match self {
            Self::List => "listing the bucket".to_owned(),
            Self::Dump => "dumping the database".to_owned(),
            Self::Version { server, client } => format!(
                "dumping the database: pg_dump {client} is older than the server ({server}) and \
                 refuses to dump it, so the image needs a newer PostgreSQL client. Retrying will \
                 not help"
            ),
            Self::TooSmall { bytes, min } => format!(
                "checking the dump: it was {bytes} bytes, under BACKUP_MIN_BYTES ({min}), so it \
                 was not uploaded"
            ),
            Self::Upload => "uploading the dump".to_owned(),
            Self::Prune => "pruning old backups".to_owned(),
            Self::KeepLocal => "copying it to BACKUP_KEEP_LOCAL".to_owned(),
            Self::Crashed => "running the check (it crashed)".to_owned(),
        }
    }
}

/// How a backup attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// Dumped, uploaded, pruned and copied.
    Done {
        /// The backup taken.
        name: BackupName,
        /// Its size.
        bytes: u64,
    },
    /// It failed at `stage`. When the stage comes after the upload, `name`
    /// is the backup that reached the bucket.
    Failed {
        /// Where.
        stage: Stage,
        /// The backup uploaded before the failure, if one was.
        name: Option<BackupName>,
    },
}

impl Attempt {
    /// Whether everything succeeded.
    #[must_use]
    pub const fn ok(&self) -> bool {
        matches!(self, Self::Done { .. })
    }
}

/// What was known about the bucket before an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Before {
    /// It could not be listed.
    Unknown,
    /// It held no backup.
    Empty,
    /// Its newest backup was taken then.
    Newest(Stamp),
}

impl Before {
    /// From a listing's newest backup.
    #[must_use]
    pub fn of(newest: Option<BackupName>) -> Self {
        newest.map_or(Self::Empty, |n| Self::Newest(n.stamp()))
    }
}

/// What started a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The service's schedule.
    Schedule,
    /// `judgebot backup run`.
    Manual,
}

/// The kind of a [`Stage`], without its details: what a streak of
/// failures is keyed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageKind {
    /// [`Stage::List`].
    List,
    /// [`Stage::Dump`].
    Dump,
    /// [`Stage::Version`].
    Version,
    /// [`Stage::TooSmall`].
    TooSmall,
    /// [`Stage::Upload`].
    Upload,
    /// [`Stage::Prune`].
    Prune,
    /// [`Stage::KeepLocal`].
    KeepLocal,
    /// [`Stage::Crashed`].
    Crashed,
}

impl Stage {
    /// Its kind.
    #[must_use]
    pub const fn kind(&self) -> StageKind {
        match self {
            Self::List => StageKind::List,
            Self::Dump => StageKind::Dump,
            Self::Version { .. } => StageKind::Version,
            Self::TooSmall { .. } => StageKind::TooSmall,
            Self::Upload => StageKind::Upload,
            Self::Prune => StageKind::Prune,
            Self::KeepLocal => StageKind::KeepLocal,
            Self::Crashed => StageKind::Crashed,
        }
    }
}

/// The failures in a row the service has seen, and the stage the latest
/// failed at. The webhook hears a failure when it starts a streak or fails
/// at a different stage than the one before it: a pruning that keeps
/// failing after each upload must not hide a dump that starts failing, nor
/// an unreadable bucket one that cannot be uploaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streak {
    /// Failed attempts in a row (the backoff reads it).
    pub failures: u32,
    /// The stage the latest failed at; `None` when the last attempt succeeded.
    pub stage: Option<StageKind>,
}

impl Streak {
    /// The streak after a scheduled `attempt`, and what the webhook is told.
    #[must_use]
    pub fn attempted(self, attempt: &Attempt, before: Before, now: i64) -> (Self, Option<String>) {
        match attempt {
            Attempt::Done { name, bytes } => (
                Self::default(),
                (self.failures > 0).then(|| {
                    format!(
                        "judgebot backup: a database backup succeeded ({name}, {} MB). The \
                         attempt before it had failed.",
                        bytes / 1_000_000
                    )
                }),
            ),
            Attempt::Failed { stage, name } => {
                let kind = stage.kind();
                let next = Self {
                    failures: self.failures.saturating_add(1),
                    stage: Some(kind),
                };
                let news = self.stage != Some(kind);
                (
                    next,
                    news.then(|| failure_text(Trigger::Schedule, stage, *name, before, now)),
                )
            }
        }
    }

    /// The streak after a check that listed the bucket and found nothing
    /// due. That ends a streak of listing failures (the bucket is readable
    /// again, and the newest backup is recent), with a word to the webhook;
    /// any other streak waits for a backup to succeed.
    #[must_use]
    pub fn listed(self) -> (Self, Option<String>) {
        if self.stage == Some(StageKind::List) {
            (
                Self::default(),
                Some(
                    "judgebot backup: the bucket can be listed again. Its newest backup is \
                     recent, so none is due yet."
                        .to_owned(),
                ),
            )
        } else {
            (self, None)
        }
    }
}

/// What the webhook is told after a manual run (`judgebot backup run`,
/// perhaps from cron): every failure, as the script does, and no recovery,
/// because a manual run has no streak to read. The scheduled service goes
/// through [`Streak::attempted`].
#[must_use]
pub fn manual_alert(attempt: &Attempt, before: Before, now: i64) -> Option<String> {
    match attempt {
        Attempt::Done { .. } => None,
        Attempt::Failed { stage, name } => {
            Some(failure_text(Trigger::Manual, stage, *name, before, now))
        }
    }
}

/// A failure, for the webhook. It names the step and sizes, never an error
/// message: those can carry hosts and keys, and the log has them.
fn failure_text(
    trigger: Trigger,
    stage: &Stage,
    name: Option<BackupName>,
    before: Before,
    now: i64,
) -> String {
    let last_good = match before {
        Before::Newest(s) => format!(
            " The newest backup in the bucket is from {} ({} days ago).",
            s.human(),
            now.saturating_sub(s.unix()).max(0) / 86_400
        ),
        Before::Empty => " There is no earlier backup in the bucket.".to_owned(),
        Before::Unknown => String::new(),
    };
    let what = match trigger {
        Trigger::Schedule => "the scheduled database backup",
        Trigger::Manual => "a database backup (`judgebot backup run`)",
    };
    let head = match name {
        Some(name) if stage.uploaded() => format!(
            "judgebot backup: {name} was uploaded, but {what} failed while {}.",
            stage.clause()
        ),
        _ => format!(
            "judgebot backup: {what} failed while {}.{last_good}",
            stage.clause()
        ),
    };
    let tail = match trigger {
        Trigger::Schedule => {
            " It is retried after an hour, then less often while it keeps failing; \
             `docker compose logs backup` has the error. You will be told again when a backup \
             succeeds or fails at another step, not on each retry."
        }
        Trigger::Manual => " The log has the error.",
    };
    format!("{head}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    /// 2026-10-09T04:15:00Z.
    const NOW: i64 = 1_791_519_300;

    fn at(secs: i64) -> BackupName {
        BackupName::at(Stamp::from_unix(secs).unwrap_or_else(Stamp::now))
    }

    fn days(n: u16) -> Days {
        Days::new(n, 3650).unwrap_or(super::super::settings::DEFAULT_EVERY_DAYS)
    }

    #[test]
    fn a_backup_is_due_once_the_newest_is_old_enough() {
        let week = days(7);
        assert_eq!(due(NOW, &[], week), Due::Now);
        assert_eq!(
            due(NOW, &[at(NOW - 8 * DAY), at(NOW - DAY)], week),
            Due::NotYet {
                in_secs: u64::try_from(6 * DAY).unwrap_or_default()
            }
        );
        assert_eq!(due(NOW, &[at(NOW - 7 * DAY)], week), Due::Now);
        assert_eq!(due(NOW, &[at(NOW - 30 * DAY)], week), Due::Now);
    }

    #[test]
    fn a_stamp_from_the_future_does_not_hold_the_schedule_off() {
        let week = days(7);
        assert_eq!(due(NOW, &[at(NOW + 100 * DAY)], week), Due::Now);
        assert_eq!(
            due(NOW, &[at(NOW + 100 * DAY), at(NOW - 8 * DAY)], week),
            Due::Now
        );
        // Minutes of skew between the host that took it and this one are fine.
        assert!(matches!(
            due(NOW, &[at(NOW + 120)], week),
            Due::NotYet { .. }
        ));
    }

    #[test]
    fn failures_back_off_to_a_day() {
        let h = |n| Duration::from_hours(n);
        assert_eq!(backoff(0), h(1));
        assert_eq!(backoff(1), h(1));
        assert_eq!(backoff(2), h(2));
        assert_eq!(backoff(4), h(8));
        assert_eq!(backoff(6), h(24));
        assert_eq!(backoff(u32::MAX), h(24));
    }

    #[test]
    fn pruning_deletes_only_old_backups_and_never_the_new_one() {
        let kept = at(NOW);
        let backups = [
            at(NOW - 61 * DAY),
            at(NOW - 60 * DAY),
            at(NOW - 59 * DAY),
            at(NOW - 400 * DAY),
            kept,
            at(NOW + 10 * DAY),
        ];
        assert_eq!(
            to_prune(NOW, &backups, days(60), kept),
            [at(NOW - 400 * DAY), at(NOW - 61 * DAY)]
        );
        // Even when the clock says the new one is ancient.
        assert_eq!(to_prune(NOW + 1000 * DAY, &[kept], days(1), kept), []);
    }

    #[test]
    fn a_lapse_longer_than_the_retention_keeps_the_newest_backups() {
        // Nothing for 90 days, then one success: the two newest stay, the
        // one just taken and the newest before the lapse.
        let kept = at(NOW);
        let old = [at(NOW - 90 * DAY), at(NOW - 97 * DAY), at(NOW - 104 * DAY)];
        assert_eq!(
            to_prune(NOW, &old, days(60), kept),
            [at(NOW - 104 * DAY), at(NOW - 97 * DAY)]
        );
        // The listing may already hold the one just uploaded.
        let mut listed = old.to_vec();
        listed.push(kept);
        assert_eq!(
            to_prune(NOW, &listed, days(60), kept),
            to_prune(NOW, &old, days(60), kept)
        );
        assert_eq!(to_prune(NOW, &[at(NOW - 400 * DAY)], days(1), kept), []);
    }

    fn failed(stage: Stage) -> Attempt {
        Attempt::Failed { stage, name: None }
    }

    #[test]
    fn the_schedule_tells_the_first_failure_and_the_recovery() -> Result<(), &'static str> {
        let upload = failed(Stage::Upload);
        let before = Before::of(Some(at(NOW - 8 * DAY)));
        let (streak, first) = Streak::default().attempted(&upload, before, NOW);
        let first = first.ok_or("first failure")?;
        assert!(
            first.contains("uploading")
                && first.contains("8 days ago")
                && first.contains("retried"),
            "{first}"
        );
        assert_eq!(streak.failures, 1);
        let (streak, again) = streak.attempted(&upload, Before::Empty, NOW);
        assert_eq!((streak.failures, again), (2, None), "a retry is not news");
        let done = Attempt::Done {
            name: at(NOW),
            bytes: 31_000_000,
        };
        let (fresh, quiet) = Streak::default().attempted(&done, Before::Empty, NOW);
        assert_eq!((fresh, quiet), (Streak::default(), None));
        let (after, back) = streak.attempted(&done, Before::Unknown, NOW);
        let back = back.ok_or("recovery")?;
        assert!(
            back.contains("succeeded") && back.contains("31 MB"),
            "{back}"
        );
        assert_eq!(after, Streak::default());
        Ok(())
    }

    #[test]
    fn a_listing_that_works_again_ends_a_listing_streak_only() -> Result<(), &'static str> {
        let (listing, told) =
            Streak::default().attempted(&failed(Stage::List), Before::Unknown, NOW);
        assert!(told.is_some());
        let (cleared, note) = listing.listed();
        assert_eq!(cleared, Streak::default());
        assert!(
            note.ok_or("a word that it is readable")?
                .contains("listed again")
        );
        // So the next listing failure is news again.
        let (_, again) = cleared.attempted(&failed(Stage::List), Before::Unknown, NOW);
        assert!(again.is_some());
        // A failure after an upload is not ended by a listing.
        let prune = Attempt::Failed {
            stage: Stage::Prune,
            name: Some(at(NOW)),
        };
        let (pruning, _) = Streak::default().attempted(&prune, Before::Unknown, NOW);
        assert_eq!(pruning.listed(), (pruning, None));
        Ok(())
    }

    #[test]
    fn a_failure_at_another_step_is_told_even_mid_streak() {
        let prune = Attempt::Failed {
            stage: Stage::Prune,
            name: Some(at(NOW - 7 * DAY)),
        };
        let (streak, _) = Streak::default().attempted(&prune, Before::Unknown, NOW);
        // A week of pruning failures, then the dump starts failing: news.
        let (streak, told) = streak.attempted(&failed(Stage::Dump), Before::Unknown, NOW);
        assert!(told.is_some_and(|t| t.contains("dumping")));
        assert_eq!(streak.failures, 2);
        // And a version refusal after dump failures is news too.
        let version = failed(Stage::Version {
            server: "17.2".to_owned(),
            client: "16.15".to_owned(),
        });
        let (_, told) = streak.attempted(&version, Before::Unknown, NOW);
        assert!(told.is_some_and(|t| t.contains("17.2") && t.contains("newer PostgreSQL client")));
    }

    #[test]
    fn a_manual_run_tells_every_failure_and_no_recovery() {
        assert!(
            manual_alert(&failed(Stage::Dump), Before::Empty, NOW).is_some_and(|t| t
                .contains("judgebot backup run")
                && t.contains("no earlier backup"))
        );
        let done = Attempt::Done {
            name: at(NOW),
            bytes: 1,
        };
        assert_eq!(manual_alert(&done, Before::Empty, NOW), None);
    }

    #[test]
    fn a_failure_after_the_upload_says_the_backup_is_safe() {
        let name = at(NOW);
        let prune = Attempt::Failed {
            stage: Stage::Prune,
            name: Some(name),
        };
        let (_, t) = Streak::default().attempted(&prune, Before::Unknown, NOW);
        assert!(
            t.as_ref().is_some_and(
                |t| t.contains(&format!("{name} was uploaded")) && t.contains("pruning")
            ),
            "{t:?}"
        );
    }
}
