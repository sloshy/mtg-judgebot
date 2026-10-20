//! The alert webhook's text, one [`Messages`] per [`Locale`]
//! (`judge_core::locale`): a tripped spend cap and a spend warning
//! ([`crate::budget`]), the scheduled refresh ([`crate::jobs`]), the backup
//! ([`crate::backup`]) and the Discord role's delivery and clock checks. An
//! alert is written in the webhook's locale (`JUDGE_LOCALE`,
//! [`super::AlertWebhook::locale`]); a check that also logs its finding
//! logs the English text, since logs stay English.
//!
//! The texts name steps, counts and settings, never an error message:
//! those can carry URLs and keys, and the log has them.

use std::fmt;

use judge_core::{Locale, locale::Catalogs};

use crate::budget::Period;

/// What a spend alert reports.
#[derive(Clone, Copy, Debug)]
pub struct Spend<'a> {
    /// The process that tripped it (`judgebot`, `ingest`).
    pub process: &'a str,
    /// What the meter counts, in-flight calls at their worst case.
    pub counted_usd: f64,
    /// The cap, `JUDGE_MAX_USD`.
    pub max_usd: f64,
    /// What the cap covers.
    pub period: Period,
}

/// How a scheduled refresh failed.
#[derive(Clone, Copy, Debug)]
pub enum RefreshFailure<'a> {
    /// The run was dropped at the limit with no step failed yet, after
    /// this many minutes.
    Dropped {
        /// The limit, in minutes.
        after_mins: u64,
    },
    /// Steps failed, by name; when the run also timed out, after this many
    /// hours.
    Steps {
        /// The failed steps' names.
        steps: &'a [&'a str],
        /// The run's time limit in hours, when it timed out.
        timed_out_hours: Option<u64>,
    },
}

/// What a failed backup reports.
#[derive(Clone, Copy, Debug)]
pub struct BackupFailure<'a> {
    /// Whether it was the service's schedule, else `judgebot backup run`.
    pub scheduled: bool,
    /// The stage it failed at, as [`Messages::backup_stage`] says it.
    pub stage: &'a str,
    /// The backup's name, when it was uploaded before the failure.
    pub uploaded: Option<&'a str>,
    /// The newest backup in the bucket before it.
    pub before: LastGood<'a>,
}

/// The newest backup in the bucket, as a failure alert states it.
#[derive(Clone, Copy, Debug)]
pub enum LastGood<'a> {
    /// The newest: when it was taken, and how many days ago.
    Newest {
        /// Its time, `YYYY-MM-DD HH:MM UTC`.
        when: &'a str,
        /// Whole days since.
        days: i64,
    },
    /// The bucket held none.
    Empty,
    /// The bucket was not read.
    Unknown,
}

/// A backup stage, for [`Messages::backup_stage`].
#[derive(Clone, Copy, Debug)]
pub enum BackupStage<'a> {
    /// Listing the bucket.
    List,
    /// Dumping the database.
    Dump,
    /// `pg_dump` older than the server: the server's and the client's
    /// versions.
    Version {
        /// The server's version.
        server: &'a str,
        /// `pg_dump`'s.
        client: &'a str,
    },
    /// A dump under `BACKUP_MIN_BYTES`: its size and the minimum.
    TooSmall {
        /// The dump's size.
        bytes: u64,
        /// The minimum.
        min: u64,
    },
    /// Uploading the dump.
    Upload,
    /// Pruning old backups.
    Prune,
    /// Copying to `BACKUP_KEEP_LOCAL`.
    KeepLocal,
    /// The check crashed.
    Crashed,
}

/// Discord's delivery of interactions disagreeing with
/// `DISCORD_INTERACTIONS` (`discord::endpoint::mismatch`).
#[derive(Clone, Copy, Debug)]
pub enum Delivery<'a> {
    /// The gateway is configured, but Discord posts to this URL.
    GatewayButUrl {
        /// The application's Interactions Endpoint URL.
        url: &'a str,
    },
    /// `http` is configured, but the application has no URL: given the
    /// endpoint's path.
    HttpNoUrl {
        /// Where this process receives interactions.
        path: &'a str,
    },
    /// `http` is configured, but the URL does not end in the endpoint's
    /// path.
    WrongPath {
        /// The application's Interactions Endpoint URL.
        url: &'a str,
        /// Where this process receives interactions.
        path: &'a str,
    },
}

