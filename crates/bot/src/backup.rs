//! The database backup: `pg_dump -Fc`, gzipped, uploaded to Cloudflare R2
//! (any S3-compatible store), then old backups pruned. `judgebot backup`
//! runs it: `run` takes one now, `list` and `fetch` serve the restore drill,
//! and `serve` is the `backup` compose service, which takes one whenever the
//! newest in the bucket is `BACKUP_EVERY_DAYS` old ([`schedule`]).
//!
//! It writes exactly what `scripts/backup-db.sh` writes: the same settings
//! ([`settings`], from `.env.deploy`), the same object names under the same
//! prefix ([`name`]), the same bytes (`pg_dump -Fc | gzip -9`, [`dump`]),
//! the same refusal of a dump under `BACKUP_MIN_BYTES`, and pruning only
//! after a successful upload. So the two read each other's backups, and an
//! operator can move from cron to the service, or run both.
//!
//! The service is its own container, not a role of the internet-facing
//! process: the R2 keys can delete every backup, and they stay out of that
//! process's environment (D15). It reaches Postgres over the compose network
//! like any client, so it needs neither the Docker socket nor a host cron.

pub mod dump;
pub mod name;
pub mod s3;
pub mod schedule;
pub mod settings;

use std::{
    io::IsTerminal as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{io::AsyncWriteExt as _, time::Instant};

use crate::alert;
use dump::{DUMP_TIMEOUT, DumpError, PgDump};
use name::{BackupName, Stamp};
use s3::S3Error;
use schedule::{Attempt, Before, Due, Stage, Streak};
use settings::{Settings, Store, is_plain_segment};

/// The first check after the service starts, before jitter: long enough
/// that a crash loop does not dump the database on every start.
pub const FIRST_CHECK: Duration = Duration::from_mins(2);
/// The time between checks, before jitter. A check lists the bucket, one
/// cheap request; a backup is at most this late.
pub const CHECK_EVERY: Duration = Duration::from_hours(1);
/// At most this is added to each wait.
pub const JITTER: Duration = Duration::from_mins(5);

/// A random part of [`JITTER`], from a v4 UUID (the randomness the crate
/// already has).
fn jitter() -> Duration {
    let max = JITTER.as_millis().max(1);
    let ms = uuid::Uuid::new_v4().as_u128() % max;
    Duration::from_millis(u64::try_from(ms).unwrap_or_default())
}

/// The backups among a listing's objects.
fn backups(listing: &s3::Listing) -> Vec<BackupName> {
    listing
        .objects
        .iter()
        .filter_map(|o| BackupName::parse(&o.name))
        .collect()
}

/// A directory of its own under the system temp dir, mode 0700, removed when
/// dropped: where a dump waits for its upload.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = std::env::temp_dir().join(format!("judgebot-backup-{}", uuid::Uuid::new_v4()));
        // Owner only: run on a host (`judgebot backup run`), the dump must not
        // be readable by every other user while it waits for the upload.
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(dir = %self.0.display(), error = %e, "removing the backup's scratch directory");
        }
    }
}

/// Copy `file` into `dir` as `name`, through a `.part` file so the
/// directory never holds a half-written backup under its real name.
async fn keep_local(file: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let dest = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    tokio::fs::copy(file, &part).await?;
    tokio::fs::rename(&part, &dest).await?;
    Ok(dest)
}

/// What a scheduled check found.
enum Checked {
    /// Nothing is due yet.
    NotDue {
        in_secs: u64,
        newest: Option<BackupName>,
    },
    /// A backup was attempted (or the bucket could not be read).
    Attempted { attempt: Attempt, before: Before },
}

/// A ready backup: the settings, the store's client and a `pg_dump` that
/// runs.
#[derive(Debug)]
pub struct Backup {
    settings: Settings,
    client: s3::Client,
    pg_dump: PgDump,
    http: reqwest::Client,
}

impl Backup {
    /// Check what a run needs before any is attempted: the store's client
    /// and `pg_dump`'s version, and that `BACKUP_KEEP_LOCAL` is writable.
    /// Logs what it will do.
    ///
    /// # Errors
    /// No `pg_dump`, a client that cannot be built, or a local copy
    /// directory that cannot be written.
    pub async fn start(settings: Settings) -> anyhow::Result<Self> {
        let client = s3::Client::new(&settings.store)?;
        let pg_dump = PgDump::find().await?;
        if let Some(dir) = &settings.policy.keep_local {
            probe_writable(dir).await.map_err(|e| {
                anyhow::anyhow!(
                    "{} ({}) is not a directory this process can write: {e}. In the backup \
                     container it is a path inside the container: mount a directory there, \
                     or leave it blank",
                    settings::KEEP_LOCAL_ENV,
                    dir.display()
                )
            })?;
        }
        let s = &settings;
        tracing::info!(
            endpoint = s.store.endpoint.host(),
            bucket = %s.store.bucket,
            prefix = %s.store.prefix,
            database = %format!("{}/{}", s.database.host(), s.database.name()),
            pg_dump = pg_dump.version(),
            keep_days = s.policy.keep_days.get(),
            min_bytes = s.policy.min_bytes,
            keep_local = %s.policy.keep_local.as_deref().map_or_else(|| "off".to_owned(), |p| p.display().to_string()),
            alert = s.alert.as_ref().map_or("off", alert::AlertWebhook::host),
            "backup settings"
        );
        if !s.store.endpoint.is_tls() {
            tracing::warn!(
                endpoint = s.store.endpoint.host(),
                "{} is plain http: the dump crosses the network unencrypted",
                settings::ENDPOINT_ENV
            );
        }
        Ok(Self {
            settings,
            client,
            pg_dump,
            http: alert::client(),
        })
    }

