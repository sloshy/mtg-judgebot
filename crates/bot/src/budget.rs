//! The spend cap as a budget: `JUDGE_BUDGET_PERIOD=day|month` makes
//! `JUDGE_MAX_USD` a cap on the current UTC day or month, shared by every
//! long-running process on the database and kept across restarts. Unset
//! (`process`), the cap is what it always was: one process, its lifetime.
//!
//! The [`SpendMeter`] stays the enforcement point, in memory and atomic. This
//! module keeps the `spend_days` ledger beside it:
//!
//! * every [`SYNC_EVERY`] it adds what this process has spent since the last
//!   write to today's row, and reads the period's total back;
//! * from that it sets the meter's adjustment, so the cap sees "spent this
//!   period by anyone" rather than "spent by me since I started".
//!
//! So two processes can overshoot a shared cap by at most what they spend
//! between two syncs, and a crash loses at most that much of the record.
//! The ledger is written in every mode, since `judge-cli stats` reads it.
//!
//! The meter bills embeddings as well as chat calls (`judge_embed::MeteredEmbedder`
//! reserves on it), so the ledger's `micro_usd` is both, combined: a row does
//! not say which. Its `calls` are the chat calls only
//! ([`SpendMeter::calls`]), comparable with every row written before. The processes that write it are the long-running
//! roles (`judgebot`, under any roles, `--jobs` alone included, whose scheduled
//! refresh embeds) and `judgebot ingest`, whose `embed`, `reembed`, `init` and
//! `refresh` can spend more than any question. `ingest` writes its share once
//! more as it exits ([`Syncing::flush`]), since it may not live to the next
//! tick. `judge-cli`, `judge-mcp` and `judge-eval` keep a per-process cap and
//! write nothing, as before.
//!
//! The same task watches [`SpendMeter::refusals`] (chat requests only: a
//! refused embedding costs a question its vector search, not its answer, and
//! a refresh's refused embed step alerts as a failed step) and tells the operator
//! ([`crate::alert`], `JUDGE_ALERT_WEBHOOK`) the first time the cap refuses a request in a
//! period: a capped bot is otherwise silent until someone reads the log.
//!
//! It also warns before the cap is reached when `JUDGE_SPEND_WARN_AT` lists
//! percentages of the cap ([`Thresholds`]; unset, no warnings): once per
//! threshold per period, logged and posted to the same webhook. A sync that
//! crosses several reports the highest. In a `day` or `month` period every
//! process sees the same total, so the processes claim each warning in
//! `spend_warnings` first, keyed by the percentage and the cap it is of, and
//! only the one whose claim lands sends it. A cap raised mid-period, or a
//! process under another cap, warns at its own.
//!
//! Spend here counts in-flight reservations at their worst case, as the cap
//! does, so with several questions in flight a threshold can be reached a
//! little before the settled spend reaches it.
//!
//! The arithmetic is [`Ledger`], pure and tested without a database; the
//! period boundary is Postgres's clock, so processes cannot disagree on it.

use std::{sync::Arc, time::Duration};

use judge_core::Locale;
use judge_llm::SpendMeter;
use nonempty::NonEmpty;
use nutype::nutype;
use sqlx::PgPool;

use crate::alert::{self, AlertWebhook, catalog::Spend};

/// `JUDGE_BUDGET_PERIOD`.
pub const PERIOD_ENV: &str = "JUDGE_BUDGET_PERIOD";
/// `JUDGE_SPEND_WARN_AT`.
pub const WARN_AT_ENV: &str = "JUDGE_SPEND_WARN_AT";
/// How often the ledger is written and the period total read back.
pub const SYNC_EVERY: Duration = Duration::from_secs(10);

/// What `JUDGE_MAX_USD` caps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Period {
    /// One process, for its lifetime. Nothing is shared and a restart
    /// counts from zero.
    #[default]
    Process,
    /// The current UTC day, across every process on the database.
    Day,
    /// The current UTC month, across every process on the database.
    Month,
}

impl Period {
    /// Parse the variable's value; blank is the default.
    ///
    /// # Errors
    /// The value, when it is not `process`, `day` or `month`.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(Self::Process),
            Some(v) if v.eq_ignore_ascii_case("process") => Ok(Self::Process),
            Some(v) if v.eq_ignore_ascii_case("day") => Ok(Self::Day),
            Some(v) if v.eq_ignore_ascii_case("month") => Ok(Self::Month),
            Some(v) => Err(v.to_owned()),
        }
    }

    /// As the variable spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// The `date_trunc` field that starts the period, if there is one.
    const fn trunc(self) -> Option<&'static str> {
        match self {
            Self::Process => None,
            Self::Day => Some("day"),
            Self::Month => Some("month"),
        }
    }
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A spend warning threshold: a whole percentage of the cap, 1 to 99.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 99),
    derive(Clone, Copy, Debug, Display, PartialEq, Eq, PartialOrd, Ord)
)]
pub struct Percent(u8);

