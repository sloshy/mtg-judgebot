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
//! The same task watches [`SpendMeter::refusals`] and tells the operator
//! (`JUDGE_ALERT_WEBHOOK`) the first time the cap refuses a request in a
//! period: a capped bot is otherwise silent until someone reads the log.
//!
//! The arithmetic is [`Ledger`], pure and tested without a database; the
//! period boundary is Postgres's clock, so processes cannot disagree on it.

use std::time::Duration;

use judge_llm::SpendMeter;
use sqlx::PgPool;

/// `JUDGE_BUDGET_PERIOD`.
pub const PERIOD_ENV: &str = "JUDGE_BUDGET_PERIOD";
/// `JUDGE_ALERT_WEBHOOK`.
pub const ALERT_WEBHOOK_ENV: &str = "JUDGE_ALERT_WEBHOOK";
/// How often the ledger is written and the period total read back.
pub const SYNC_EVERY: Duration = Duration::from_secs(10);
/// How long the alert webhook gets to answer.
pub const ALERT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Where the operator is told about a tripped cap: an `https` webhook that
/// takes a JSON body. Redacted in `Debug`, because a webhook URL is a
/// credential: anyone holding it can post to the channel.
#[derive(Clone, PartialEq, Eq)]
pub struct AlertWebhook(url::Url);

impl AlertWebhook {
    /// Parse the variable's value.
    ///
    /// # Errors
    /// Anything that is not an absolute `https` URL with a host.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match url::Url::parse(raw.trim()) {
            Ok(u) if u.scheme() == "https" && u.host_str().is_some() => Ok(Self(u)),
            _ => Err("an https:// webhook URL".to_owned()),
        }
    }

    /// The host, which is safe to log.
    #[must_use]
    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }
}

impl std::fmt::Debug for AlertWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AlertWebhook(https://{}/<redacted>)", self.host())
    }
}

/// The budget settings of one process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    /// What the cap covers.
    pub period: Period,
    /// Where a tripped cap is reported, if anywhere.
    pub alert: Option<AlertWebhook>,
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

/// The text sent to the webhook.
#[must_use]
pub fn alert_text(process: &str, period: Period, meter: &SpendMeter) -> String {
    let scope = match period {
        Period::Process => "this process (a restart counts from zero)".to_owned(),
        Period::Day => "today (UTC), across every process".to_owned(),
        Period::Month => "this month (UTC), across every process".to_owned(),
    };
    format!(
        "judgebot `{process}` is refusing questions at its spend cap: ${:.2} of ${:.2} spent for \
{scope}, and what is left is less than one call's worst case. Raise JUDGE_MAX_USD and restart to \
resume{}.",
        meter.counted_usd(),
        meter.max_spend_usd(),
        match period {
            Period::Process => "",
            Period::Day | Period::Month => ", or wait for the next period",
        }
    )
}

/// Post `text` to the webhook. The body carries both `content` (Discord) and
/// `text` (Slack and its imitators); each ignores the other's key.
async fn post_alert(client: &reqwest::Client, hook: &AlertWebhook, text: &str) {
    let body = serde_json::json!({ "content": text, "text": text });
    match client.post(hook.0.clone()).json(&body).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::info!(host = hook.host(), "spend cap alert sent");
        }
        Ok(r) => {
            tracing::warn!(host = hook.host(), status = %r.status(), "spend cap alert refused");
        }
        // `without_url`: the URL is the credential.
        Err(e) => {
            tracing::warn!(host = hook.host(), error = %e.without_url(), "spend cap alert failed");
        }
    }
}

