//! Advisory leases: one process at a time does a thing, across every process
//! on the database.
//!
//! A [`Lease<K>`] holds the session-level advisory lock of `K` on a
//! connection of its own for as long as it lives. `K` ([`LeaseKey`], sealed)
//! is what the lease guards, so a [`RefreshLease`] cannot be handed to the
//! Discord gateway and a [`GatewayLease`] cannot admit an ingest step. Each
//! key carries its lock, its `application_name` label and its wait policy:
//!
//! * [`Refresh`] ([`RefreshLease`]): one data-writing ingest run at a time.
//!   Every step that writes data takes `&mut RefreshLease` and reads its pool
//!   from it ([`Lease::pool`]), so a step run without the lease does not
//!   compile, two steps cannot run at once under one lease (a `join!` of two
//!   needs two mutable borrows), and the lock is always on the database the
//!   step writes. Waiting is bounded ([`RefreshLease::acquire`]).
//! * [`Gateway`] ([`GatewayLease`]): one process connected to the Discord
//!   gateway on the bot's token ([`crate::discord::gateway`]). Waiting is a
//!   standby that lasts as long as the holder ([`GatewayLease::stand_by`]).
//!
//! ## The advisory keys
//!
//! The database's three advisory lock keys, all arbitrary, distinct (checked
//! at compile time below) and the same in every process:
//!
//! | Key | Scope | Held by | For |
//! |---|---|---|---|
//! | [`REFRESH_LOCK`] | session | a data-writing run, for minutes | [`RefreshLease`] |
//! | [`GATEWAY_LOCK`] | session | the gateway holder, for its life | [`GatewayLease`] |
//! | [`CALLS_REWRITE_LOCK`](crate::db::CALLS_REWRITE_LOCK) | transaction, shared or exclusive; session for a migration | a vector write (shared); the CR load, the retirement pass, a space switch (exclusive); a migration, for its whole run | the calls rewrite and the vector space |
//!
//! They nest in one order only. A refresh run holds the lease and its steps
//! take the calls lock; nothing takes the refresh lease while holding the
//! calls lock (`init` releases the migration's before taking it). The gateway
//! lease is held on a connection that takes nothing else, and a process
//! waiting for it holds nothing else on that connection. So no two of them
//! can deadlock. A migration does not wait for a refresh, only for the step
//! inside it that holds the calls lock, and never for the gateway.
//!
//! **Release.** [`Lease::release`] unlocks and closes the connection. The
//! connection is detached from the pool once it is taken (so it does not count
//! against the pool's size, and the pool keeps every connection it had), and
//! it never goes back to the pool: a lease dropped without `release` (an early
//! return, a panic, a cancelled future, a process that exits) drops the
//! socket, and Postgres releases a session lock when its session ends. A
//! crashed process therefore leaves nothing behind to clean up.
//!
//! **Session settings.** Every lease session, holding or waiting, gets
//! [`SESSION_SETTINGS`]: TCP keepalives that end a vanished client's session
//! (and free its lock) in about 25 s instead of the kernel's two hours, and
//! `statement_timeout` off, so the waits are bounded only by their own
//! `lock_timeout` and the client. Every bookkeeping query is bounded by
//! [`QUICK`] on the client side. A lease needs a direct connection to
//! Postgres: a pooler in transaction mode (`PgBouncer`) would hand the
//! session, and its lock, to whichever client runs next.
//!
//! **Visibility.** The holder is findable: its session's `application_name`
//! is `<label> (<process>) since <UTC minute>` (`… waiting` while queued,
//! when `query_start` is when the wait began), visible in
//! `pg_stat_activity`, where the label is `judgebot refresh lease` or
//! `judgebot gateway`.
//!
//! **A lost lease.** If the lease's session dies (the server restarted, an
//! operator ended it, the network dropped it), another process may take the
//! lock while this one still acts on it. [`Lease::check`] asks the session
//! whether it still holds the lock; a refresh run calls it before each step
//! and the gateway holder every few seconds, and each stops when it fails.