/// `JUDGE_SPEND_WARN_AT`: the percentages of the cap the operator is warned
/// at, ascending and distinct. Made only by [`Thresholds::parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thresholds(NonEmpty<Percent>);

impl Thresholds {
    /// Parse the variable's value: comma-separated whole percentages, each
    /// optionally followed by `%`, in any order. Blank is no warnings.
    ///
    /// # Errors
    /// The value, when a part is not a whole number from 1 to 99.
    pub fn parse(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let mut found = raw
            .split(',')
            .map(|part| {
                let digits = part.trim().strip_suffix('%').unwrap_or(part.trim());
                digits
                    .trim_end()
                    .parse::<u8>()
                    .ok()
                    .and_then(|n| Percent::try_new(n).ok())
                    .ok_or_else(|| raw.to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        found.sort_unstable();
        found.dedup();
        Ok(NonEmpty::from_vec(found).map(Self))
    }

    /// The highest threshold `counted` has reached under `cap`, both in
    /// micro-dollars. None under a zero cap, which every amount reaches.
    #[must_use]
    pub fn reached(&self, counted: u64, cap: u64) -> Option<Percent> {
        if cap == 0 {
            return None;
        }
        self.0
            .iter()
            .rev()
            .copied()
            .find(|&p| counted >= threshold_micro(cap, p))
    }
}

impl std::fmt::Display for Thresholds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{p}%")?;
        }
        Ok(())
    }
}

/// `p` percent of `cap`, in micro-dollars, rounded down.
#[must_use]
pub fn threshold_micro(cap: u64, p: Percent) -> u64 {
    let micro = u128::from(cap) * u128::from(p.into_inner()) / 100;
    u64::try_from(micro).unwrap_or(u64::MAX)
}

/// The budget settings of one process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    /// What the cap covers.
    pub period: Period,
    /// Where a tripped cap and a spend warning are reported, if anywhere.
    pub alert: Option<AlertWebhook>,
    /// Where the operator is warned before the cap, if anywhere.
    pub warn_at: Option<Thresholds>,
}

/// This process's side of the ledger. All amounts are micro-dollars.
///
/// `total` is the meter's process total, which only ever describes this
/// process and never moves back across a period. The ledger remembers how
/// much of it belongs to earlier periods (`baseline`) and how much of the
/// current period's share it has written (`flushed`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    period_start: Option<String>,
    baseline: u64,
    flushed: i64,
    calls_flushed: u64,
    refusals_seen: u64,
    alerted: bool,
    /// The highest threshold warned about (or claimed by another process)
    /// this period.
    warned: Option<Percent>,
}

/// What one sync writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    /// Added to today's `micro_usd` (negative when a reservation settled low).
    pub micro_usd: i64,
    /// Added to today's `calls`.
    pub calls: i64,
}

impl Ledger {
    /// The write for a sync at which the meter reads `total` / `calls` and the
    /// database says the period began at `period_start`, with the ledger as
    /// it will be once that write has landed.
    ///
    /// Nothing is committed here: the caller replaces its ledger with the
    /// returned one only after the row is written. A sync that fails between
    /// the two plans the same write again, so a database outage delays the
    /// record and never loses it.
    ///
    /// On a new period the unwritten remainder still goes to today's row: it
    /// was spent within one [`SYNC_EVERY`] of the boundary, and splitting it
    /// would need a clock this process does not trust.
    #[must_use]
    pub fn plan(&self, total: u64, calls: u64, period_start: &str) -> (Write, Self) {
        // Signed: a reservation written before a period turned can settle
        // lower after it, which leaves the total *below* the baseline until
        // new spend passes it. That correction has to reach the row too.
        let own = signed(total).saturating_sub(signed(self.baseline));
        let write = Write {
            micro_usd: own.saturating_sub(self.flushed),
            calls: signed(calls.saturating_sub(self.calls_flushed)),
        };
        let mut next = self.clone();
        next.calls_flushed = calls;
        if self.period_start.as_deref() == Some(period_start) {
            next.flushed = own;
        } else {
            // First sync, or the period rolled over: everything this process
            // has spent so far is now in the ledger and behind the baseline.
            next.period_start = Some(period_start.to_owned());
            next.baseline = total;
            next.flushed = 0;
            next.alerted = false;
            // Only a period that turned clears the warnings: a first sync
            // that lands after a database outage is the same period, and a
            // `process` budget may already have warned without one.
            if self.period_start.is_some() {
                next.warned = None;
            }
        }
        (write, next)
    }