/// Seed the meter from the ledger, then keep the two in step for the life of
/// the process. `process` names the caller in an alert (`bot`, `api`).
///
/// The first sync runs before this returns, so a process restarted into an
/// exhausted period refuses its first question rather than answering until
/// the first tick. A database error there is logged and retried on the tick:
/// the cap still holds per process, exactly as with no period configured.
pub async fn start(pool: PgPool, meter: SpendMeter, budget: Budget, process: &'static str) {
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
    // A webhook that accepts the connection and never answers must not stop
    // the syncing, which is what lifts the cap when the period turns.
    let client = reqwest::Client::builder()
        .timeout(ALERT_TIMEOUT)
        .build()
        .unwrap_or_default();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = sync(&pool, &meter, period, &mut ledger).await {
                tracing::warn!(error = %e, "spend ledger sync failed; will retry");
            }
            if ledger.should_alert(meter.refusals()) {
                tracing::warn!(
                    %period,
                    spent_usd = format_args!("{:.4}", meter.counted_usd()),
                    cap_usd = format_args!("{:.2}", meter.max_spend_usd()),
                    "spend cap reached; questions are being refused"
                );
                if let Some(hook) = &budget.alert {
                    post_alert(&client, hook, &alert_text(process, period, &meter)).await;
                }
            }
        }
    });
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
    fn the_webhook_must_be_https_and_never_prints_its_path() -> Result<(), String> {
        let hook = AlertWebhook::parse("https://discord.com/api/webhooks/1/secret-token")?;
        let shown = format!("{hook:?}");
        assert!(
            shown.contains("discord.com") && !shown.contains("secret"),
            "{shown}"
        );
        for bad in ["http://example.com/hook", "discord.com/api", "", "https://"] {
            assert!(AlertWebhook::parse(bad).is_err(), "{bad}");
        }
        Ok(())
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
        let text = alert_text("bot", Period::Month, &meter);
        assert!(
            text.contains("`bot`") && text.contains("$4.90 of $5.00 spent"),
            "{text}"
        );
        assert!(text.contains("next period"));
        assert!(!alert_text("api", Period::Process, &meter).contains("next period"));
        Ok(())
    }

    /// Two processes on one database: each sees the other's spend, a restart
    /// picks the period up where it was, and `Process` only keeps the record.
    #[sqlx::test(migrations = "./migrations")]
    async fn two_processes_share_a_period_through_the_ledger(pool: PgPool) -> anyhow::Result<()> {
        let (bot, api) = (
            SpendMeter::new().with_max_spend_usd(5.0)?,
            SpendMeter::new().with_max_spend_usd(5.0)?,
        );
        let (mut bot_ledger, mut api_ledger) = (Ledger::default(), Ledger::default());
        assert_eq!(
            sync(&pool, &bot, Period::Month, &mut bot_ledger).await?,
            Some(0)
        );
        assert_eq!(
            sync(&pool, &api, Period::Month, &mut api_ledger).await?,
            Some(0)
        );

        // The api process spends $3 (as an adjustment-free stand-in for real
        // calls, which need a backend: the ledger reads only `spent_micro`).
        spend(&api, 3_000_000);
        assert_eq!(
            sync(&pool, &api, Period::Month, &mut api_ledger).await?,
            Some(3_000_000)
        );
        assert!(
            (api.counted_usd() - 3.0).abs() < 1e-9,
            "its own spend is not counted twice"
        );
        // The bot learns of it on its next sync.
        sync(&pool, &bot, Period::Month, &mut bot_ledger).await?;
        assert!((bot.counted_usd() - 3.0).abs() < 1e-9);
        assert!(bot.spent_usd().abs() < 1e-9, "its own total is untouched");

        // A restarted bot starts from the period total, not from zero.
        let restarted = SpendMeter::new().with_max_spend_usd(5.0)?;
        sync(&pool, &restarted, Period::Month, &mut Ledger::default()).await?;
        assert!((restarted.counted_usd() - 3.0).abs() < 1e-9);

        // With no period the day is still recorded, and the meter is left alone.
        let lone = SpendMeter::new();
        spend(&lone, 250_000);
        assert_eq!(
            sync(&pool, &lone, Period::Process, &mut Ledger::default()).await?,
            None
        );
        assert!((lone.counted_usd() - 0.25).abs() < 1e-9);
        let (rows, micro): (i64, i64) =
            sqlx::query_as("SELECT count(*), COALESCE(sum(micro_usd), 0)::bigint FROM spend_days")
                .fetch_one(&pool)
                .await?;
        assert_eq!((rows, micro), (1, 3_250_000));
        Ok(())
    }

    /// Move a meter's process total the way a settled call does.
    fn spend(meter: &SpendMeter, micro: u64) {
        meter.record_micro(micro);
    }
}