use std::{
    marker::PhantomData,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use sqlx::{Connection as _, PgConnection, PgPool};

/// Advisory lock key for the refresh lease.
pub const REFRESH_LOCK: i64 = 0x6a75_6467_6572_6566; // "judgeref"

/// Advisory lock key for the Discord gateway lease.
pub const GATEWAY_LOCK: i64 = 0x6a75_6467_6567_7779; // "judgegwy"

const _: () = assert!(
    REFRESH_LOCK != GATEWAY_LOCK
        && REFRESH_LOCK != crate::db::CALLS_REWRITE_LOCK
        && GATEWAY_LOCK != crate::db::CALLS_REWRITE_LOCK
);

/// How long [`RefreshLease::acquire`] waits for another run. Longer than any
/// healthy run (a daily refresh takes minutes, a first `init` on a NAS well
/// under an hour) and far shorter than a day, so a hung run fails the next
/// command with the holder named instead of blocking it forever. The schedule
/// ([`crate::jobs`]) never waits: it uses [`Lease::try_acquire`] and checks
/// again later.
pub const LEASE_WAIT: Duration = Duration::from_hours(1);

/// How long one round of a gateway standby waits on its connection before
/// asking again ([`GatewayLease::stand_by`]). The server ends the round
/// (`lock_timeout`), so a healthy standby re-asks on the same session; a
/// round with no answer at all (a connection that died without a word) is
/// abandoned [`STANDBY_SLACK`] later and the standby reconnects.
pub const STANDBY_ROUND: Duration = Duration::from_mins(1);

/// How long past [`STANDBY_ROUND`] a standby waits for the server to end
/// the round before it counts the connection as dead.
pub const STANDBY_SLACK: Duration = Duration::from_secs(10);

/// The first pause before a gateway standby reconnects after its connection
/// failed. It doubles per failure in a row up to [`STANDBY_RETRY_MAX`].
pub const STANDBY_RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest pause between a gateway standby's reconnection attempts.
pub const STANDBY_RETRY_MAX: Duration = Duration::from_secs(30);

/// How long a lease's bookkeeping query (hardening, labelling, naming the
/// holder, setting `lock_timeout`, a standby's first try) may go unanswered
/// before the connection counts as dead. The lock waits themselves are
/// bounded separately.
pub const QUICK: Duration = Duration::from_secs(10);

/// The server-side settings every lease session carries ([`harden`]):
///
/// * TCP keepalives after 10 s idle, every 5 s, 3 unanswered: a client that
///   vanished without closing its socket (power loss, a partition, a container
///   network torn down) is noticed in about 25 s, and its session ends and its
///   lock is freed. The server's defaults (`0`, the kernel's: two hours idle)
///   would hold a dead holder's lock, or grant it to a dead standby, for hours.
/// * `tcp_user_timeout` 15 s: data the server sent that stays unacknowledged
///   that long ends the session too.
/// * `statement_timeout` 0: a lease session's waits are bounded by
///   `lock_timeout` and by the client, never by an operator's
///   `statement_timeout` (in `DATABASE_URL`'s `options` or an `ALTER ROLE`),
///   which would cut a refresh lease's hour-long wait or end a standby round
///   early. The steps a refresh lease admits run on pooled connections and
///   keep whatever bounds their pool sets.
/// * `idle_session_timeout` 0 (Postgres 14 on) and `transaction_timeout` 0
///   (Postgres 17 on): a lease session is idle between checks (a refresh
///   holder for minutes at a time), and an operator's timeout would end it
///   and free the lock mid-run. A server too old to know one has no such
///   timeout to turn off, so its "unrecognized parameter" is not an error.
///
/// Postgres ignores the TCP settings on a Unix-socket connection.
pub const SESSION_SETTINGS: [Setting; 7] = [
    Setting::always("tcp_keepalives_idle", "10"),
    Setting::always("tcp_keepalives_interval", "5"),
    Setting::always("tcp_keepalives_count", "3"),
    Setting::always("tcp_user_timeout", "15000"),
    Setting::always("statement_timeout", "0"),
    Setting::from_pg(14, "idle_session_timeout", "0"),
    Setting::from_pg(17, "transaction_timeout", "0"),
];

/// One of [`SESSION_SETTINGS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    /// The parameter.
    pub name: &'static str,
    /// Its value for a lease session.
    pub value: &'static str,
    /// The first Postgres major version that has it, when that is later
    /// than the oldest one supported; an older server's "unrecognized
    /// parameter" is then not an error.
    pub since: Option<u32>,
}

impl Setting {
    const fn always(name: &'static str, value: &'static str) -> Self {
        Self {
            name,
            value,
            since: None,
        }
    }

    const fn from_pg(major: u32, name: &'static str, value: &'static str) -> Self {
        Self {
            name,
            value,
            since: Some(major),
        }
    }
}

/// Postgres's SQLSTATE for an unrecognized configuration parameter.
const UNDEFINED_OBJECT: &str = "42704";

/// Postgres keeps 63 bytes of `application_name`.
const NAME_BYTES: usize = 63;

/// The label's fixed parts: ` (` + `) ` + `since ` + `YYYY-MM-DD HH:MMZ`.
const LABEL_FIXED: usize = 2 + 2 + 6 + 17;

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Refresh {}
    impl Sealed for super::Gateway {}
}

/// What a [`Lease`] guards. Sealed: the advisory keys are listed in one
/// place (the module docs), and a key added there is a marker type here.
pub trait LeaseKey: sealed::Sealed {
    /// The advisory lock key.
    const LOCK: i64;
    /// The prefix of the lease session's `application_name`, ASCII.
    const LABEL: &'static str;
    /// The lease's name in a log line or an error.
    const NOUN: &'static str;
    /// The most of a process name the label keeps, so the longest label is
    /// [`NAME_BYTES`].
    const PROCESS_CHARS: usize = NAME_BYTES - Self::LABEL.len() - LABEL_FIXED;
}

/// The refresh lease's key: a data-writing ingest run. Its wait is bounded
/// by [`LEASE_WAIT`].
#[derive(Debug)]
pub enum Refresh {}

impl LeaseKey for Refresh {
    const LOCK: i64 = REFRESH_LOCK;
    const LABEL: &'static str = "judgebot refresh lease";
    const NOUN: &'static str = "refresh lease";
}

/// The Discord gateway lease's key: the one process connected to the gateway
/// on the bot's token. Its wait is a standby with no bound.
#[derive(Debug)]
pub enum Gateway {}

impl LeaseKey for Gateway {
    const LOCK: i64 = GATEWAY_LOCK;
    const LABEL: &'static str = "judgebot gateway";
    const NOUN: &'static str = "gateway lease";
}

// Each label leaves room for a process name: 22 + 14 + 27 = 63 for the
// refresh lease, as before it was generic; 16 + 20 + 27 for the gateway.
const _: () = assert!(Refresh::PROCESS_CHARS == 14 && Gateway::PROCESS_CHARS == 20);