/// One locale's alert text.
#[derive(Clone, Copy)]
pub struct Messages {
    /// A spend warning: what was spent, and the percentage of the cap
    /// reached.
    pub spend_warning: fn(&Spend<'_>, u8) -> String,
    /// The spend cap tripped: questions are refused.
    pub spend_cap: fn(&Spend<'_>) -> String,
    /// A scheduled refresh that never finished, given the process running
    /// the next one and when the abandoned run started.
    pub refresh_abandoned: fn(&str, &str) -> String,
    /// A scheduled refresh succeeded after a failure, given the process.
    pub refresh_recovered: fn(&str) -> String,
    /// A scheduled refresh failed, given the process.
    pub refresh_failed: fn(&str, RefreshFailure<'_>) -> String,
    /// A scheduled refresh left rows unembedded over the ceiling: the
    /// process, the rows and the ceiling.
    pub embed_ceiling: fn(&str, u64, u64) -> String,
    /// The refresh check panicked, given the process.
    pub refresh_panicked: fn(&str) -> String,
    /// A backup succeeded after a failure: its name and size in MB.
    pub backup_recovered: fn(&str, u64) -> String,
    /// The bucket can be listed again, and no backup is due.
    pub backup_listed: &'static str,
    /// A backup failed.
    pub backup_failed: fn(&BackupFailure<'_>) -> String,
    /// What a backup was doing when it failed: a clause, `listing the
    /// bucket` (`judgebot backup run` prints it too).
    pub backup_stage: fn(BackupStage<'_>) -> String,
    /// The backup's settings are not usable, so it takes none: the
    /// variables at fault, by name (never a value).
    pub backup_unusable: fn(&[&str]) -> String,
    /// A signed Discord interaction refused for its timestamp: how far it
    /// was from this host's clock (`None`: unreadable), and the allowance,
    /// in seconds.
    pub discord_clock: fn(Option<u64>, u64) -> String,
    /// Discord's delivery disagrees with `DISCORD_INTERACTIONS`.
    pub discord_delivery: fn(Delivery<'_>) -> String,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("alert::catalog::Messages")
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

/// The alert text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    CATALOGS.get(locale)
}

/// The variable a [`Delivery`] alert names.
const INTERACTIONS_ENV: &str = crate::discord::Delivery::ENV;

/// English.
pub const EN: Messages = Messages {
    spend_warning: |s, reached| {
        format!(
            "judgebot `{}` has used {reached}% of its spend cap: ${:.2} of ${:.2} for {}, \
counting calls in flight at their worst case. Questions are refused once what is left is less \
than one call's worst case. The thresholds are {}.",
            s.process,
            s.counted_usd,
            s.max_usd,
            en_scope(s.period),
            crate::budget::WARN_AT_ENV,
        )
    },
    spend_cap: |s| {
        format!(
            "judgebot `{}` is refusing questions at its spend cap: ${:.2} of ${:.2} spent for \
{}, and what is left is less than one call's worst case. Raise JUDGE_MAX_USD and restart to \
resume{}.",
            s.process,
            s.counted_usd,
            s.max_usd,
            en_scope(s.period),
            match s.period {
                Period::Process => "",
                Period::Day | Period::Month => ", or wait for the next period",
            }
        )
    },
    refresh_abandoned: |process, started| {
        format!(
            "judgebot: a data refresh started at {started} never finished; its process may have \
             died (out of memory?) or lost the database. `{process}` is running the next one."
        )
    },
    refresh_recovered: |process| {
        format!(
            "judgebot `{process}`: a scheduled data refresh succeeded. The refresh before it had \
             failed."
        )
    },
    refresh_failed: |process, failure| {
        let timed_out = |hours: Option<u64>| {
            hours
                .map(|h| format!(", timing out after {h} h; the steps after it were not run"))
                .unwrap_or_default()
        };
        let what = match failure {
            RefreshFailure::Dropped { after_mins } => {
                format!("timed out: it was dropped after {after_mins} min")
            }
            RefreshFailure::Steps {
                steps: [one],
                timed_out_hours,
            } => format!("failed (step {one}){}", timed_out(timed_out_hours)),
            RefreshFailure::Steps {
                steps,
                timed_out_hours,
            } => format!(
                "failed (steps {}){}",
                steps.join(", "),
                timed_out(timed_out_hours)
            ),
        };
        format!(
            "judgebot `{process}`: a scheduled data refresh {what}. It is retried after an \
             hour, then less often while it keeps failing; the log has each error \
             (`refresh step failed`). You will be told again when it succeeds, not on each \
             retry."
        )
    },
    embed_ceiling: |process, rows, ceiling| {
        format!(
            "judgebot `{process}`: a scheduled data refresh did not embed {rows} rows, \
             more than the {ceiling} it embeds unattended. If that spend is expected (a \
             new embedder, an interrupted reembed), run `judgebot ingest embed` \
             (`docker compose run --rm refresh embed`); vector search misses those rows \
             until then."
        )
    },
    refresh_panicked: |process| {
        format!(
            "judgebot `{process}`: the scheduled data refresh check crashed (a panic; the log has \
             it). It is tried again at every check; you will not be told again until a refresh \
             succeeds."
        )
    },
    backup_recovered: |name, mb| {
        format!(
            "judgebot backup: a database backup succeeded ({name}, {mb} MB). The attempt before \
             it had failed."
        )
    },
    backup_listed: "judgebot backup: the bucket can be listed again. Its newest backup is recent, \
                    so none is due yet.",
    backup_failed: |f| {
        let last_good = match f.before {
            LastGood::Newest { when, days } => {
                format!(" The newest backup in the bucket is from {when} ({days} days ago).")
            }
            LastGood::Empty => " There is no earlier backup in the bucket.".to_owned(),
            LastGood::Unknown => String::new(),
        };
        let what = if f.scheduled {
            "the scheduled database backup"
        } else {
            "a database backup (`judgebot backup run`)"
        };
        let head = match f.uploaded {
            Some(name) => format!(
                "judgebot backup: {name} was uploaded, but {what} failed while {}.",
                f.stage
            ),
            None => format!(
                "judgebot backup: {what} failed while {}.{last_good}",
                f.stage
            ),
        };
        let tail = if f.scheduled {
            " It is retried after an hour, then less often while it keeps failing; \
             `docker compose logs backup` has the error. You will be told again when a backup \
             succeeds or fails at another step, not on each retry."
        } else {
            " The log has the error."
        };
        format!("{head}{tail}")
    },
    backup_stage: |stage| match stage {
        BackupStage::List => "listing the bucket".to_owned(),
        BackupStage::Dump => "dumping the database".to_owned(),
        BackupStage::Version { server, client } => format!(
            "dumping the database: pg_dump {client} is older than the server ({server}) and \
             refuses to dump it, so the image needs a newer PostgreSQL client. Retrying will \
             not help"
        ),
        BackupStage::TooSmall { bytes, min } => format!(
            "checking the dump: it was {bytes} bytes, under BACKUP_MIN_BYTES ({min}), so it \
             was not uploaded"
        ),
        BackupStage::Upload => "uploading the dump".to_owned(),
        BackupStage::Prune => "pruning old backups".to_owned(),
        BackupStage::KeepLocal => "copying it to BACKUP_KEEP_LOCAL".to_owned(),
        BackupStage::Crashed => "running the check (it crashed)".to_owned(),
    },
    backup_unusable: |vars| {
        format!(
            "judgebot backup: the backup settings are not usable, so no backup is taken until \
             they are fixed: {}. The log says what each must be (`docker compose logs \
             backup`). Fix .env.deploy; `docker compose up -d` then recreates the service with \
             it.",
            vars.join(", ")
        )
    },
    discord_clock: |skew, allowed| {
        format!(
            "Discord interaction refused: its signed timestamp is {} from this host's clock \
             (allowed: {allowed} s). One is a replay; every interaction refused means this host's \
             clock is wrong, and Discord's commands get no answer until it is fixed.",
            skew.map_or_else(|| "unreadable".to_owned(), |s| format!("{s} s")),
        )
    },
    discord_delivery: |d| match d {
        Delivery::GatewayButUrl { url } => format!(
            "Discord delivers this application's interactions to {url}, not over the gateway, \
             so none reach this process: set {INTERACTIONS_ENV}=http, or clear the Interactions \
             Endpoint URL in the Developer Portal"
        ),
        Delivery::HttpNoUrl { path } => format!(
            "{INTERACTIONS_ENV}=http, but the application has no Interactions Endpoint URL, so \
             Discord delivers its interactions over the gateway, which this process does not \
             open: save https://<your public host>{path} as the Interactions Endpoint URL in the \
             Developer Portal (General Information)"
        ),
        Delivery::WrongPath { url, path } => format!(
            "Discord delivers this application's interactions to {url}, which does not end in \
             {path}, where this process receives them"
        ),
    },
};

/// What a spend cap covers, as an English alert says it.
const fn en_scope(period: Period) -> &'static str {
    match period {
        Period::Process => "this process (a restart counts from zero)",
        Period::Day => "today (UTC), across every process",
        Period::Month => "this month (UTC), across every process",
    }
}

/// The stub locale's (`judge_core::locale`, tests only): each message its
/// name in `⟦…⟧`, with its values.
pub const STUB: Messages = Messages {
    spend_warning: |s, reached| {
        format!(
            "⟦spend_warning {} {reached} {:.2} {:.2} {}⟧",
            s.process, s.counted_usd, s.max_usd, s.period
        )
    },
    spend_cap: |s| {
        format!(
            "⟦spend_cap {} {:.2} {:.2} {}⟧",
            s.process, s.counted_usd, s.max_usd, s.period
        )
    },
    refresh_abandoned: |process, started| format!("⟦refresh_abandoned {process} {started}⟧"),
    refresh_recovered: |process| format!("⟦refresh_recovered {process}⟧"),
    refresh_failed: |process, failure| match failure {
        RefreshFailure::Dropped { after_mins } => {
            format!("⟦refresh_failed {process} {after_mins}⟧")
        }
        RefreshFailure::Steps {
            steps,
            timed_out_hours,
        } => format!(
            "⟦refresh_failed {process} {} {}⟧",
            steps.join(","),
            timed_out_hours.unwrap_or_default()
        ),
    },
    embed_ceiling: |process, rows, ceiling| format!("⟦embed_ceiling {process} {rows} {ceiling}⟧"),
    refresh_panicked: |process| format!("⟦refresh_panicked {process}⟧"),
    backup_recovered: |name, mb| format!("⟦backup_recovered {name} {mb}⟧"),
    backup_listed: "⟦backup_listed⟧",
    backup_failed: |f| {
        let before = match f.before {
            LastGood::Newest { when, days } => format!("{when} {days}"),
            LastGood::Empty => "empty".to_owned(),
            LastGood::Unknown => String::new(),
        };
        format!(
            "⟦backup_failed {} {} {} {before}⟧",
            f.scheduled,
            f.stage,
            f.uploaded.unwrap_or_default()
        )
    },
    backup_stage: |stage| format!("⟦backup_stage {stage:?}⟧"),
    backup_unusable: |vars| format!("⟦backup_unusable {}⟧", vars.join(",")),
    discord_clock: |skew, allowed| {
        format!("⟦discord_clock {} {allowed}⟧", skew.unwrap_or_default())
    },
    discord_delivery: |d| format!("⟦discord_delivery {d:?}⟧"),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every locale renders every message.
    #[test]
    fn every_locale_renders_every_message() {
        let spend = Spend {
            process: "judgebot",
            counted_usd: 4.1,
            max_usd: 5.0,
            period: Period::Month,
        };
        for l in Locale::ALL {
            let m = messages(l);
            let mut texts = vec![
                (m.spend_warning)(&spend, 80),
                (m.spend_cap)(&spend),
                (m.refresh_abandoned)("judgebot", "2026-10-20 04:00 UTC"),
                (m.refresh_recovered)("judgebot"),
                (m.refresh_failed)("judgebot", RefreshFailure::Dropped { after_mins: 190 }),
                (m.refresh_failed)(
                    "judgebot",
                    RefreshFailure::Steps {
                        steps: &["cards", "rules"],
                        timed_out_hours: Some(3),
                    },
                ),
                (m.embed_ceiling)("judgebot", 900, 800),
                (m.refresh_panicked)("judgebot"),
                (m.backup_recovered)("judgebot-x.dump.gz", 12),
                m.backup_listed.to_owned(),
                (m.backup_failed)(&BackupFailure {
                    scheduled: true,
                    stage: "x",
                    uploaded: None,
                    before: LastGood::Empty,
                }),
                (m.backup_unusable)(&["BACKUP_S3_ENDPOINT", "R2_BUCKET"]),
                (m.discord_clock)(None, 300),
                (m.discord_delivery)(Delivery::HttpNoUrl { path: "/p" }),
            ];
            for stage in [
                BackupStage::List,
                BackupStage::Dump,
                BackupStage::Version {
                    server: "17",
                    client: "16",
                },
                BackupStage::TooSmall { bytes: 1, min: 2 },
                BackupStage::Upload,
                BackupStage::Prune,
                BackupStage::KeepLocal,
                BackupStage::Crashed,
            ] {
                texts.push((m.backup_stage)(stage));
            }
            for t in &texts {
                assert!(!t.trim().is_empty(), "{l}: an empty alert");
            }
        }
    }
}
