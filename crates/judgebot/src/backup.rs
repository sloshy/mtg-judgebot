//! `judgebot backup` — the database backup to Cloudflare R2: argument
//! parsing over `judge_bot::backup`.
//!
//! ```text
//! judgebot backup run                 # dump, upload, prune now (what scripts/backup-db.sh does)
//! judgebot backup list                # the objects under the prefix, oldest first
//! judgebot backup fetch <name> [file] # download one to <file>, or to standard output
//! judgebot backup serve               # the `backup` compose service: a backup whenever the
//!                                     #   newest is BACKUP_EVERY_DAYS old
//! ```
//!
//! The settings are the ones `scripts/backup-db.sh` reads from `.env.deploy`
//! (`R2_*`, `BACKUP_*`, `JUDGE_ALERT_WEBHOOK`), plus `DATABASE_URL` for `run`
//! and `serve`. A `.env.deploy`, then a `.env`, in the working directory are
//! loaded first, and the process environment wins: the compose service has
//! neither file and gets `.env.deploy` as its `env_file` and `DATABASE_URL`
//! from the compose file, never `.env`. Logs go to standard error, so
//! `list` and `fetch` can write to standard output.

use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;
use judge_bot::backup::{
    self, Backup, Output,
    schedule::Attempt,
    settings::{Settings, Store},
};

/// What to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// One backup now.
    Run,
    /// The objects under the prefix.
    List,
    /// One object, to a file or standard output.
    Fetch {
        /// Its name, as `list` prints it.
        name: String,
        /// Where it goes.
        out: Output,
    },
    /// The schedule, forever.
    Serve,
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Print [`USAGE`].
    Help,
    /// Do this.
    Run(Command),
}

/// The usage `judgebot backup --help` prints.
pub const USAGE: &str = "\
usage: judgebot backup <run | list | fetch <name> [file] | serve>

  run          dump the database, upload it, prune old backups (scripts/backup-db.sh)
  list         the objects under the prefix, oldest first
  fetch        download one backup to <file>, or to standard output (not a terminal):
               docker compose run --rm --no-deps -T backup fetch <name> > <name>
  serve        the backup service: a backup whenever the newest in the bucket is
               BACKUP_EVERY_DAYS old (default 7), checked hourly

Settings come from .env.deploy (R2_ENDPOINT, R2_BUCKET, R2_ACCESS_KEY_ID,
R2_SECRET_ACCESS_KEY, BACKUP_PREFIX, BACKUP_KEEP_DAYS, BACKUP_MIN_BYTES,
BACKUP_KEEP_LOCAL, BACKUP_EVERY_DAYS, JUDGE_ALERT_WEBHOOK) and, for run and
serve, DATABASE_URL. See .env.deploy.example.";

/// Parse the arguments after `backup`.
///
/// # Errors
/// An unknown command, a missing name, or an extra argument, with the usage.
pub fn parse(args: Vec<OsString>) -> Result<Launch> {
    let mut args = args.into_iter().map(|a| a.to_string_lossy().into_owned());
    let bad = |what: String| anyhow::anyhow!("{what}\n\n{USAGE}");
    let cmd = match args.next().as_deref() {
        None => return Err(bad("judgebot backup needs a command".to_owned())),
        Some("-h" | "--help") => return Ok(Launch::Help),
        Some("run") => Command::Run,
        Some("list") => Command::List,
        Some("serve") => Command::Serve,
        Some("fetch") => {
            let name = args.next().ok_or_else(|| {
                bad("fetch needs the object's name (judgebot backup list)".to_owned())
            })?;
            let out = match args.next() {
                None => Output::Stdout,
                Some(f) if f == "-" => Output::Stdout,
                Some(f) => Output::File(PathBuf::from(f)),
            };
            Command::Fetch { name, out }
        }
        Some(other) => return Err(bad(format!("unknown backup command {other:?}"))),
    };
    if let Some(extra) = args.next() {
        return Err(bad(format!("unexpected argument {extra:?}")));
    }
    Ok(Launch::Run(cmd))
}

/// `.env.deploy`, then `.env` (neither overrides what is already set), then
/// logging to standard error.
pub fn setup() -> Result<()> {
    for file in [".env.deploy", ".env"] {
        match dotenvy::from_filename(file) {
            Ok(_) | Err(dotenvy::Error::Io(_)) => {}
            Err(e) => return Err(anyhow::Error::from(e).context(format!("load {file}"))),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    Ok(())
}

/// Run `cmd`.
///
/// # Errors
/// Unusable settings, a failed backup (non-zero exit, as the script), or a
/// failed listing or download.
pub async fn run(cmd: Command) -> Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    match cmd {
        Command::List => {
            let store = Store::from_env(env)?;
            for name in backup::list(&store).await? {
                println!("{name}");
            }
            Ok(())
        }
        Command::Fetch { name, out } => {
            let store = Store::from_env(env)?;
            let bytes = backup::fetch(&store, &name, &out).await?;
            match out {
                Output::File(path) => tracing::info!(path = %path.display(), bytes, "fetched"),
                Output::Stdout => tracing::info!(bytes, "fetched to standard output"),
            }
            Ok(())
        }
        Command::Run => {
            let backup = Backup::start(Settings::from_env(env)?).await?;
            match backup.run_now().await {
                Attempt::Done { .. } => Ok(()),
                Attempt::Failed { stage, name } => anyhow::bail!(
                    "the backup failed while {}{}; the log above has the error",
                    stage.clause(),
                    name.map(|n| format!(" (after {n} was uploaded)"))
                        .unwrap_or_default()
                ),
            }
        }
        Command::Serve => {
            let backup = Backup::start(Settings::from_env(env)?).await?;
            match backup::serve(backup).await {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Launch> {
        parse(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn the_commands_parse() {
        for (args, want) in [
            (&["run"][..], Command::Run),
            (&["list"], Command::List),
            (&["serve"], Command::Serve),
            (
                &["fetch", "judgebot-20261009T041500Z.dump.gz"],
                Command::Fetch {
                    name: "judgebot-20261009T041500Z.dump.gz".to_owned(),
                    out: Output::Stdout,
                },
            ),
            (
                &["fetch", "x.dump.gz", "-"],
                Command::Fetch {
                    name: "x.dump.gz".to_owned(),
                    out: Output::Stdout,
                },
            ),
            (
                &["fetch", "x.dump.gz", "/tmp/x.dump.gz"],
                Command::Fetch {
                    name: "x.dump.gz".to_owned(),
                    out: Output::File(PathBuf::from("/tmp/x.dump.gz")),
                },
            ),
        ] {
            assert_eq!(p(args).ok(), Some(Launch::Run(want)), "{args:?}");
        }
        assert_eq!(p(&["--help"]).ok(), Some(Launch::Help));
        for bad in [
            &[][..],
            &["backup"],
            &["fetch"],
            &["list", "extra"],
            &["fetch", "a", "b", "c"],
        ] {
            let r = p(bad);
            assert!(
                r.as_ref()
                    .is_err_and(|e| e.to_string().contains("usage: judgebot backup")),
                "{bad:?}: {r:?}"
            );
        }
    }
}
