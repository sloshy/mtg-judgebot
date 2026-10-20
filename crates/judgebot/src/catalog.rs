//! `judgebot`'s text for people, one [`Messages`] per [`Locale`]
//! (`judge_core::locale`): the usages `--help` prints, for the roles,
//! `judgebot ingest` and `judgebot backup`.
//!
//! Everything else the binary writes is a log line, an error or machine
//! output, and stays English: logs and errors so they can be grepped and
//! pasted into bug reports, and machine output because a script reads it
//! (`ingest upgrade`'s `upgrade: <kind>` line, which `scripts/deploy.sh`
//! parses; `backup list`'s object names; `backup fetch`'s bytes). The usage
//! an error quotes is [`EN`]'s.

use std::fmt;

use judge_core::{Locale, locale::Catalogs};

/// One locale's `judgebot` text.
#[derive(Clone, Copy)]
pub struct Messages {
    /// `judgebot --help`: the roles, each by its flag.
    pub usage: &'static str,
    /// `judgebot ingest --help`.
    pub ingest_usage: &'static str,
    /// `judgebot backup --help`.
    pub backup_usage: &'static str,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("judgebot::catalog::Messages")
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

/// The `judgebot` text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    CATALOGS.get(locale)
}

/// English.
pub const EN: Messages = Messages {
    usage: CLI,
    ingest_usage: INGEST,
    backup_usage: BACKUP,
};

const CLI: &str = "\
usage: judgebot <role>...
       judgebot ingest <command> [args]   (judgebot ingest --help)
       judgebot backup <command> [args]   (judgebot backup --help)

Roles: at least one, named on the command line or, with none there, in
JUDGE_ROLES (the same flags separated by spaces).

  --discord  the Discord bot (requires DISCORD_TOKEN and JUDGE_OPERATOR_DISCORD)
  --api      POST /api/judge, the anonymous question route
  --web      the built web app, from WEB_DIST (requires its index.html)
  --mcp      the MCP transport at /mcp (requires MCP_TOKEN)
  --admin    the operator's pages and reads on ADMIN_ADDR (default
             127.0.0.1:8789; requires ADMIN_TOKEN and the built web app)
  --jobs     the scheduled data refresh (JUDGE_REFRESH_HOURS; 0 turns it off)

--api, --web and --mcp require JUDGE_OPERATOR_EMAIL and listen on API_ADDR,
where GET /api/health and GET /api/about are served whichever of them is on.
`judge-cli admin-url` prints the admin page's address with its token.
Everything else is configured through the environment; see .env.example.";

const INGEST: &str = "usage: judgebot ingest <init | cards | rules <path-or-url | latest> | aliases [yaml] | notes [yaml] | embed | reembed [--yes] [--clear] | emoji | retire | prune | migrate | refresh | upgrade>\n\
init: the whole first load (migrate, cards, rules latest, aliases, notes, embed, emoji); safe to run again.\n\
aliases, notes: with no file, the lists this binary was built with (data/*.yaml), which refresh then keeps current; a file is yours, and refresh leaves it alone.";

const BACKUP: &str = "\
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

/// The stub locale's (`judge_core::locale`, tests only): each message its
/// name in `⟦…⟧`, with the flags the usage lists (they are command-line
/// syntax, the same in every language).
pub const STUB: Messages = Messages {
    usage: "⟦usage --discord --api --web --mcp --admin --jobs⟧",
    ingest_usage: "⟦ingest_usage⟧",
    backup_usage: "⟦backup_usage⟧",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_locale_renders_every_message() {
        for l in Locale::ALL {
            let m = messages(l);
            for t in [m.usage, m.ingest_usage, m.backup_usage] {
                assert!(!t.trim().is_empty(), "{l}");
            }
        }
    }
}