/// Proof that this process holds a refresh run's lock; see the module docs.
/// Made only by [`Lease::try_acquire`] and [`RefreshLease::acquire`].
///
/// Steps borrow it mutably, so they run one after another:
///
/// ```no_run
/// # async fn f(lease: &mut judge_bot::lease::RefreshLease, dir: &std::path::Path) {
/// use judge_bot::ingest::{cr, scryfall};
/// let _ = scryfall::run(lease, dir).await;
/// let _ = cr::run_latest(lease, dir).await;
/// # }
/// ```
///
/// and never side by side:
///
/// ```compile_fail,E0499
/// # async fn f(lease: &mut judge_bot::lease::RefreshLease, dir: &std::path::Path) {
/// use judge_bot::ingest::{cr, scryfall};
/// let _ = tokio::join!(scryfall::run(lease, dir), cr::run_latest(lease, dir));
/// # }
/// ```
///
/// and a gateway lease is not one:
///
/// ```compile_fail,E0308
/// # async fn f(lease: &mut judge_bot::lease::GatewayLease, dir: &std::path::Path) {
/// let _ = judge_bot::ingest::scryfall::run(lease, dir).await;
/// # }
/// ```
pub type RefreshLease = Lease<Refresh>;

/// Proof that this process holds the Discord gateway's lock; see the module
/// docs and [`crate::discord::gateway`]. Made only by [`Lease::try_acquire`]
/// and [`GatewayLease::stand_by`].
pub type GatewayLease = Lease<Gateway>;

/// A session-level advisory lock on `K`'s key, held on a connection of its
/// own; see the module docs.
#[derive(Debug)]
pub struct Lease<K: LeaseKey> {
    /// The session holding the lock, detached from `pool`.
    conn: PgConnection,
    /// What the steps run on.
    pool: PgPool,
    /// Who holds it: the binary's name, recorded with a refresh run.
    process: &'static str,
    /// The session's `application_name`, as the server set it.
    name: Option<String>,
    key: PhantomData<K>,
}

impl<K: LeaseKey> Lease<K> {
    /// The pool the lease was taken on, for the steps it admits.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The process name the lease was taken with.
    #[must_use]
    pub const fn process(&self) -> &'static str {
        self.process
    }

    /// The lease session's `application_name` (`judgebot gateway
    /// (judgebot) since …`), or `None` if labelling it failed.
    #[must_use]
    pub fn application_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Whether this session still holds the lock: `Err` when the session is
    /// gone or no longer holds it, in which case another process may hold it
    /// and the caller must stop.
    ///
    /// # Errors
    /// When the lease is lost, or its session cannot be asked.
    pub async fn check(&mut self) -> Result<()> {
        let (classid, objid) = halves(K::LOCK);
        let held = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM pg_locks
                 WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND granted
                   AND classid = $1::bigint::oid AND objid = $2::bigint::oid AND objsubid = 1
               ) AS "held!""#,
            classid,
            objid,
        )
        .fetch_one(&mut self.conn)
        .await
        .with_context(|| format!("{} lost: its session cannot be reached", K::NOUN))?;
        anyhow::ensure!(
            held,
            "{} lost: its session no longer holds the lock",
            K::NOUN
        );
        Ok(())
    }

    /// Unlock, then close the session. The unlock is a round trip, so the
    /// next [`Lease::try_acquire`] succeeds as soon as this returns; failures
    /// are logged only, because closing (or dropping) the session releases
    /// the lock anyway.
    pub async fn release(mut self) {
        match sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", K::LOCK)
            .fetch_one(&mut self.conn)
            .await
        {
            Ok(Some(true)) => {}
            Ok(held) => {
                tracing::warn!(?held, "the {} was not held at release", K::NOUN);
            }
            Err(e) => {
                tracing::warn!(error = %e, "releasing the {} (the server releases it with the connection)", K::NOUN);
            }
        }
        if let Err(e) = self.conn.close().await {
            tracing::debug!(error = %e, "closing the {} connection", K::NOUN);
        }
    }

    /// The lease if no other process holds it, else `None` at once. A miss
    /// returns the connection it tried on to the pool unchanged, so polling
    /// costs nothing.
    ///
    /// # Errors
    /// When the database cannot be reached.
    pub async fn try_acquire(pool: &PgPool, process: &'static str) -> Result<Option<Self>> {
        let mut pooled = pool
            .acquire()
            .await
            .with_context(|| format!("connecting for the {}", K::NOUN))?;
        if !try_on::<K>(&mut pooled).await? {
            return Ok(None);
        }
        // Hardened only once won, so a miss returns the pooled connection
        // with its settings untouched; a failure drops the session, and the
        // lock with it.
        let mut conn = pooled.detach();
        harden::<K>(&mut conn).await?;
        Ok(Some(Self::holding(conn, pool, process).await))
    }

    /// A session that has just taken the lock, labelled as holding it.
    async fn holding(mut conn: PgConnection, pool: &PgPool, process: &'static str) -> Self {
        let name = label::<K>(&mut conn, process, Label::Holding).await;
        Self {
            conn,
            pool: pool.clone(),
            process,
            name,
            key: PhantomData,
        }
    }
}

impl Lease<Refresh> {
    /// The lease, waiting up to [`LEASE_WAIT`] for a run that holds it to
    /// finish, with a warning naming the holder first so a wait does not read
    /// as a hang.
    ///
    /// # Errors
    /// When the database cannot be reached, or the lease is still held after
    /// [`LEASE_WAIT`] (the error names the holder).
    pub async fn acquire(pool: &PgPool, process: &'static str) -> Result<Self> {
        Self::acquire_within(pool, process, LEASE_WAIT).await
    }

