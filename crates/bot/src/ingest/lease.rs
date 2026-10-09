//! The refresh lease: one data-writing ingest run at a time, across every
//! process on the database.
//!
//! [`RefreshLease`] holds the session-level advisory lock [`REFRESH_LOCK`] on
//! a connection of its own for as long as it lives. Every step that writes
//! data takes `&mut RefreshLease` and reads its pool from it
//! ([`RefreshLease::pool`]), so a step run without the lease does not compile,
//! two steps cannot run at once under one lease (a `join!` of two needs two
//! mutable borrows), and the lock is always on the database the step writes.
//! It lives here rather than among the [`crate::db`] adapters because only
//! this module's entry points take it; the request path never does.
//!
//! **Why a key of its own.** [`CALLS_REWRITE_LOCK`](crate::db::CALLS_REWRITE_LOCK)
//! is short and transaction-scoped: the CR load, the retirement pass and each
//! vector write take it *inside* a run, and a migration takes it for its whole
//! run. This lease serialises whole runs and is held for minutes (a Scryfall
//! download, a CR parse). The two nest in one order only: a run holds the
//! lease and its steps take the calls lock; nothing takes the lease while
//! holding the calls lock (`init` releases the migration's before taking it),
//! so the two cannot deadlock. A migration does not wait for a refresh, only
//! for the step inside it that holds the calls lock.
//!
//! **Release.** [`RefreshLease::release`] unlocks and closes the connection.
//! The connection is detached from the pool once the lock is won (so it does
//! not count against the pool's size either, and the steps keep every pooled
//! connection), and it never goes back to the pool: a lease dropped without
//! `release` (an early return, a panic, a cancelled future) drops the socket,
//! and Postgres releases a session lock when its session ends. A crashed
//! process therefore leaves nothing behind to clean up.
//!
//! **Waiting is bounded.** [`lease`] waits at most [`LEASE_WAIT`], then fails
//! naming the holder, so a run that hangs makes the next one fail (and alert)
//! instead of queueing every later run behind it. The holder is findable:
//! the session's `application_name` is `judgebot refresh lease (<process>)
//! since <UTC minute>` (`… waiting` while queued, when `query_start` is
//! when the wait began), visible in `pg_stat_activity`.
//!
//! **A lost lease.** If the lease's session dies mid-run (the server restarted,
//! an operator ended it), another run may take the lock while this one is
//! still writing. [`RefreshLease::check`] asks the session whether it still
//! holds the lock; a run calls it before each step and stops when it fails.

use std::time::Duration;

use anyhow::{Context as _, Result};
use sqlx::{Connection as _, PgConnection, PgPool};

/// Advisory lock key for the refresh lease. Arbitrary, distinct from
/// [`crate::db::CALLS_REWRITE_LOCK`], and the same in every process.
pub const REFRESH_LOCK: i64 = 0x6a75_6467_6572_6566; // "judgeref"

/// The two halves `pg_locks` shows a bigint advisory key as (`classid`,
/// `objid`, with `objsubid = 1`).
const LOCK_CLASSID: i64 = REFRESH_LOCK >> 32;
const LOCK_OBJID: i64 = REFRESH_LOCK & 0xffff_ffff;

/// How long [`lease`] waits for another run. Longer than any healthy run (a
/// daily refresh takes minutes, a first `init` on a NAS well under an hour)
/// and far shorter than a day, so a hung run fails the next command with the
/// holder named instead of blocking it forever. The schedule ([`crate::jobs`])
/// never waits: it uses [`try_lease`] and checks again later.
pub const LEASE_WAIT: Duration = Duration::from_hours(1);

/// The prefix of the lease session's `application_name`.
pub const APPLICATION_NAME: &str = "judgebot refresh lease";

/// The most of a process name the label keeps: 22 (prefix) + 2 + 14 + 2 +
/// 6 (`since `) + 17 (`YYYY-MM-DD HH:MMZ`) = 63, Postgres's limit, ASCII.
const PROCESS_CHARS: usize = 14;
const _: () = assert!(APPLICATION_NAME.len() + 2 + PROCESS_CHARS + 2 + 6 + 17 <= 63);

/// Proof that this process holds [`REFRESH_LOCK`]; see the module docs.
/// Made only by [`try_lease`] and [`lease`].
///
/// Steps borrow it mutably, so they run one after another:
///
/// ```no_run
/// # async fn f(lease: &mut judge_bot::ingest::RefreshLease, dir: &std::path::Path) {
/// use judge_bot::ingest::{cr, scryfall};
/// let _ = scryfall::run(lease, dir).await;
/// let _ = cr::run_latest(lease, dir).await;
/// # }
/// ```
///
/// and never side by side:
///
/// ```compile_fail,E0499
/// # async fn f(lease: &mut judge_bot::ingest::RefreshLease, dir: &std::path::Path) {
/// use judge_bot::ingest::{cr, scryfall};
/// let _ = tokio::join!(scryfall::run(lease, dir), cr::run_latest(lease, dir));
/// # }
/// ```
#[derive(Debug)]
pub struct RefreshLease {
    /// The session holding the lock, detached from `pool`.
    conn: PgConnection,
    /// What the steps run on.
    pool: PgPool,
    /// Who holds it: the binary's name, recorded with a refresh run.
    process: &'static str,
}