    /// The meter adjustment once the period's rows sum to `period_total`
    /// (this process's writes included): `total + adjustment` is then the
    /// period total plus whatever this process has not written yet.
    #[must_use]
    pub fn adjustment(&self, period_total: i64) -> i64 {
        period_total
            .saturating_sub(self.flushed)
            .saturating_sub(signed(self.baseline))
    }

    /// The day the current period began, as the database names it
    /// (`process` for a [`Period::Process`] budget), once a sync has landed.
    #[must_use]
    pub fn period_start(&self) -> Option<&str> {
        self.period_start.as_deref()
    }

    /// The threshold to warn about now, if any: the highest one `counted`
    /// has reached under `cap`, when it is above any already warned about
    /// this period.
    #[must_use]
    pub fn warning_due(&self, thresholds: &Thresholds, counted: u64, cap: u64) -> Option<Percent> {
        let reached = thresholds.reached(counted, cap)?;
        (self.warned < Some(reached)).then_some(reached)
    }

    /// Record that `p` has been warned about this period, here or by
    /// another process.
    pub fn warned(&mut self, p: Percent) {
        self.warned = self.warned.max(Some(p));
    }

    /// Whether the operator should be told now: the cap has refused a request
    /// since the last look, and has not been reported this period.
    pub fn should_alert(&mut self, refusals: u64) -> bool {
        let new = refusals > self.refusals_seen;
        self.refusals_seen = refusals;
        if new && !self.alerted {
            self.alerted = true;
            return true;
        }
        false
    }
}

fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// One sync against the database: write this process's share, read the
/// period back, move the meter's adjustment. Returns the period total in
/// micro-dollars (`None` in [`Period::Process`], where nothing is summed).
///
/// # Errors
/// sqlx.
pub async fn sync(
    pool: &PgPool,
    meter: &SpendMeter,
    period: Period,
    ledger: &mut Ledger,
) -> Result<Option<i64>, sqlx::Error> {
    // The period's first day by the database's clock, in UTC. `Process` has
    // no boundary, so its "period" never changes.
    let start: String = match period.trunc() {
        Some(field) => {
            sqlx::query_scalar!(
                r#"SELECT date_trunc($1, now() AT TIME ZONE 'utc')::date::text AS "start!""#,
                field
            )
            .fetch_one(pool)
            .await?
        }
        None => "process".to_owned(),
    };
    let (w, next) = ledger.plan(meter.spent_micro(), meter.calls(), &start);
    if w.micro_usd != 0 || w.calls != 0 {
        sqlx::query!(
            r#"
            INSERT INTO spend_days (day, micro_usd, calls)
            VALUES ((now() AT TIME ZONE 'utc')::date, $1, $2)
            ON CONFLICT (day) DO UPDATE
               SET micro_usd = spend_days.micro_usd + EXCLUDED.micro_usd,
                   calls = spend_days.calls + EXCLUDED.calls
            "#,
            w.micro_usd,
            w.calls
        )
        .execute(pool)
        .await?;
    }
    // Only now is the write part of the ledger (see `Ledger::plan`).
    *ledger = next;
    // The settings beside the ledger, for the operator's report. Its own
    // failure (a schema that predates the table, during a major upgrade's
    // migration) costs the report its budget figures, never the cap.
    if let Err(e) = record_setting(pool, period, meter.max_spend_micro()).await {
        tracing::debug!(error = %e, "spend ledger: the budget settings were not recorded");
    }
    let Some(field) = period.trunc() else {
        return Ok(None);
    };
    let total: i64 = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(micro_usd), 0)::bigint AS "total!"
        FROM spend_days
        WHERE day >= date_trunc($1, now() AT TIME ZONE 'utc')::date
        "#,
        field
    )
    .fetch_one(pool)
    .await?;
    meter.set_adjustment_micro(ledger.adjustment(total));
    Ok(Some(total))
}