    async fn acquire_within(pool: &PgPool, process: &'static str, wait: Duration) -> Result<Self> {
        if let Some(l) = Self::try_acquire(pool, process).await? {
            return Ok(l);
        }
        // Detached before waiting: a cancelled wait must not hand the pool a
        // session still queued for the lock.
        let mut conn = pool
            .acquire()
            .await
            .context("connecting for the refresh lease")?
            .detach();
        harden::<Refresh>(&mut conn).await?;
        label::<Refresh>(&mut conn, process, Label::Waiting).await;
        let held_by = holder::<Refresh>(&mut conn).await;
        tracing::warn!(
            holder = %held_by,
            wait_mins = wait.as_secs() / 60,
            "another refresh or ingest step holds the refresh lease; waiting for it to finish"
        );
        // The server enforces the bound (`lock_timeout` applies to advisory
        // locks), so a timed-out wait leaves no queued session behind.
        set_lock_timeout(&mut conn, wait)
            .await
            .context("bounding the wait for the refresh lease")?;
        match sqlx::query!("SELECT pg_advisory_lock($1)", REFRESH_LOCK)
            .execute(&mut conn)
            .await
        {
            Ok(_) => {}
            Err(e) if timed_out(&e) => {
                let held_by = holder::<Refresh>(&mut conn).await;
                anyhow::bail!(
                    "the refresh lease is still held after {} min, by {held_by}; if that run is hung, end it with \
                     `select pg_terminate_backend(<pid>)` and run again",
                    wait.as_secs() / 60
                );
            }
            Err(e) => return Err(e).context("waiting for the refresh lease"),
        }
        set_lock_timeout(&mut conn, Duration::ZERO)
            .await
            .context("resetting lock_timeout on the refresh lease")?;
        Ok(Self::holding(conn, pool, process).await)
    }
}

impl Lease<Gateway> {
    /// The gateway lease, standing by for as long as another process holds
    /// it. Never fails: a standby whose connection drops (the database
    /// restarted, the network went) logs it and reconnects after a pause
    /// ([`STANDBY_RETRY_FIRST`], doubling to [`STANDBY_RETRY_MAX`]), so the
    /// process waits out a database outage instead of exiting over it.
    pub async fn stand_by(pool: &PgPool, process: &'static str) -> Self {
        Self::stand_by_in(pool, process, STANDBY_ROUND).await
    }

    async fn stand_by_in(pool: &PgPool, process: &'static str, round: Duration) -> Self {
        let mut pause = STANDBY_RETRY_FIRST;
        loop {
            let began = Instant::now();
            match Self::wait_once(pool, process, round).await {
                Ok(lease) => return lease,
                Err(e) => {
                    // A connection that served a while before failing starts
                    // the backoff afresh.
                    if began.elapsed() > STANDBY_RETRY_MAX {
                        pause = STANDBY_RETRY_FIRST;
                    }
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        retry_secs = pause.as_secs(),
                        "waiting for the gateway lease: the database connection failed; reconnecting"
                    );
                    tokio::time::sleep(pause).await;
                    pause = pause.saturating_mul(2).min(STANDBY_RETRY_MAX);
                }
            }
        }
    }

    /// One connection's worth of standby: the lease, or the error that ended
    /// the connection.
    async fn wait_once(pool: &PgPool, process: &'static str, round: Duration) -> Result<Self> {
        // Detached at once: the session either holds the lock or waits for it,
        // and neither may go back to the pool.
        let mut conn = pool
            .acquire()
            .await
            .context("connecting for the gateway lease")?
            .detach();
        // Every query before the wait is bounded, so a standby on a link
        // that died without a word reconnects instead of hanging on it.
        harden::<Gateway>(&mut conn).await?;
        if quick(try_on::<Gateway>(&mut conn)).await?? {
            return Ok(Self::holding(conn, pool, process).await);
        }
        label::<Gateway>(&mut conn, process, Label::Waiting).await;
        let held_by = holder::<Gateway>(&mut conn).await;
        tracing::info!(
            holder = %held_by,
            "standing by: another instance holds the Discord gateway; this one connects when it lets go"
        );
        quick(set_lock_timeout(&mut conn, round))
            .await?
            .context("bounding a round of the gateway standby")?;
        let limit = round.saturating_add(STANDBY_SLACK);
        loop {
            let asked =
                sqlx::query!("SELECT pg_advisory_lock($1)", GATEWAY_LOCK).execute(&mut conn);
            match tokio::time::timeout(limit, asked).await {
                Ok(Ok(_)) => break,
                // The round ended (`lock_timeout`), or someone cancelled the
                // statement (`pg_cancel_backend`): the session is fine.
                Ok(Err(e)) if timed_out(&e) || cancelled(&e) => {
                    tracing::debug!("still standing by for the gateway lease");
                }
                Ok(Err(e)) => return Err(e).context("waiting for the gateway lease"),
                // Dropping the connection (on return) ends the session, and
                // with it any lock the server granted unseen.
                Err(_) => anyhow::bail!(
                    "waiting for the gateway lease: no answer from the database in {} s",
                    limit.as_secs()
                ),
            }
        }
        quick(set_lock_timeout(&mut conn, Duration::ZERO))
            .await?
            .context("resetting lock_timeout on the gateway lease")?;
        Ok(Self::holding(conn, pool, process).await)
    }
}

/// `query`, or an error once it has gone [`QUICK`] without an answer.
async fn quick<T>(query: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(QUICK, query)
        .await
        .with_context(|| format!("no answer from the database in {} s", QUICK.as_secs()))
}

