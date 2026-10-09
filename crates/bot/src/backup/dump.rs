//! `pg_dump -Fc` of the database, gzipped into a file: the same bytes
//! `scripts/backup-db.sh` produces (`pg_dump -Fc | gzip -9`), so either
//! restores with `gunzip -c <file> | pg_restore`.
//!
//! `pg_dump` connects over the network like any client, with the
//! connection in its environment ([`Database::pg_env`]), never in its
//! arguments. It must be at least the server's major version; an older one
//! refuses to dump, and that refusal is told apart from other failures
//! ([`DumpError::VersionMismatch`]) because the fix is a newer image, not a
//! retry. The image carries a `pg_dump` from the PostgreSQL project's own
//! Debian repository (`Dockerfile`), matched to the compose file's server.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use super::settings::Database;

/// The program run: `pg_dump` on `PATH`.
pub const PG_DUMP: &str = "pg_dump";
/// How long a dump may take before it is killed. The live database dumps in
/// well under a minute; an hour means a lock it is waiting on, or a hang.
pub const DUMP_TIMEOUT: Duration = Duration::from_hours(1);
/// The only variables `pg_dump` inherits from this process; the connection
/// comes from [`Database::pg_env`]. `HOME` is where libpq looks for
/// `~/.postgresql/root.crt` under `sslmode=verify-*`.
const INHERITED: [&str; 3] = ["PATH", "HOME", "TMPDIR"];
/// How much of `pg_dump`'s stderr is kept for the log: the tail.
const STDERR_KEEP: usize = 8 * 1024;

/// Why a dump failed.
#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    /// `pg_dump` could not be run at all.
    #[error("cannot run {PG_DUMP}: {0}")]
    Missing(std::io::Error),
    /// `pg_dump --version` said something this cannot read.
    #[error("{PG_DUMP} --version printed {0:?}")]
    Version(String),
    /// `pg_dump` is older than the server, and refused.
    #[error(
        "{PG_DUMP} {client} is older than the database server ({server}) and refuses to dump it; \
         the image needs a newer PostgreSQL client"
    )]
    VersionMismatch {
        /// The server's version, as `pg_dump` reported it.
        server: String,
        /// `pg_dump`'s own.
        client: String,
    },
    /// `pg_dump` exited non-zero.
    #[error("{PG_DUMP} exited with {status}: {stderr}")]
    Failed {
        /// Its exit status.
        status: std::process::ExitStatus,
        /// The tail of what it wrote to stderr.
        stderr: String,
    },
    /// It ran past [`DUMP_TIMEOUT`] and was killed.
    #[error("{PG_DUMP} ran for more than {} min and was killed", DUMP_TIMEOUT.as_secs() / 60)]
    TimedOut,
    /// The dump file could not be written.
    #[error("writing the dump: {0}")]
    Io(#[from] std::io::Error),
}

/// A `pg_dump` this process can run, and its major version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgDump {
    program: PathBuf,
    major: u16,
    version: String,
}

/// A finished dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dumped {
    /// Its size, gzipped.
    pub bytes: u64,
    /// The lowercase hex SHA-256 of the gzipped file.
    pub sha256: String,
}