/// Record `period` and `cap` (micro-dollars) as the instance's budget
/// settings (`budget_setting`), which the operator's report reads
/// (`crate::db::stats::usage`), so `judge-cli stats` shows the instance's
/// cap rather than its own. Rewritten when they change, and every five
/// minutes so the row says it is current.
///
/// # Errors
/// sqlx.
pub async fn record_setting(pool: &PgPool, period: Period, cap: u64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO budget_setting (id, period, cap_micro) VALUES (true, $1, $2)
        ON CONFLICT (id) DO UPDATE
           SET period = EXCLUDED.period, cap_micro = EXCLUDED.cap_micro, written_at = now()
         WHERE budget_setting.period <> EXCLUDED.period
            OR budget_setting.cap_micro <> EXCLUDED.cap_micro
            OR budget_setting.written_at < now() - interval '5 minutes'
        "#,
        period.as_str(),
        i64::try_from(cap).unwrap_or(i64::MAX),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim the warning at `p` percent of `cap` micro-dollars for the period
/// that began on `period_start`: true when this process is the first to, and
/// so the one to send it. A claim lands before its warning is sent, so a webhook that
/// fails loses that warning rather than repeating it on every process.
///
/// # Errors
/// sqlx.
pub async fn claim_warning(
    pool: &PgPool,
    period: Period,
    period_start: &str,
    cap: u64,
    p: Percent,
) -> Result<bool, sqlx::Error> {
    let claimed = sqlx::query_scalar!(
        r#"
        INSERT INTO spend_warnings (period, period_start, cap_micro, percent)
        VALUES ($1, $2::text::date, $3, $4)
        ON CONFLICT DO NOTHING
        RETURNING true AS "claimed!"
        "#,
        period.as_str(),
        period_start,
        signed(cap),
        i16::from(p.into_inner())
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if claimed {
        // A row only has to outlive its period; a month is the longest.
        sqlx::query!(
            "DELETE FROM spend_warnings WHERE period_start < (now() AT TIME ZONE 'utc')::date - 62"
        )
        .execute(pool)
        .await?;
    }
    Ok(claimed)
}

/// What a spend alert reports about `meter`.
fn spend<'a>(process: &'a str, period: Period, meter: &SpendMeter) -> Spend<'a> {
    Spend {
        process,
        counted_usd: meter.counted_usd(),
        max_usd: meter.max_spend_usd(),
        period,
    }
}

/// The text a spend warning sends, in `locale`.
#[must_use]
pub fn warn_text(
    locale: Locale,
    process: &str,
    period: Period,
    meter: &SpendMeter,
    reached: Percent,
) -> String {
    (alert::catalog::messages(locale).spend_warning)(
        &spend(process, period, meter),
        reached.into_inner(),
    )
}

/// The text sent to the webhook when the cap trips, in `locale`.
#[must_use]
pub fn alert_text(locale: Locale, process: &str, period: Period, meter: &SpendMeter) -> String {
    (alert::catalog::messages(locale).spend_cap)(&spend(process, period, meter))
}

/// A process's ledger as [`start`] leaves it running: the handle a
/// short-lived process (`judgebot ingest`) writes its last spend through
/// before it exits.
#[derive(Clone)]
#[must_use = "a short-lived process flushes it before it exits; a long-running one may let it go"]
pub struct Syncing {
    pool: PgPool,
    meter: SpendMeter,
    period: Period,
    ledger: Arc<tokio::sync::Mutex<Ledger>>,
}

impl Syncing {
    /// Write what this process has spent since the last sync, now. A
    /// failure is logged: the spend was capped all the same, only its record
    /// is short.
    pub async fn flush(&self) {
        let mut ledger = self.ledger.lock().await;
        if let Err(e) = sync(&self.pool, &self.meter, self.period, &mut ledger).await {
            tracing::warn!(error = %e, "spend ledger: the last write failed; this run's spend is missing from `judge-cli stats`");
        }
    }
}