/// Give a lease session [`SESSION_SETTINGS`], bounded by [`QUICK`].
///
/// # Errors
/// When the session does not answer, or refuses a setting.
async fn harden<K: LeaseKey>(conn: &mut PgConnection) -> Result<()> {
    for setting in SESSION_SETTINGS {
        let set = quick(
            sqlx::query_scalar!(
                r#"SELECT set_config($1, $2, false) AS "set!""#,
                setting.name,
                setting.value
            )
            .fetch_one(&mut *conn),
        )
        .await?;
        match set {
            Ok(_) => {}
            // Each statement runs on its own, so the error leaves the
            // session as it was.
            Err(sqlx::Error::Database(e))
                if setting.since.is_some() && e.code().as_deref() == Some(UNDEFINED_OBJECT) =>
            {
                tracing::debug!(
                    name = setting.name,
                    since = setting.since,
                    "the server predates this setting; nothing to turn off"
                );
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("setting {} on the {} session", setting.name, K::NOUN)
                });
            }
        }
    }
    Ok(())
}

/// The two halves `pg_locks` shows a bigint advisory key as (`classid`,
/// `objid`, with `objsubid = 1`).
const fn halves(key: i64) -> (i64, i64) {
    (key >> 32, key & 0xffff_ffff)
}

async fn try_on<K: LeaseKey>(conn: &mut PgConnection) -> Result<bool> {
    sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "free!""#, K::LOCK)
        .fetch_one(conn)
        .await
        .with_context(|| format!("trying the {}", K::NOUN))
}

/// Set the session's `lock_timeout` (zero turns it off).
async fn set_lock_timeout(conn: &mut PgConnection, wait: Duration) -> Result<()> {
    sqlx::query_scalar!(
        r#"SELECT set_config('lock_timeout', $1, false) AS "set!""#,
        format!("{}ms", wait.as_millis())
    )
    .fetch_one(conn)
    .await?;
    Ok(())
}

/// Postgres's SQLSTATE for `lock_timeout` expiring.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// Postgres's SQLSTATE for a cancelled statement (`pg_cancel_backend`, or a
/// `statement_timeout`, which lease sessions turn off).
const QUERY_CANCELED: &str = "57014";

/// Was a statement cancelled?
fn cancelled(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(e) if e.code().as_deref() == Some(QUERY_CANCELED))
}

/// Did a lock wait end on `lock_timeout`?
fn timed_out(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(e) if e.code().as_deref() == Some(LOCK_NOT_AVAILABLE))
}

/// How the lease session is labelled.
#[derive(Clone, Copy)]
enum Label {
    /// `<LABEL> (<process>) since <now, UTC minute>`.
    Holding,
    /// `<LABEL> (<process>) waiting`; `query_start` says since when.
    Waiting,
}

/// Label the session for `pg_stat_activity`, returning the name the server
/// set. Postgres keeps 63 bytes of it (and logs a notice when it cuts), so
/// the process name is cut to [`LeaseKey::PROCESS_CHARS`], which keeps the
/// longest label at 63. Cosmetic, so a failure (or no answer within
/// [`QUICK`]) is logged only.
async fn label<K: LeaseKey>(
    conn: &mut PgConnection,
    process: &str,
    label: Label,
) -> Option<String> {
    let process: String = process.chars().take(K::PROCESS_CHARS).collect();
    let (prefix, stamp) = match label {
        Label::Holding => (format!("{} ({process}) since ", K::LABEL), true),
        Label::Waiting => (format!("{} ({process}) waiting", K::LABEL), false),
    };
    let set = sqlx::query_scalar!(
        r#"SELECT set_config('application_name',
             $1 || CASE WHEN $2 THEN to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI"Z"') ELSE '' END,
             false) AS "set!""#,
        prefix,
        stamp,
    )
    .fetch_one(conn);
    match quick(set)
        .await
        .and_then(|r| r.map_err(anyhow::Error::from))
    {
        Ok(name) => Some(name),
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "labelling the {} session", K::NOUN);
            None
        }
    }
}