impl RefreshLease {
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

    /// Whether this session still holds the lock: `Err` when the session is
    /// gone or no longer holds it, in which case another run may hold it and
    /// the caller must stop writing.
    ///
    /// # Errors
    /// When the lease is lost, or its session cannot be asked.
    pub async fn check(&mut self) -> Result<()> {
        let held = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM pg_locks
                 WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND granted
                   AND classid = $1::bigint::oid AND objid = $2::bigint::oid AND objsubid = 1
               ) AS "held!""#,
            LOCK_CLASSID,
            LOCK_OBJID,
        )
        .fetch_one(&mut self.conn)
        .await
        .context("refresh lease lost: its session cannot be reached")?;
        anyhow::ensure!(
            held,
            "refresh lease lost: its session no longer holds the lock"
        );
        Ok(())
    }

    /// Unlock, then close the session. The unlock is a round trip, so the
    /// next [`try_lease`] succeeds as soon as this returns; failures are
    /// logged only, because closing (or dropping) the session releases the
    /// lock anyway.
    pub async fn release(mut self) {
        match sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", REFRESH_LOCK)
            .fetch_one(&mut self.conn)
            .await
        {
            Ok(Some(true)) => {}
            Ok(held) => {
                tracing::warn!(?held, "the refresh lease was not held at release");
            }
            Err(e) => {
                tracing::warn!(error = %e, "releasing the refresh lease (the server releases it with the connection)");
            }
        }
        if let Err(e) = self.conn.close().await {
            tracing::debug!(error = %e, "closing the refresh lease connection");
        }
    }
}

async fn try_on(conn: &mut PgConnection) -> Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT pg_try_advisory_lock($1) AS "free!""#,
        REFRESH_LOCK
    )
    .fetch_one(conn)
    .await
    .context("trying the refresh lease")
}

/// How the lease session is labelled.
#[derive(Clone, Copy)]
enum Label {
    /// `<APPLICATION_NAME> (<process>) since <now, UTC minute>`.
    Holding,
    /// `<APPLICATION_NAME> (<process>) waiting`; `query_start` says since when.
    Waiting,
}

/// Label the session for `pg_stat_activity`. Postgres keeps 63 bytes of it
/// (and logs a notice when it cuts), so the process name is cut to
/// [`PROCESS_CHARS`], which keeps the longest label at 63. Cosmetic, so a
/// failure is logged only.
async fn label(conn: &mut PgConnection, process: &str, label: Label) {
    let process: String = process.chars().take(PROCESS_CHARS).collect();
    let (prefix, stamp) = match label {
        Label::Holding => (format!("{APPLICATION_NAME} ({process}) since "), true),
        Label::Waiting => (format!("{APPLICATION_NAME} ({process}) waiting"), false),
    };
    if let Err(e) = sqlx::query_scalar!(
        r#"SELECT set_config('application_name',
             $1 || CASE WHEN $2 THEN to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI"Z"') ELSE '' END,
             false) AS "set!""#,
        prefix,
        stamp,
    )
    .fetch_one(conn)
    .await
    {
        tracing::debug!(error = %e, "labelling the refresh lease session");
    }
}

/// The lease if no other run holds it, else `None` at once. A miss returns
/// the connection it tried on to the pool unchanged, so polling costs nothing.
///
/// # Errors
/// When the database cannot be reached.
pub async fn try_lease(pool: &PgPool, process: &'static str) -> Result<Option<RefreshLease>> {
    let mut pooled = pool
        .acquire()
        .await
        .context("connecting for the refresh lease")?;
    if !try_on(&mut pooled).await? {
        return Ok(None);
    }
    let mut conn = pooled.detach();
    label(&mut conn, process, Label::Holding).await;
    Ok(Some(RefreshLease {
        conn,
        pool: pool.clone(),
        process,
    }))
}

/// The lease, waiting up to [`LEASE_WAIT`] for a run that holds it to finish,
/// with a warning naming the holder first so a wait does not read as a hang.
///
/// # Errors
/// When the database cannot be reached, or the lease is still held after
/// [`LEASE_WAIT`] (the error names the holder).
pub async fn lease(pool: &PgPool, process: &'static str) -> Result<RefreshLease> {
    lease_within(pool, process, LEASE_WAIT).await
}