/// Seed the meter from the ledger, then keep the two in step for the life of
/// the process. `process` names the caller in an alert (`judgebot`).
///
/// The first sync runs before this returns, so a process restarted into an
/// exhausted period refuses its first question rather than answering until
/// the first tick. A database error there is logged and retried on the tick:
/// the cap still holds per process, exactly as with no period configured.
pub async fn start(
    pool: PgPool,
    meter: SpendMeter,
    budget: Budget,
    process: &'static str,
) -> Syncing {
    let mut ledger = Ledger::default();
    let period = budget.period;
    match sync(&pool, &meter, period, &mut ledger).await {
        Ok(Some(total)) => tracing::info!(
            %period,
            spent_usd = format_args!("{:.4}", micro_to_usd(total)),
            cap_usd = format_args!("{:.2}", meter.max_spend_usd()),
            "spend budget: period total loaded"
        ),
        Ok(None) => tracing::info!(%period, "spend cap covers this process's lifetime"),
        Err(e) => {
            tracing::error!(error = %e, "spend ledger unavailable; capping per process until it is");
        }
    }
    if let Some(at) = &budget.warn_at {
        tracing::info!(
            thresholds = %at,
            webhook = budget.alert.is_some(),
            "spend warnings set"
        );
    }
    let handle = Syncing {
        pool,
        meter,
        period,
        ledger: Arc::new(tokio::sync::Mutex::new(ledger)),
    };
    // A webhook that accepts the connection and never answers must not stop
    // the syncing, which is what lifts the cap when the period turns.
    let client = alert::client();
    let Syncing {
        pool,
        meter,
        ledger,
        ..
    } = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let (alert_now, warn_now) = {
                let mut ledger = ledger.lock().await;
                if let Err(e) = sync(&pool, &meter, period, &mut ledger).await {
                    tracing::warn!(error = %e, "spend ledger sync failed; will retry");
                }
                let warn = budget.warn_at.as_ref().and_then(|at| {
                    let p =
                        ledger.warning_due(at, meter.counted_micro(), meter.max_spend_micro())?;
                    Some((p, ledger.period_start().map(str::to_owned)))
                });
                (ledger.should_alert(meter.refusals()), warn)
            };
            if let Some((p, start)) = warn_now {
                warn(&pool, &client, &budget, process, &meter, &ledger, p, start).await;
            }
            if alert_now {
                tracing::warn!(
                    %period,
                    spent_usd = format_args!("{:.4}", meter.counted_usd()),
                    cap_usd = format_args!("{:.2}", meter.max_spend_usd()),
                    "spend cap reached; questions are being refused"
                );
                if let Some(hook) = &budget.alert {
                    let text = alert_text(hook.locale(), process, period, &meter);
                    alert::post(&client, hook, "spend cap alert", &text).await;
                }
            }
        }
    });
    handle
}

/// Send the warning at `p`, once it is claimed: a `process` budget's are
/// its own, a period's go through [`claim_warning`]. `start` is the period
/// the warning was found in; one that turned meanwhile keeps its own
/// warnings.
#[expect(
    clippy::too_many_arguments,
    reason = "the sync task's state, borrowed piecewise"
)]
async fn warn(
    pool: &PgPool,
    client: &reqwest::Client,
    budget: &Budget,
    process: &str,
    meter: &SpendMeter,
    ledger: &tokio::sync::Mutex<Ledger>,
    p: Percent,
    start: Option<String>,
) {
    let period = budget.period;
    let cap = meter.max_spend_micro();
    let claimed = match (period, start.as_deref()) {
        (Period::Process, _) => true,
        // No sync has landed yet, so the period is not known: next tick.
        (Period::Day | Period::Month, None) => return,
        (Period::Day | Period::Month, Some(day)) => {
            match claim_warning(pool, period, day, cap, p).await {
                Ok(claimed) => claimed,
                Err(e) => {
                    tracing::warn!(error = %e, "spend warning: claim failed; will retry");
                    return;
                }
            }
        }
    };
    if claimed {
        tracing::warn!(
            %period,
            threshold = %p,
            spent_usd = format_args!("{:.4}", meter.counted_usd()),
            cap_usd = format_args!("{:.2}", meter.max_spend_usd()),
            "spend warning: a threshold of the cap is reached"
        );
        if let Some(hook) = &budget.alert {
            let text = warn_text(hook.locale(), process, period, meter, p);
            alert::post(client, hook, "spend warning", &text).await;
        }
    }
    let mut ledger = ledger.lock().await;
    if period == Period::Process || ledger.period_start() == start.as_deref() {
        ledger.warned(p);
    }
}