    /// Take a backup now, whatever the schedule says (`judgebot backup
    /// run`), and alert on a failure as the script does.
    pub async fn run_now(&self) -> Attempt {
        let now = Stamp::now().unix();
        let before = match self.client.list(&self.settings.store.prefix).await {
            Ok(listing) => Before::of(schedule::newest(now, &backups(&listing))),
            Err(e) => {
                tracing::warn!(error = %e, "listing the bucket before the backup; going on");
                Before::Unknown
            }
        };
        let attempt = self.take().await;
        self.tell(schedule::manual_alert(&attempt, before, now))
            .await;
        attempt
    }

    /// Dump, check, upload, prune, copy.
    async fn take(&self) -> Attempt {
        let failed = |stage| Attempt::Failed { stage, name: None };
        let s = &self.settings;
        let name = BackupName::at(Stamp::now());
        let scratch = match Scratch::new() {
            Ok(dir) => dir,
            Err(e) => {
                tracing::error!(error = %e, "creating a scratch directory for the dump");
                return failed(Stage::Dump);
            }
        };
        let file = scratch.0.join(name.to_string());
        tracing::info!(%name, "dumping the database");
        let started = Instant::now();
        let dumped = match self.pg_dump.dump(&s.database, &file, DUMP_TIMEOUT).await {
            Ok(d) => d,
            Err(DumpError::VersionMismatch { server, client }) => {
                tracing::error!(%server, %client, "pg_dump is older than the database server and refused to dump it; the image needs a newer PostgreSQL client");
                return failed(Stage::Version { server, client });
            }
            Err(e) => {
                tracing::error!(error = %e, "dumping the database failed");
                return failed(Stage::Dump);
            }
        };
        if dumped.bytes < s.policy.min_bytes {
            tracing::error!(
                bytes = dumped.bytes,
                floor = s.policy.min_bytes,
                "the dump is under {}; refusing to upload it",
                settings::MIN_BYTES_ENV
            );
            return failed(Stage::TooSmall {
                bytes: dumped.bytes,
                min: s.policy.min_bytes,
            });
        }
        tracing::info!(%name, bytes = dumped.bytes, secs = started.elapsed().as_secs(), "dump ok");
        let key = s.store.prefix.key(&name.to_string());
        if let Err(e) = self
            .client
            .put_file(&key, &file, dumped.bytes, &dumped.sha256)
            .await
        {
            tracing::error!(error = %e, "uploading the dump failed");
            return failed(Stage::Upload);
        }
        tracing::info!(bucket = %s.store.bucket, %key, "uploaded");
        // Only after a successful upload, so a failed run never costs
        // history; the one just uploaded is never a candidate.
        let mut stage = self.prune(name).await.err();
        if let Some(dir) = &s.policy.keep_local {
            match keep_local(&file, dir, &name.to_string()).await {
                Ok(dest) => tracing::info!(path = %dest.display(), "local copy kept"),
                Err(e) => {
                    tracing::error!(dir = %dir.display(), error = %e, "keeping the local copy failed");
                    stage.get_or_insert(Stage::KeepLocal);
                }
            }
        }
        drop(scratch);
        match stage {
            None => {
                tracing::info!(%name, "backup done");
                Attempt::Done {
                    name,
                    bytes: dumped.bytes,
                }
            }
            Some(stage) => Attempt::Failed {
                stage,
                name: Some(name),
            },
        }
    }

    /// Delete the backups older than `BACKUP_KEEP_DAYS`, never `kept`.
    async fn prune(&self, kept: BackupName) -> Result<(), Stage> {
        let s = &self.settings;
        let listing = self.client.list(&s.store.prefix).await.map_err(|e| {
            tracing::error!(error = %e, "listing the bucket to prune it");
            Stage::Prune
        })?;
        let old = schedule::to_prune(
            Stamp::now().unix(),
            &backups(&listing),
            s.policy.keep_days,
            kept,
        );
        let mut result = Ok(());
        for name in old {
            match self
                .client
                .delete(&s.store.prefix.key(&name.to_string()))
                .await
            {
                Ok(()) => tracing::info!(%name, keep_days = s.policy.keep_days.get(), "pruned"),
                Err(e) => {
                    tracing::error!(%name, error = %e, "pruning failed");
                    result = Err(Stage::Prune);
                }
            }
        }
        result
    }