/// What `pg_stat_activity` says about the session holding `K`'s lock, for a
/// log line or an error. Best effort, bounded by [`QUICK`]: a failure says
/// so instead.
async fn holder<K: LeaseKey>(conn: &mut PgConnection) -> String {
    let (classid, objid) = halves(K::LOCK);
    let row = sqlx::query!(
        r#"SELECT a.pid AS "pid!", a.application_name AS "application_name!",
                  extract(epoch FROM now() - a.backend_start)::bigint AS "connected_secs?",
                  a.state
           FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
           WHERE l.locktype = 'advisory' AND l.granted
             AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
             AND l.classid = $1::bigint::oid AND l.objid = $2::bigint::oid AND l.objsubid = 1"#,
        classid,
        objid,
    )
    .fetch_optional(conn);
    match quick(row)
        .await
        .and_then(|r| r.map_err(anyhow::Error::from))
    {
        Ok(Some(r)) => format!(
            "pid {} ({:?}, connected {} s ago, {})",
            r.pid,
            r.application_name,
            r.connected_secs.unwrap_or_default(),
            r.state.as_deref().unwrap_or("state unknown")
        ),
        Ok(None) => "a session that has just released it".to_owned(),
        Err(e) => format!("an unidentified session ({e:#})"),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! What the lease's tests and the gateway's share: waiting for a session
    //! to reach a state, seen from another session.

    use std::time::Duration;

    use anyhow::{Context as _, Result};
    use sqlx::PgPool;

    pub use crate::clock::manual::HANG;

    /// `probe`'s first `Some`, polled until [`HANG`] has passed, which fails
    /// naming `what`. The deadline only guards against a hang; a test
    /// asserts what the probe finds, never how soon.
    ///
    /// # Errors
    /// The probe's, or none in time.
    pub async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        let found = async {
            loop {
                if let Some(t) = probe().await? {
                    return anyhow::Ok(t);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(HANG, found)
            .await
            .with_context(|| format!("{what}: not within {} s", HANG.as_secs()))?
    }

    /// The pid and current statement start of each of this test database's
    /// sessions named `name` that waits for a lock (an advisory lock is a
    /// lock wait like any other). `pg_stat_activity` spans the cluster, and
    /// other tests' databases hold leases of their own.
    ///
    /// # Errors
    /// The query's.
    pub async fn lock_waits(pool: &PgPool, name: &str) -> Result<Vec<(i32, String)>> {
        Ok(sqlx::query_as(
            "SELECT pid, query_start::text FROM pg_stat_activity
             WHERE datname = current_database() AND application_name = $1
               AND state = 'active' AND wait_event_type = 'Lock'
             ORDER BY pid",
        )
        .bind(name)
        .fetch_all(pool)
        .await?)
    }

    /// Wait until a session named `name` waits for a lock: its pid and
    /// statement start.
    ///
    /// # Errors
    /// None in time, or a query's.
    pub async fn waiting(pool: &PgPool, name: &str) -> Result<(i32, String)> {
        eventually(&format!("{name:?} waiting for a lock"), || async {
            Ok(lock_waits(pool, name).await?.into_iter().next())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        testing::{HANG, eventually, lock_waits, waiting},
        *,
    };

    async fn try_refresh(pool: &PgPool, process: &'static str) -> Result<Option<RefreshLease>> {
        RefreshLease::try_acquire(pool, process).await
    }

    async fn try_gateway(pool: &PgPool, process: &'static str) -> Result<Option<GatewayLease>> {
        GatewayLease::try_acquire(pool, process).await
    }

    /// The pid of this test database's session whose `application_name`
    /// starts with `prefix`. `pg_stat_activity` spans the cluster, and other
    /// tests' databases hold leases of their own.
    async fn session_pid(pool: &PgPool, prefix: &str) -> Result<Option<i32>> {
        Ok(sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE datname = current_database() AND application_name LIKE $1 || '%'",
        )
        .bind(prefix)
        .fetch_optional(pool)
        .await?)
    }

    async fn terminate(pool: &PgPool, pid: i32) -> Result<()> {
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(pool)
            .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn one_lease_at_a_time_and_release_frees_it(pool: PgPool) -> Result<()> {
        let first = try_refresh(&pool, "test").await?.context("a free lease")?;
        assert!(
            try_refresh(&pool, "test").await?.is_none(),
            "the lease is held"
        );
        first.release().await;
        let again = try_refresh(&pool, "test")
            .await?
            .context("free after release")?;
        again.release().await;
        Ok(())
    }

    async fn backend(pool: &PgPool) -> Result<(i32, String)> {
        Ok(
            sqlx::query_as("SELECT pg_backend_pid(), current_setting('application_name')")
                .fetch_one(pool)
                .await?,
        )
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_miss_keeps_the_pooled_connection(pool: PgPool) -> Result<()> {
        let held = try_refresh(&pool, "test").await?.context("a free lease")?;
        // A pool of one session: each acquire waits for the connection the
        // one before it returned, so a miss that detached or closed its
        // connection would leave the next acquire a new session.
        let one = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with((*pool.connect_options()).clone())
            .await?;
        let before = backend(&one).await?;
        for _ in 0..5 {
            assert!(try_refresh(&one, "test").await?.is_none());
        }
        assert_eq!(
            backend(&one).await?,
            before,
            "the misses tried on the pooled session and gave it back unlabelled"
        );
        one.close().await;
        held.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_dropped_lease_is_freed_by_the_server(pool: PgPool) -> Result<()> {
        let first = try_refresh(&pool, "test").await?.context("a free lease")?;
        drop(first);
        // The socket closed on drop; the server ends the session, and frees
        // the lock, once it notices.
        let again = eventually("the dropped lease freed", || try_refresh(&pool, "test")).await?;
        again.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_waiting_form_gets_the_lease_once_it_is_released(pool: PgPool) -> Result<()> {
        let first = try_refresh(&pool, "holder")
            .await?
            .context("a free lease")?;
        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move { RefreshLease::acquire(&pool, "test").await }
        });
        waiting(&pool, "judgebot refresh lease (test) waiting").await?;
        assert!(!waiter.is_finished(), "it waits while the lease is held");
        first.release().await;
        let mut second = tokio::time::timeout(HANG, waiter).await???;
        assert!(
            try_refresh(&pool, "test").await?.is_none(),
            "the waiter holds it now"
        );
        second.check().await?;
        second.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_wait_is_bounded_and_names_the_holder(pool: PgPool) -> Result<()> {
        let first = try_refresh(&pool, "holder")
            .await?
            .context("a free lease")?;
        let err = RefreshLease::acquire_within(&pool, "test", Duration::from_millis(300))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("still held"), "{err}");
        assert!(
            err.contains("judgebot refresh lease (holder) since "),
            "names the holder's application_name: {err}"
        );
        first.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_lease_whose_session_died_is_reported_lost(pool: PgPool) -> Result<()> {
        let mut lease = try_refresh(&pool, "test").await?.context("a free lease")?;
        lease.check().await?;
        let pid = session_pid(&pool, "judgebot refresh lease (test)")
            .await?
            .context("the lease's session")?;
        terminate(&pool, pid).await?;
        let err = lease.check().await.err().map(|e| format!("{e:#}"));
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("refresh lease lost")),
            "{err:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_lease_does_not_block_the_calls_rewrite_lock(pool: PgPool) -> Result<()> {
        let lease = try_refresh(&pool, "test").await?.context("a free lease")?;
        // What a step inside a run does (the CR load, the retirement pass),
        // and what another process's migration or persist does meanwhile.
        let step = async {
            let mut tx = lease.pool().begin().await?;
            sqlx::query!(
                "SELECT pg_advisory_xact_lock($1)",
                crate::db::CALLS_REWRITE_LOCK
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            anyhow::Ok(())
        };
        tokio::time::timeout(HANG, step).await??;
        let mut other = pool.acquire().await?;
        let free = sqlx::query_scalar!(
            r#"SELECT pg_try_advisory_lock($1) AS "free!""#,
            crate::db::CALLS_REWRITE_LOCK
        )
        .fetch_one(&mut *other)
        .await?;
        assert!(
            free,
            "the calls rewrite lock is free while the lease is held"
        );
        sqlx::query_scalar!(
            "SELECT pg_advisory_unlock($1)",
            crate::db::CALLS_REWRITE_LOCK
        )
        .fetch_one(&mut *other)
        .await?;
        lease.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn one_gateway_holder_and_a_standby(pool: PgPool) -> Result<()> {
        let holder = try_gateway(&pool, "first").await?.context("a free lease")?;
        assert!(
            holder
                .application_name()
                .is_some_and(|n| n.starts_with("judgebot gateway (first) since ")),
            "{:?}",
            holder.application_name()
        );
        assert!(try_gateway(&pool, "second").await?.is_none(), "held");
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by(&pool, "second").await }
        });
        // Labelled as waiting, and waiting on the lock.
        waiting(&pool, "judgebot gateway (second) waiting").await?;
        assert!(
            !standby.is_finished(),
            "it stands by while the lease is held"
        );
        holder.release().await;
        let mut taken = tokio::time::timeout(HANG, standby).await??;
        taken.check().await?;
        assert!(try_gateway(&pool, "third").await?.is_none(), "taken over");
        taken.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_standby_takes_over_from_a_dropped_holder(pool: PgPool) -> Result<()> {
        let holder = try_gateway(&pool, "first").await?.context("a free lease")?;
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by(&pool, "second").await }
        });
        waiting(&pool, "judgebot gateway (second) waiting").await?;
        // What a process that exits does: its socket closes.
        drop(holder);
        let taken = tokio::time::timeout(HANG, standby).await??;
        taken.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_terminated_gateway_session_is_reported_lost(pool: PgPool) -> Result<()> {
        let mut lease = try_gateway(&pool, "test").await?.context("a free lease")?;
        lease.check().await?;
        let pid = session_pid(&pool, "judgebot gateway (test) since")
            .await?
            .context("the lease's session")?;
        terminate(&pool, pid).await?;
        let err = lease.check().await.err().map(|e| format!("{e:#}"));
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("gateway lease lost")),
            "{err:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn refresh_and_gateway_leases_are_independent(pool: PgPool) -> Result<()> {
        let refresh = try_refresh(&pool, "test").await?.context("refresh free")?;
        let gateway = try_gateway(&pool, "test")
            .await?
            .context("the gateway lease is free beside a refresh run")?;
        gateway.release().await;
        refresh.release().await;
        let gateway = try_gateway(&pool, "test").await?.context("gateway free")?;
        let refresh = try_refresh(&pool, "test")
            .await?
            .context("the refresh lease is free beside the gateway holder")?;
        refresh.release().await;
        gateway.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_standby_whose_connection_died_reconnects(pool: PgPool) -> Result<()> {
        let holder = try_gateway(&pool, "first").await?.context("a free lease")?;
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by(&pool, "second").await }
        });
        let name = "judgebot gateway (second) waiting";
        let (pid, _) = waiting(&pool, name).await?;
        terminate(&pool, pid).await?;
        // The standby logs, pauses and stands by again on a new session.
        eventually("the standby waiting on a new session", || async {
            Ok(lock_waits(&pool, name)
                .await?
                .into_iter()
                .find(|(p, _)| *p != pid))
        })
        .await?;
        assert!(!standby.is_finished(), "and still stands by");
        holder.release().await;
        let taken = tokio::time::timeout(HANG, standby).await??;
        taken.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_standby_round_ends_and_the_standby_asks_again(pool: PgPool) -> Result<()> {
        let holder = try_gateway(&pool, "first").await?.context("a free lease")?;
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by_in(&pool, "second", Duration::from_millis(100)).await }
        });
        // A round ends on lock_timeout and the standby asks again: a new
        // statement on the same session, twice over.
        let name = "judgebot gateway (second) waiting";
        let (pid, mut round) = waiting(&pool, name).await?;
        for _ in 0..2 {
            round = eventually("the next round on the same session", || async {
                let waits = lock_waits(&pool, name).await?;
                anyhow::ensure!(
                    waits.iter().all(|(p, _)| *p == pid),
                    "only the first session waits: {waits:?}"
                );
                Ok(waits
                    .into_iter()
                    .find(|(_, started)| *started != round)
                    .map(|(_, started)| started))
            })
            .await?;
        }
        assert!(!standby.is_finished(), "it stands by across rounds");
        holder.release().await;
        let mut taken = tokio::time::timeout(HANG, standby).await??;
        taken.check().await?;
        taken.release().await;
        Ok(())
    }

    /// Assert that the session holds every one of [`SESSION_SETTINGS`] the
    /// server knows, and every one it must know. `current_setting` shows
    /// each value as written, over TCP, which the tests use (a Unix socket
    /// shows the TCP ones as 0).
    async fn assert_hardened(conn: &mut PgConnection) -> Result<()> {
        for setting in SESSION_SETTINGS {
            let shown: Option<String> = sqlx::query_scalar("SELECT current_setting($1, true)")
                .bind(setting.name)
                .fetch_one(&mut *conn)
                .await?;
            match (shown, setting.since) {
                (Some(v), _) => assert_eq!(v, setting.value, "{}", setting.name),
                (None, Some(_)) => {}
                (None, None) => anyhow::bail!("{} is unknown to the server", setting.name),
            }
        }
        Ok(())
    }

    /// A pool on the test database whose sessions start with `name` set to
    /// `value`, as an operator's `DATABASE_URL` might.
    async fn with_option(pool: &PgPool, name: &str, value: &str) -> Result<PgPool> {
        let options = (*pool.connect_options()).clone().options([(name, value)]);
        Ok(sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?)
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn every_lease_session_is_hardened(pool: PgPool) -> Result<()> {
        let pool = with_option(&pool, "statement_timeout", "30min").await?;
        let mut refresh = try_refresh(&pool, "test").await?.context("refresh free")?;
        assert_hardened(&mut refresh.conn).await?;
        let mut holder = try_gateway(&pool, "first").await?.context("gateway free")?;
        assert_hardened(&mut holder.conn).await?;
        // A pooled connection a miss tried on keeps the pool's settings.
        assert!(try_gateway(&pool, "again").await?.is_none());
        let mut pooled = pool.acquire().await?;
        let timeout: String = sqlx::query_scalar("SELECT current_setting('statement_timeout')")
            .fetch_one(&mut *pooled)
            .await?;
        assert_eq!(timeout, "30min");
        drop(pooled);
        // A standby's session, and the lease it ends up with.
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by(&pool, "second").await }
        });
        waiting(&pool, "judgebot gateway (second) waiting").await?;
        assert!(!standby.is_finished(), "it stands by");
        holder.release().await;
        let mut taken = tokio::time::timeout(HANG, standby).await??;
        assert_hardened(&mut taken.conn).await?;
        taken.release().await;
        refresh.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn an_operator_statement_timeout_does_not_cut_a_lease_wait(base: PgPool) -> Result<()> {
        // The operator's bound is on `pool`; the test watches from `base`,
        // which has none. Every wait below outlasts the bound several times.
        let bound = Duration::from_millis(250);
        let pool = with_option(
            &base,
            "statement_timeout",
            &format!("{}ms", bound.as_millis()),
        )
        .await?;
        let first = try_refresh(&pool, "holder")
            .await?
            .context("a free lease")?;
        // The wait ends on its own bound (lock_timeout), not the operator's,
        // which would end it with another error.
        let err = RefreshLease::acquire_within(&pool, "test", bound * 4)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("still held"), "{err}");
        // A standby's statement outlives it too, on the same session.
        let gateway = try_gateway(&pool, "first").await?.context("gateway free")?;
        let standby = tokio::spawn({
            let pool = pool.clone();
            async move { GatewayLease::stand_by(&pool, "second").await }
        });
        let (pid, started) = waiting(&base, "judgebot gateway (second) waiting").await?;
        let past = (bound * 3).as_secs_f64();
        eventually("the standby's statement outliving the bound", || async {
            let age: Option<f64> = sqlx::query_scalar(
                "SELECT extract(epoch FROM clock_timestamp() - query_start)::float8
                 FROM pg_stat_activity
                 WHERE pid = $1 AND query_start::text = $2
                   AND state = 'active' AND wait_event_type = 'Lock'",
            )
            .bind(pid)
            .bind(&started)
            .fetch_optional(&base)
            .await?;
            let age = age.context("the standby's statement ended before its round did")?;
            Ok((age > past).then_some(()))
        })
        .await?;
        gateway.release().await;
        let taken = tokio::time::timeout(HANG, standby).await??;
        taken.release().await;
        first.release().await;
        pool.close().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn an_operator_idle_session_timeout_does_not_end_a_lease(base: PgPool) -> Result<()> {
        let pool = with_option(&base, "idle_session_timeout", "1s").await?;
        let mut refresh = try_refresh(&pool, "test").await?.context("refresh free")?;
        let mut gateway = try_gateway(&pool, "test").await?.context("gateway free")?;
        // An ordinary session, idle since after the leases' last statements.
        let mut plain = pool.acquire().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *plain)
            .await?;
        // Once the operator's timeout has ended it, the leases have been
        // idle longer than it.
        eventually("the operator's timeout ending an idle session", || async {
            let alive: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)")
                    .bind(pid)
                    .fetch_one(&base)
                    .await?;
            Ok((!alive).then_some(()))
        })
        .await?;
        assert!(
            sqlx::query("SELECT 1").execute(&mut *plain).await.is_err(),
            "the operator's timeout ends an ordinary idle session"
        );
        drop(plain);
        refresh.check().await?;
        gateway.check().await?;
        gateway.release().await;
        refresh.release().await;
        pool.close().await;
        Ok(())
    }
}