fn micro_to_usd(micro: i64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "micro-dollar amounts stay far below 2^53"
    )]
    let usd = micro as f64 / 1_000_000.0;
    usd
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Ledger {
        /// Plan a write and commit it, as a sync whose INSERT succeeded does.
        fn write(&mut self, total: u64, calls: u64, period_start: &str) -> Write {
            let (w, next) = self.plan(total, calls, period_start);
            *self = next;
            w
        }
    }

    #[test]
    fn a_write_that_never_landed_is_planned_again_in_full() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-01");
        // The database is down: the plan is made and dropped, twice.
        let (lost, _) = l.plan(1_200_000, 3, "2026-09-01");
        let (again, _) = l.plan(1_500_000, 4, "2026-09-01");
        assert_eq!(lost.micro_usd, 1_200_000);
        assert_eq!(
            again,
            Write {
                micro_usd: 1_500_000,
                calls: 4
            },
            "nothing was lost"
        );
        // Across a rollover too: the baseline does not move until it lands.
        let (rolled, _) = l.plan(1_500_000, 4, "2026-10-01");
        assert_eq!(rolled.micro_usd, 1_500_000);
        assert_eq!(l.write(1_500_000, 4, "2026-10-01").micro_usd, 1_500_000);
        assert_eq!(l.write(1_500_000, 4, "2026-10-01").micro_usd, 0);
    }

    #[test]
    fn a_reservation_settling_after_the_period_turned_still_corrects_the_row() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-30");
        // $0.45 reserved and written on the 30th; the day turns; it settles at $0.10.
        l.write(450_000, 0, "2026-09-30");
        assert_eq!(l.write(450_000, 0, "2026-10-01").micro_usd, 0);
        let w = l.write(100_000, 1, "2026-10-01");
        assert_eq!(w.micro_usd, -350_000, "the over-reservation comes back out");
        // The next $0.35 of real spend is then seen as spend.
        assert_eq!(l.write(450_000, 2, "2026-10-01").micro_usd, 350_000);
    }

    #[test]
    fn the_period_parses_and_blank_is_the_process() {
        assert_eq!(Period::parse(None), Ok(Period::Process));
        assert_eq!(Period::parse(Some("  ")), Ok(Period::Process));
        assert_eq!(Period::parse(Some("Day")), Ok(Period::Day));
        assert_eq!(Period::parse(Some(" month ")), Ok(Period::Month));
        assert_eq!(Period::parse(Some("weekly")), Err("weekly".to_owned()));
    }

    #[test]
    fn a_process_writes_only_what_is_new_and_corrections_can_be_negative() {
        let mut l = Ledger::default();
        // First sync of a fresh process: nothing spent.
        assert_eq!(
            l.write(0, 0, "2026-09-01"),
            Write {
                micro_usd: 0,
                calls: 0
            }
        );
        // A call in flight has $0.50 reserved.
        assert_eq!(
            l.write(500_000, 0, "2026-09-01"),
            Write {
                micro_usd: 500_000,
                calls: 0
            }
        );
        // It settled at $0.10.
        assert_eq!(
            l.write(100_000, 1, "2026-09-01"),
            Write {
                micro_usd: -400_000,
                calls: 1
            }
        );
        assert_eq!(
            l.write(100_000, 1, "2026-09-01"),
            Write {
                micro_usd: 0,
                calls: 0
            }
        );
    }

    #[test]
    fn the_cap_sees_the_period_total_plus_what_is_not_written_yet() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-01");
        // Another process has spent $3 this period; this one nothing.
        assert_eq!(l.adjustment(3_000_000), 3_000_000);
        // This one spends $1 and writes it; the row total becomes $4.
        l.write(1_000_000, 4, "2026-09-01");
        let adj = l.adjustment(4_000_000);
        assert_eq!(1_000_000 + adj, 4_000_000);
        // $0.25 more, not yet written: counted at once, on top of the total.
        assert_eq!(1_250_000 + adj, 4_250_000);
    }

    #[test]
    fn a_restart_into_a_spent_period_starts_at_the_period_total() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-01");
        assert_eq!(l.adjustment(4_900_000), 4_900_000);
    }

    #[test]
    fn a_new_period_discounts_everything_this_process_spent_before_it() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-01");
        l.write(2_000_000, 8, "2026-09-01");
        // $0.30 more, then the month turns before the next sync.
        let w = l.write(2_300_000, 9, "2026-10-01");
        assert_eq!(
            w,
            Write {
                micro_usd: 300_000,
                calls: 1
            },
            "the remainder lands on today"
        );
        // October's rows hold just that remainder.
        let adj = l.adjustment(300_000);
        assert_eq!(2_300_000 + adj, 300_000);
        // Spend in the new period counts from there.
        let w = l.write(2_800_000, 10, "2026-10-01");
        assert_eq!(w.micro_usd, 500_000);
        assert_eq!(2_800_000 + l.adjustment(800_000), 800_000);
    }

    fn thresholds(raw: &str) -> Result<Thresholds, String> {
        Thresholds::parse(Some(raw))?.ok_or_else(|| "blank".to_owned())
    }

    #[test]
    fn thresholds_parse_sorted_and_distinct_and_refuse_anything_else() -> Result<(), String> {
        assert_eq!(Thresholds::parse(None)?, None);
        assert_eq!(Thresholds::parse(Some("  "))?, None);
        assert_eq!(thresholds("95, 50,80%,50")?.to_string(), "50%, 80%, 95%");
        assert_eq!(thresholds("80 %")?.to_string(), "80%");
        for bad in ["0", "100", "80,", "eighty", "50;80", "12.5", "-5", "256"] {
            assert_eq!(Thresholds::parse(Some(bad)), Err(bad.to_owned()), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn the_highest_threshold_reached_is_the_one_reported() -> Result<(), String> {
        let at = thresholds("50,80,95")?;
        let cap = 5_000_000;
        let reached = |counted| at.reached(counted, cap).map(Percent::into_inner);
        assert_eq!(reached(2_499_999), None);
        assert_eq!(reached(2_500_000), Some(50));
        assert_eq!(reached(4_100_000), Some(80));
        assert_eq!(reached(6_000_000), Some(95));
        assert_eq!(at.reached(1, 0), None, "a zero cap warns about nothing");
        // The amounts round down and cannot overflow.
        let p = Percent::try_new(99).map_err(|e| e.to_string())?;
        assert_eq!(
            u128::from(threshold_micro(u64::MAX, p)),
            u128::from(u64::MAX) * 99 / 100
        );
        assert_eq!(threshold_micro(5_000_001, p), 4_950_000);
        Ok(())
    }

    #[test]
    fn each_threshold_is_warned_once_per_period() -> Result<(), String> {
        let at = thresholds("50,80")?;
        let cap = 10_000_000;
        let mut l = Ledger::default();
        let due = |l: &mut Ledger, counted| {
            let p = l.warning_due(&at, counted, cap);
            if let Some(p) = p {
                l.warned(p);
            }
            p.map(Percent::into_inner)
        };
        l.write(0, 0, "2026-10-01");
        assert_eq!(due(&mut l, 4_000_000), None);
        assert_eq!(
            due(&mut l, 9_000_000),
            Some(80),
            "a jump past both reports the higher"
        );
        assert_eq!(due(&mut l, 9_500_000), None);
        assert_eq!(
            due(&mut l, 5_000_000),
            None,
            "a lower one is not reported after a higher"
        );
        l.write(0, 0, "2026-11-01");
        assert_eq!(due(&mut l, 6_000_000), Some(50), "a new period warns again");
        Ok(())
    }

    #[test]
    fn a_first_sync_after_an_outage_keeps_a_process_budgets_warning() -> Result<(), String> {
        let at = thresholds("80")?;
        let mut l = Ledger::default();
        let p = l.warning_due(&at, 9, 10).ok_or("due")?;
        l.warned(p);
        l.write(0, 0, "process");
        assert_eq!(l.warning_due(&at, 9, 10), None);
        Ok(())
    }

    #[test]
    fn the_warning_names_the_process_the_share_and_the_amounts() -> anyhow::Result<()> {
        let meter = SpendMeter::new().with_max_spend_usd(5.0)?;
        meter.set_adjustment_micro(4_100_000);
        let p = Percent::try_new(80)?;
        let text = warn_text(Locale::En, "judgebot", Period::Month, &meter, p);
        assert!(
            text.contains("`judgebot`")
                && text.contains("80% of its spend cap")
                && text.contains("$4.10 of $5.00 for this month"),
            "{text}"
        );
        assert!(text.contains(WARN_AT_ENV));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_warning_is_claimed_once_per_period_and_cap(pool: PgPool) -> anyhow::Result<()> {
        let (half, most) = (Percent::try_new(50)?, Percent::try_new(80)?);
        let claim = |period, day, cap, p| claim_warning(&pool, period, day, cap, p);
        assert!(claim(Period::Month, "2026-10-01", 5_000_000, most).await?);
        assert!(
            !claim(Period::Month, "2026-10-01", 5_000_000, most).await?,
            "the second process"
        );
        assert!(
            claim(Period::Month, "2026-10-01", 8_000_000, half).await?,
            "half of a raised cap is its own warning, though it is the same amount"
        );
        assert!(claim(Period::Month, "2026-11-01", 5_000_000, most).await?);
        assert!(
            claim(Period::Day, "2026-11-01", 5_000_000, most).await?,
            "a day and a month starting together are different periods"
        );
        Ok(())
    }

    #[test]
    fn the_operator_is_told_once_per_period() {
        let mut l = Ledger::default();
        l.write(0, 0, "2026-09-01");
        assert!(!l.should_alert(0));
        assert!(l.should_alert(1));
        assert!(!l.should_alert(1), "no new refusal");
        assert!(!l.should_alert(5), "already told this period");
        l.write(0, 0, "2026-10-01");
        assert!(!l.should_alert(5), "no refusal in the new period yet");
        assert!(l.should_alert(6));
    }

    #[test]
    fn the_alert_names_the_process_the_amounts_and_the_way_out() -> Result<(), judge_llm::LlmError>
    {
        let meter = SpendMeter::new().with_max_spend_usd(5.0)?;
        meter.set_adjustment_micro(4_900_000);
        let text = alert_text(Locale::En, "bot", Period::Month, &meter);
        assert!(
            text.contains("`bot`") && text.contains("$4.90 of $5.00 spent"),
            "{text}"
        );
        assert!(text.contains("next period"));
        assert!(!alert_text(Locale::En, "api", Period::Process, &meter).contains("next period"));
        Ok(())
    }

    /// What [`two_processes_share_a_period_through_the_ledger`] saw, checked
    /// once the run is known to have stayed in one UTC day.
    #[derive(Debug, Default)]
    struct Seen {
        first_syncs: (Option<i64>, Option<i64>),
        api_total: Option<i64>,
        api_counted: f64,
        bot_counted: f64,
        bot_spent: f64,
        restarted_counted: f64,
        lone_total: Option<i64>,
        lone_counted: f64,
        rows: i64,
        micro: i64,
    }

    /// The scenario: the api process spends, the bot learns of it, a
    /// restarted bot starts from the period total, and a `Process` meter
    /// only records its day.
    async fn share_a_period(pool: &PgPool) -> anyhow::Result<Seen> {
        let (bot, api) = (
            SpendMeter::new().with_max_spend_usd(5.0)?,
            SpendMeter::new().with_max_spend_usd(5.0)?,
        );
        let (mut bot_ledger, mut api_ledger) = (Ledger::default(), Ledger::default());
        let mut seen = Seen {
            first_syncs: (
                sync(pool, &bot, Period::Month, &mut bot_ledger).await?,
                sync(pool, &api, Period::Month, &mut api_ledger).await?,
            ),
            ..Seen::default()
        };
        // An adjustment-free stand-in for real calls, which need a backend:
        // the ledger reads only `spent_micro`.
        spend(&api, 3_000_000);
        seen.api_total = sync(pool, &api, Period::Month, &mut api_ledger).await?;
        seen.api_counted = api.counted_usd();
        sync(pool, &bot, Period::Month, &mut bot_ledger).await?;
        (seen.bot_counted, seen.bot_spent) = (bot.counted_usd(), bot.spent_usd());
        let restarted = SpendMeter::new().with_max_spend_usd(5.0)?;
        sync(pool, &restarted, Period::Month, &mut Ledger::default()).await?;
        seen.restarted_counted = restarted.counted_usd();
        let lone = SpendMeter::new();
        spend(&lone, 250_000);
        seen.lone_total = sync(pool, &lone, Period::Process, &mut Ledger::default()).await?;
        seen.lone_counted = lone.counted_usd();
        (seen.rows, seen.micro) =
            sqlx::query_as("SELECT count(*), COALESCE(sum(micro_usd), 0)::bigint FROM spend_days")
                .fetch_one(pool)
                .await?;
        Ok(seen)
    }

    /// Two processes on one database: each sees the other's spend, a restart
    /// picks the period up where it was, and `Process` only keeps the record.
    /// The ledger keys on the database's UTC day, so a run that crossed
    /// midnight (or a month's end) is run again on an empty ledger: it cannot
    /// cross twice.
    #[sqlx::test(migrations = "./migrations")]
    async fn two_processes_share_a_period_through_the_ledger(pool: PgPool) -> anyhow::Result<()> {
        let day = async || -> anyhow::Result<String> {
            Ok(
                sqlx::query_scalar("SELECT (now() AT TIME ZONE 'utc')::date::text")
                    .fetch_one(&pool)
                    .await?,
            )
        };
        let mut seen = None;
        for _ in 0..2 {
            sqlx::query("DELETE FROM spend_days").execute(&pool).await?;
            let before = day().await?;
            let run = share_a_period(&pool).await?;
            if day().await? == before {
                seen = Some(run);
                break;
            }
        }
        let seen = anyhow::Context::context(seen, "the run crossed midnight UTC twice")?;
        assert_eq!(seen.first_syncs, (Some(0), Some(0)));
        assert_eq!(seen.api_total, Some(3_000_000));
        assert!(
            (seen.api_counted - 3.0).abs() < 1e-9,
            "its own spend is not counted twice"
        );
        // The bot learns of it on its next sync.
        assert!((seen.bot_counted - 3.0).abs() < 1e-9);
        assert!(seen.bot_spent.abs() < 1e-9, "its own total is untouched");
        // A restarted bot starts from the period total, not from zero.
        assert!((seen.restarted_counted - 3.0).abs() < 1e-9);
        // With no period the day is still recorded, and the meter is left alone.
        assert_eq!(seen.lone_total, None);
        assert!((seen.lone_counted - 0.25).abs() < 1e-9);
        assert_eq!((seen.rows, seen.micro), (1, 3_250_000));
        Ok(())
    }

    /// Move a meter's process total the way a settled call does.
    fn spend(meter: &SpendMeter, micro: u64) {
        meter.record_micro(micro);
    }
}