    /// One scheduled check: list, and take a backup if one is due.
    async fn check(&self) -> Checked {
        let now = Stamp::now().unix();
        let listing = match self.client.list(&self.settings.store.prefix).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, "listing the bucket failed");
                return Checked::Attempted {
                    attempt: Attempt::Failed {
                        stage: Stage::List,
                        name: None,
                    },
                    before: Before::Unknown,
                };
            }
        };
        let found = backups(&listing);
        let newest = schedule::newest(now, &found);
        match schedule::due(now, &found, self.settings.policy.every) {
            Due::NotYet { in_secs } => Checked::NotDue { in_secs, newest },
            Due::Now => {
                tracing::info!(
                    newest = %newest.map_or_else(|| "none".to_owned(), |n| n.to_string()),
                    every_days = self.settings.policy.every.get(),
                    "a backup is due"
                );
                Checked::Attempted {
                    attempt: self.take().await,
                    before: Before::of(newest),
                }
            }
        }
    }

    /// Post `text` to the webhook, if there is one and something to say.
    async fn tell(&self, text: Option<String>) {
        if let (Some(hook), Some(text)) = (&self.settings.alert, text) {
            alert::post(&self.http, hook, "backup alert", &text).await;
        }
    }
}

/// Create `dir` and write a file in it, to know a copy will land.
async fn probe_writable(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let probe = dir.join(format!(".judgebot-backup-probe-{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&probe, b"").await?;
    tokio::fs::remove_file(&probe).await
}

/// The `backup` service: check every [`CHECK_EVERY`] (the first after
/// [`FIRST_CHECK`]) and take a backup when one is due. Never returns. Each
/// check is a task of its own, so a panic in one is a failed attempt, not
/// the end of the schedule.
pub async fn serve(backup: Backup) -> std::convert::Infallible {
    let backup = Arc::new(backup);
    let policy = &backup.settings.policy;
    tracing::info!(
        every_days = policy.every.get(),
        first_check_secs = FIRST_CHECK.as_secs(),
        "backup schedule on: a backup whenever the newest in the bucket is {} days old",
        policy.every.get()
    );
    if policy.keep_days < policy.every {
        tracing::warn!(
            keep_days = policy.keep_days.get(),
            every_days = policy.every.get(),
            "{} is shorter than {}: each backup prunes all but the newest {}",
            settings::KEEP_DAYS_ENV,
            settings::EVERY_DAYS_ENV,
            schedule::KEEP_NEWEST
        );
    }
    let mut streak = Streak::default();
    let mut retry_at: Option<Instant> = None;
    let mut told_next = false;
    tokio::time::sleep(FIRST_CHECK + jitter()).await;
    loop {
        if retry_at.is_none_or(|t| Instant::now() >= t) {
            let task = tokio::spawn({
                let backup = Arc::clone(&backup);
                async move { backup.check().await }
            });
            let checked = task.await.unwrap_or_else(|e| {
                tracing::error!(error = %e, "the backup check crashed");
                Checked::Attempted {
                    attempt: Attempt::Failed {
                        stage: Stage::Crashed,
                        name: None,
                    },
                    before: Before::Unknown,
                }
            });
            match checked {
                Checked::NotDue { in_secs, newest } => {
                    let (next, note) = streak.listed();
                    streak = next;
                    backup.tell(note).await;
                    let newest = newest.map_or_else(|| "none".to_owned(), |n| n.to_string());
                    if told_next {
                        tracing::debug!(in_secs, %newest, "no backup due");
                    } else {
                        tracing::info!(in_hours = in_secs / 3600, %newest, "next backup due");
                        told_next = true;
                    }
                }
                Checked::Attempted { attempt, before } => {
                    let (next, told) = streak.attempted(&attempt, before, Stamp::now().unix());
                    streak = next;
                    backup.tell(told).await;
                    if attempt.ok() {
                        retry_at = None;
                    } else {
                        let wait = schedule::backoff(streak.failures);
                        retry_at = Some(Instant::now() + wait);
                        tracing::warn!(
                            failures = streak.failures,
                            retry_in_mins = wait.as_secs() / 60,
                            "backup attempt failed"
                        );
                    }
                    told_next = false;
                }
            }
        }
        tokio::time::sleep(CHECK_EVERY + jitter()).await;
    }
}

/// The names directly under the prefix, sorted (backups oldest first), as
/// `scripts/backup-db.sh list` prints them; "directories" end in `/`.
///
/// # Errors
/// The listing failed.
pub async fn list(store: &Store) -> Result<Vec<String>, S3Error> {
    let listing = s3::Client::new(store)?.list(&store.prefix).await?;
    let mut names: Vec<String> = listing
        .objects
        .into_iter()
        .map(|o| o.name)
        .chain(listing.folders)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Where `fetch` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Standard output, which must not be a terminal.
    Stdout,
    /// A file, written through `<file>.part` and renamed when complete.
    File(PathBuf),
}

/// Download the object `name` under the prefix to `out`; the bytes written.
///
/// # Errors
/// `name` is not a plain object name, standard output is a terminal, the
/// download failed (`NoSuchKey` for a name that is not there) or the file
/// could not be written.
pub async fn fetch(store: &Store, name: &str, out: &Output) -> anyhow::Result<u64> {
    anyhow::ensure!(
        is_plain_segment(name),
        "{name:?} is not an object name: give one as `judgebot backup list` prints it"
    );
    let client = s3::Client::new(store)?;
    let key = store.prefix.key(name);
    match out {
        Output::Stdout => {
            anyhow::ensure!(
                !std::io::stdout().is_terminal(),
                "standard output is a terminal: redirect it to a file (`> {name}`), or name one \
                 after the object; with `docker compose run`, pass -T so no terminal is allocated"
            );
            let mut stdout = tokio::io::stdout();
            let n = client.get_to(&key, &mut stdout).await?;
            stdout.flush().await?;
            Ok(n)
        }
        Output::File(path) => {
            let mut part = path.clone().into_os_string();
            part.push(".part");
            let part = PathBuf::from(part);
            let mut file = tokio::fs::File::create(&part).await?;
            let got = client.get_to(&key, &mut file).await;
            let synced = file.sync_all().await;
            drop(file);
            let renamed = match (got, synced) {
                (Ok(n), Ok(())) => tokio::fs::rename(&part, path)
                    .await
                    .map(|()| n)
                    .map_err(anyhow::Error::from),
                (Err(e), _) => Err(e.into()),
                (_, Err(e)) => Err(e.into()),
            };
            if renamed.is_err() {
                // A broken download must not be left looking like a backup.
                if let Err(e) = tokio::fs::remove_file(&part).await {
                    tracing::warn!(path = %part.display(), error = %e, "removing the partial download");
                }
            }
            renamed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scratch_directory_is_private_and_removed() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new()?;
        let dir = scratch.0.clone();
        let mode = std::fs::metadata(&dir)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:o}");
        drop(scratch);
        assert!(!dir.exists());
        Ok(())
    }

    /// A real `pg_dump` against the test database, when one at least as new
    /// as the server is installed (CI's runner has one; a workstation may
    /// not, and the image carries its own). Skipped with a line saying why
    /// otherwise.
    #[sqlx::test(migrations = "./migrations")]
    async fn pg_dump_writes_a_gzipped_custom_archive(pool: sqlx::PgPool) -> anyhow::Result<()> {
        let pg_dump = match PgDump::find().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipped: {e}");
                return Ok(());
            }
        };
        let server: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
            .fetch_one(&pool)
            .await?;
        let server_major = u16::try_from(server / 10_000)?;
        if pg_dump.major() < server_major {
            eprintln!(
                "skipped: pg_dump {} is older than the test server ({server_major})",
                pg_dump.major()
            );
            return Ok(());
        }
        sqlx::query("INSERT INTO rules (id, subsection, body, cr_version) VALUES ('100.1', '100', 'a rule', '20260101')")
            .execute(&pool)
            .await?;
        let base = std::env::var("DATABASE_URL")?;
        let mut url = url::Url::parse(&base)?;
        url.set_path(pool.connect_options().get_database().unwrap_or_default());
        let db = settings::Database::parse(url.as_str()).map_err(anyhow::Error::msg)?;
        let scratch = Scratch::new()?;
        let file = scratch.0.join("t.dump.gz");
        let dumped = pg_dump.dump(&db, &file, Duration::from_mins(2)).await?;
        let bytes = std::fs::read(&file)?;
        assert_eq!(u64::try_from(bytes.len())?, dumped.bytes);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(bytes.as_slice()),
            &mut plain,
        )?;
        assert!(plain.starts_with(b"PGDMP"), "a custom-format archive");
        // A database that does not exist fails as a dump, not as a version.
        url.set_path("judgebot_no_such_database");
        let missing = settings::Database::parse(url.as_str()).map_err(anyhow::Error::msg)?;
        let err = pg_dump
            .dump(&missing, &file, Duration::from_mins(1))
            .await
            .err();
        assert!(matches!(err, Some(DumpError::Failed { .. })), "{err:?}");
        Ok(())
    }
}