async fn lease_within(
    pool: &PgPool,
    process: &'static str,
    wait: Duration,
) -> Result<RefreshLease> {
    if let Some(l) = try_lease(pool, process).await? {
        return Ok(l);
    }
    // Detached before waiting: a cancelled wait must not hand the pool a
    // session still queued for the lock.
    let mut conn = pool
        .acquire()
        .await
        .context("connecting for the refresh lease")?
        .detach();
    label(&mut conn, process, Label::Waiting).await;
    let held_by = holder(&mut conn).await;
    tracing::warn!(
        holder = %held_by,
        wait_mins = wait.as_secs() / 60,
        "another refresh or ingest step holds the refresh lease; waiting for it to finish"
    );
    // The server enforces the bound (`lock_timeout` applies to advisory
    // locks), so a timed-out wait leaves no queued session behind.
    sqlx::query_scalar!(
        r#"SELECT set_config('lock_timeout', $1, false) AS "set!""#,
        format!("{}ms", wait.as_millis())
    )
    .fetch_one(&mut conn)
    .await
    .context("bounding the wait for the refresh lease")?;
    match sqlx::query!("SELECT pg_advisory_lock($1)", REFRESH_LOCK)
        .execute(&mut conn)
        .await
    {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(LOCK_NOT_AVAILABLE) => {
            let held_by = holder(&mut conn).await;
            anyhow::bail!(
                "the refresh lease is still held after {} min, by {held_by}; if that run is hung, end it with \
                 `select pg_terminate_backend(<pid>)` and run again",
                wait.as_secs() / 60
            );
        }
        Err(e) => return Err(e).context("waiting for the refresh lease"),
    }
    sqlx::query_scalar!(r#"SELECT set_config('lock_timeout', '0', false) AS "set!""#)
        .fetch_one(&mut conn)
        .await
        .context("resetting lock_timeout on the refresh lease")?;
    label(&mut conn, process, Label::Holding).await;
    Ok(RefreshLease {
        conn,
        pool: pool.clone(),
        process,
    })
}

/// Postgres's SQLSTATE for `lock_timeout` expiring.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// What `pg_stat_activity` says about the session holding the lease, for a
/// log line or an error. Best effort: a failure says so instead.
async fn holder(conn: &mut PgConnection) -> String {
    let row = sqlx::query!(
        r#"SELECT a.pid AS "pid!", a.application_name AS "application_name!",
                  extract(epoch FROM now() - a.backend_start)::bigint AS "connected_secs?",
                  a.state
           FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
           WHERE l.locktype = 'advisory' AND l.granted
             AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
             AND l.classid = $1::bigint::oid AND l.objid = $2::bigint::oid AND l.objsubid = 1"#,
        LOCK_CLASSID,
        LOCK_OBJID,
    )
    .fetch_optional(conn)
    .await;
    match row {
        Ok(Some(r)) => format!(
            "pid {} ({:?}, connected {} s ago, {})",
            r.pid,
            r.application_name,
            r.connected_secs.unwrap_or_default(),
            r.state.as_deref().unwrap_or("state unknown")
        ),
        Ok(None) => "a session that has just released it".to_owned(),
        Err(e) => format!("an unidentified session ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn one_lease_at_a_time_and_release_frees_it(pool: PgPool) -> Result<()> {
        let first = try_lease(&pool, "test").await?.context("a free lease")?;
        assert!(
            try_lease(&pool, "test").await?.is_none(),
            "the lease is held"
        );
        first.release().await;
        let again = try_lease(&pool, "test")
            .await?
            .context("free after release")?;
        again.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_miss_keeps_the_pooled_connection(pool: PgPool) -> Result<()> {
        let held = try_lease(&pool, "test").await?.context("a free lease")?;
        for _ in 0..5 {
            assert!(try_lease(&pool, "test").await?.is_none());
        }
        // A detached connection never comes back; the one a miss tried on does.
        assert!(pool.num_idle() >= 1, "the connection went back to the pool");
        held.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_dropped_lease_is_freed_by_the_server(pool: PgPool) -> Result<()> {
        let first = try_lease(&pool, "test").await?.context("a free lease")?;
        drop(first);
        // The socket closed on drop; the server notices and ends the session
        // asynchronously, so poll briefly rather than expect it at once.
        for _ in 0..100 {
            if let Some(l) = try_lease(&pool, "test").await? {
                l.release().await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("the lease was still held 5 s after it was dropped")
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_waiting_form_gets_the_lease_once_it_is_released(pool: PgPool) -> Result<()> {
        let first = try_lease(&pool, "test").await?.context("a free lease")?;
        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move { lease(&pool, "test").await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiter.is_finished(), "it waits while the lease is held");
        first.release().await;
        let mut second = tokio::time::timeout(Duration::from_secs(5), waiter).await???;
        assert!(
            try_lease(&pool, "test").await?.is_none(),
            "the waiter holds it now"
        );
        second.check().await?;
        second.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_wait_is_bounded_and_names_the_holder(pool: PgPool) -> Result<()> {
        let first = try_lease(&pool, "holder").await?.context("a free lease")?;
        let err = lease_within(&pool, "test", Duration::from_millis(300))
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
        let mut lease = try_lease(&pool, "test").await?.context("a free lease")?;
        lease.check().await?;
        let pid: i32 = sqlx::query_scalar(
            // pg_stat_activity spans the cluster, and other tests' databases
            // hold leases of their own.
            "SELECT pid FROM pg_stat_activity
             WHERE datname = current_database() AND application_name LIKE 'judgebot refresh lease (test)%'",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&pool)
            .await?;
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
        let lease = try_lease(&pool, "test").await?.context("a free lease")?;
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
        tokio::time::timeout(Duration::from_secs(5), step).await??;
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
}