/// The major version in `pg_dump --version`'s output
/// (`pg_dump (PostgreSQL) 16.15 (Debian 16.15-1.pgdg12+2)`).
#[must_use]
pub fn major_version(output: &str) -> Option<u16> {
    let rest = output.trim().strip_prefix("pg_dump (PostgreSQL) ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The two versions in `pg_dump`'s refusal to dump a newer server
/// (`pg_dump: detail: server version: 17.2; pg_dump version: 16.4`).
#[must_use]
pub fn mismatch(stderr: &str) -> Option<(String, String)> {
    if !stderr.contains("server version mismatch") {
        return None;
    }
    let after = |label: &str| {
        let start = stderr.find(label)? + label.len();
        let value: String = stderr
            .get(start..)?
            .chars()
            .take_while(|c| !matches!(c, ';' | '\n' | '\r'))
            .collect();
        Some(value.trim().to_owned()).filter(|v| !v.is_empty())
    };
    Some((
        after("server version:").unwrap_or_else(|| "newer".to_owned()),
        after("pg_dump version:").unwrap_or_else(|| "this one".to_owned()),
    ))
}

/// `bytes` as lowercase hex, as `x-amz-content-sha256` takes a hash.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write as _;
            // Writing to a String cannot fail.
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// A writer that hashes and counts what passes through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: std::io::Write> std::io::Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let written = buf.get(..n).unwrap_or_default();
        self.hash.update(written);
        self.bytes += u64::try_from(n).unwrap_or(u64::MAX);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl PgDump {
    /// Find `pg_dump` on `PATH` and read its version.
    ///
    /// # Errors
    /// It cannot be run, or its version is unreadable.
    pub async fn find() -> Result<Self, DumpError> {
        let out = tokio::process::Command::new(PG_DUMP)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(DumpError::Missing)?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        match major_version(&text) {
            Some(major) if out.status.success() => Ok(Self {
                program: PathBuf::from(PG_DUMP),
                major,
                version: text,
            }),
            _ => Err(DumpError::Version(text)),
        }
    }

    /// A stand-in program, for tests that need no PostgreSQL.
    #[cfg(test)]
    fn stand_in(program: PathBuf) -> Self {
        Self {
            program,
            major: 16,
            version: "stand-in".to_owned(),
        }
    }

    /// The major version.
    #[must_use]
    pub const fn major(&self) -> u16 {
        self.major
    }

    /// What `--version` printed.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Dump `db` in the custom format, gzipped (level 9, as the script's
    /// `gzip -9`), into `dest`, within `limit`. On any failure `dest` may
    /// hold a partial file, which the caller removes.
    ///
    /// The compression runs on the task reading `pg_dump`'s output, a
    /// chunk at a time: a backup process does nothing else meanwhile.
    ///
    /// # Errors
    /// See [`DumpError`].
    pub async fn dump(
        &self,
        db: &Database,
        dest: &Path,
        limit: Duration,
    ) -> Result<Dumped, DumpError> {
        let mut child = tokio::process::Command::new(&self.program)
            .args(["--format=custom", "--no-password"])
            // Nothing inherited but what a program needs to run: an ambient
            // PGSERVICE or PGSSLMODE must not change what is dumped, and the
            // R2 keys in this process's environment are not pg_dump's.
            .env_clear()
            .envs(
                INHERITED
                    .iter()
                    .filter_map(|k| std::env::var_os(k).map(|v| (*k, v))),
            )
            .envs(db.pg_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dump abandoned at the limit (or by a dropped future) must not
            // go on holding its snapshot on the server.
            .kill_on_drop(true)
            .spawn()
            .map_err(DumpError::Missing)?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(DumpError::Io(std::io::Error::other(
                "pg_dump's pipes were not opened",
            )));
        };
        let file = std::fs::File::create(dest)?;
        let mut gz = GzEncoder::new(
            Hashing {
                inner: std::io::BufWriter::new(file),
                hash: Sha256::new(),
                bytes: 0,
            },
            Compression::best(),
        );
        let drain = async {
            // Read all of it so pg_dump never blocks on a full pipe; keep the
            // tail, which is where an error is.
            let mut tail: Vec<u8> = Vec::new();
            let mut buf = vec![0_u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(buf.get(..n).unwrap_or_default());
                        if tail.len() > STDERR_KEEP {
                            tail.drain(..tail.len() - STDERR_KEEP);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&tail).trim().to_owned()
        };
        let run = async {
            // `stdout` is moved in, so it closes when the copy stops: a write
            // that fails (a full disk) must not leave pg_dump blocked on a
            // pipe nobody reads until the time limit. It is killed as well.
            let copy = async {
                let mut stdout = stdout;
                let mut buf = vec![0_u8; 64 * 1024];
                let copied = loop {
                    let n = match stdout.read(&mut buf).await {
                        Ok(0) => break Ok(()),
                        Ok(n) => n,
                        Err(e) => break Err(e),
                    };
                    if let Err(e) = gz.write_all(buf.get(..n).unwrap_or_default()) {
                        break Err(e);
                    }
                };
                if copied.is_err()
                    && let Err(e) = child.start_kill()
                {
                    tracing::warn!(error = %e, "stopping pg_dump after a failed write");
                }
                copied
            };
            let (copied, stderr) = tokio::join!(copy, drain);
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((copied, stderr, status))
        };
        let Ok(finished) = tokio::time::timeout(limit, run).await else {
            return Err(DumpError::TimedOut);
        };
        let (copied, stderr, status) = finished?;
        // Our own write failing comes first: pg_dump's exit then only
        // reports the pipe we closed.
        copied?;
        if !status.success() {
            return Err(match mismatch(&stderr) {
                Some((server, client)) => DumpError::VersionMismatch { server, client },
                None => DumpError::Failed { status, stderr },
            });
        }
        if !stderr.is_empty() {
            tracing::warn!(stderr = %stderr, "pg_dump succeeded with warnings");
        }
        let mut hashing = gz.finish()?;
        hashing.flush()?;
        hashing
            .inner
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()?;
        let sha256 = hex(&hashing.hash.finalize());
        Ok(Dumped {
            bytes: hashing.bytes,
            sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding an executable `pg_dump` stand-in running `body`.
    fn stand_in(body: &str) -> std::io::Result<(PathBuf, PgDump)> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("judgebot-dump-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let program = dir.join("pg_dump");
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n"))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        Ok((dir, PgDump::stand_in(program)))
    }

    fn db() -> Result<Database, String> {
        Database::parse("postgres://judgebot:pw@db.local:5432/judgebot")
    }

    /// A full disk fails the dump at once with the write error, not after
    /// the time limit with `pg_dump` blocked on its pipe.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_failed_write_stops_the_dump_and_says_why() -> Result<(), Box<dyn std::error::Error>>
    {
        let (dir, pg_dump) = stand_in("exec head -c 50000000 /dev/urandom")?;
        let started = std::time::Instant::now();
        let r = pg_dump
            .dump(&db()?, Path::new("/dev/full"), Duration::from_secs(60))
            .await;
        std::fs::remove_dir_all(dir)?;
        assert!(matches!(r, Err(DumpError::Io(_))), "{r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        Ok(())
    }

    /// `pg_dump` sees the connection and a few basics, nothing else of this
    /// process's environment (cargo sets CARGO_* for every test).
    #[tokio::test]
    async fn pg_dump_inherits_only_what_it_needs() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, pg_dump) = stand_in("exec env")?;
        let out = dir.join("env.gz");
        pg_dump.dump(&db()?, &out, Duration::from_secs(30)).await?;
        let mut env = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(std::fs::File::open(&out)?),
            &mut env,
        )?;
        std::fs::remove_dir_all(dir)?;
        assert!(env.lines().any(|l| l == "PGDATABASE=judgebot"), "{env}");
        assert!(env.lines().any(|l| l == "PGPASSWORD=pw"), "{env}");
        assert!(!env.contains("CARGO"), "{env}");
        let names: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
        assert!(
            names.iter().all(|n| n.starts_with("PG")
                || INHERITED.contains(n)
                || *n == "PWD"
                || *n == "SHLVL"
                || *n == "_"),
            "{names:?}"
        );
        Ok(())
    }

    #[test]
    fn the_major_version_is_read_from_any_build() {
        for (out, major) in [
            (
                "pg_dump (PostgreSQL) 16.15 (Debian 16.15-1.pgdg12+2)\n",
                Some(16),
            ),
            ("pg_dump (PostgreSQL) 17.2", Some(17)),
            ("pg_dump (PostgreSQL) 18beta1", Some(18)),
            ("pg_dump (PostgreSQL) 9.6.24", Some(9)),
            ("psql (PostgreSQL) 16.1", None),
            ("", None),
        ] {
            assert_eq!(major_version(out), major, "{out}");
        }
    }

    #[test]
    fn a_version_refusal_is_told_apart() {
        let refusal = "pg_dump: error: aborting because of server version mismatch\n\
                       pg_dump: detail: server version: 17.2; pg_dump version: 16.15 (Debian 16.15-1.pgdg12+2)";
        assert_eq!(
            mismatch(refusal),
            Some((
                "17.2".to_owned(),
                "16.15 (Debian 16.15-1.pgdg12+2)".to_owned()
            ))
        );
        assert_eq!(
            mismatch(
                "pg_dump: error: connection to server at \"db\" failed: FATAL: password authentication failed"
            ),
            None
        );
    }
}
