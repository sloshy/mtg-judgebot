//! The backup's settings, read from the environment once at start and held
//! as types: a variable that is set but malformed is an error naming it, and
//! every such error is reported at once.
//!
//! The variables are the ones `scripts/backup-db.sh` reads from
//! `.env.deploy`, with the same defaults, so one file serves both the script
//! and the `backup` compose service. Blank means unset, as `${VAR:-default}`
//! reads it in the script. `BACKUP_EVERY_DAYS` is the service's alone (cron
//! is the script's schedule).

use std::{fmt, num::NonZeroU16, path::PathBuf};

use judge_llm::ApiKey;
use nonempty::NonEmpty;

use crate::alert::{ALERT_WEBHOOK_ENV, AlertWebhook};

/// `R2_ENDPOINT`: `https://<account-id>.r2.cloudflarestorage.com`.
pub const ENDPOINT_ENV: &str = "R2_ENDPOINT";
/// `R2_BUCKET`.
pub const BUCKET_ENV: &str = "R2_BUCKET";
/// `R2_ACCESS_KEY_ID`.
pub const ACCESS_KEY_ID_ENV: &str = "R2_ACCESS_KEY_ID";
/// `R2_SECRET_ACCESS_KEY`.
pub const SECRET_ACCESS_KEY_ENV: &str = "R2_SECRET_ACCESS_KEY";
/// `BACKUP_PREFIX`: the "directory" in the bucket.
pub const PREFIX_ENV: &str = "BACKUP_PREFIX";
/// `BACKUP_KEEP_DAYS`: backups older than this are pruned after an upload.
pub const KEEP_DAYS_ENV: &str = "BACKUP_KEEP_DAYS";
/// `BACKUP_MIN_BYTES`: a smaller dump is refused, never uploaded.
pub const MIN_BYTES_ENV: &str = "BACKUP_MIN_BYTES";
/// `BACKUP_KEEP_LOCAL`: a directory to copy each uploaded dump into.
pub const KEEP_LOCAL_ENV: &str = "BACKUP_KEEP_LOCAL";
/// `BACKUP_EVERY_DAYS`: how old the newest backup may get before the service
/// takes another.
pub const EVERY_DAYS_ENV: &str = "BACKUP_EVERY_DAYS";
/// `DATABASE_URL`: the database `pg_dump` reads.
pub const DATABASE_URL_ENV: &str = "DATABASE_URL";

/// The script's default prefix.
pub const DEFAULT_PREFIX: &str = "db";
/// The script's default retention.
pub const DEFAULT_KEEP_DAYS: Days = Days(NonZeroU16::MIN.saturating_add(59));
/// The script's default size floor: a stub dump is far smaller, the live
/// database's tens of MB.
pub const DEFAULT_MIN_BYTES: u64 = 20_000_000;
/// The default interval: weekly, as the script's cron line.
pub const DEFAULT_EVERY_DAYS: Days = Days(NonZeroU16::MIN.saturating_add(6));
/// The longest `BACKUP_KEEP_DAYS` (ten years).
pub const MAX_KEEP_DAYS: u16 = 3650;
/// The longest `BACKUP_EVERY_DAYS`.
pub const MAX_EVERY_DAYS: u16 = 365;

const _: () = assert!(DEFAULT_KEEP_DAYS.get() == 60 && DEFAULT_EVERY_DAYS.get() == 7);

/// One variable that is missing or malformed, and what it should be. The
/// value itself is never quoted: most of these are credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The variable.
    pub var: &'static str,
    /// What it must be.
    pub expected: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.var, self.expected)
    }
}

/// Every problem the environment has, so one start names them all.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the backup settings are not usable (.env.deploy, or the environment):\n  {}",
    self.0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n  ")
)]
pub struct Problems(pub NonEmpty<Problem>);

fn problem(var: &'static str, expected: impl Into<String>) -> Problem {
    Problem {
        var,
        expected: expected.into(),
    }
}

/// The S3 endpoint: an absolute `https` (or, for a local stand-in, `http`)
/// URL with a host and nothing after it.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint(url::Url);

impl Endpoint {
    /// Parse `R2_ENDPOINT`.
    ///
    /// # Errors
    /// What the value must be, when it is not that.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let expected = "an https:// URL with nothing after the host, such as \
                        https://<account-id>.r2.cloudflarestorage.com";
        let url = url::Url::parse(raw.trim()).map_err(|_| expected.to_owned())?;
        let plain = matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some_and(|h| !h.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/";
        if plain {
            Ok(Self(url))
        } else {
            Err(expected.to_owned())
        }
    }

    /// The URL.
    #[must_use]
    pub fn url(&self) -> &url::Url {
        &self.0
    }

    /// The host, for log lines.
    #[must_use]
    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }

    /// Whether the connection is encrypted. Plain `http` is accepted for a
    /// local stand-in (`MinIO` on the compose network) and warned about.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.0.scheme() == "https"
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Endpoint({})", self.0)
    }
}

/// A bucket name as S3 and R2 take it: 3 to 63 lowercase letters, digits,
/// hyphens and dots, starting and ending with a letter or digit. It is a
/// path segment of every request, so nothing else may reach the URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bucket(String);

impl Bucket {
    /// Parse `R2_BUCKET`.
    ///
    /// # Errors
    /// What the value must be, when it is not that.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let b = raw.trim();
        let edge =
            |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let ok = (3..=63).contains(&b.len())
            && b.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
            && edge(b.chars().next())
            && edge(b.chars().next_back());
        if ok {
            Ok(Self(b.to_owned()))
        } else {
            Err("a bucket name: 3 to 63 lowercase letters, digits, hyphens and dots".to_owned())
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Is `s` one segment of an object key this module writes or reads: ASCII
/// letters, digits, `.`, `_` and `-`, and not `.` or `..`? Such a segment
/// needs no percent-encoding, so the URL sent and the one signed agree.
#[must_use]
pub fn is_plain_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The key prefix the backups live under, without a leading or trailing
/// slash: one or more [plain segments](is_plain_segment) joined by `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefix(String);

impl Prefix {
    /// Parse `BACKUP_PREFIX`; blank is [`DEFAULT_PREFIX`], as in the script.
    /// Slashes around it are dropped (`db/` is `db`).
    ///
    /// # Errors
    /// What the value must be, when it is not that.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let p = raw.map(str::trim).unwrap_or_default().trim_matches('/');
        if p.is_empty() {
            return Ok(Self(DEFAULT_PREFIX.to_owned()));
        }
        if p.split('/').all(is_plain_segment) {
            Ok(Self(p.to_owned()))
        } else {
            Err(
                "a key prefix such as db or backups/judgebot: letters, digits, '.', '_' and '-' \
                 between single slashes"
                    .to_owned(),
            )
        }
    }

    /// The prefix.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key of the object `name` (a [plain segment](is_plain_segment))
    /// under this prefix.
    #[must_use]
    pub fn key(&self, name: &str) -> String {
        format!("{}/{name}", self.0)
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A whole number of days, at least one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Days(NonZeroU16);

impl Days {
    /// `days` if it is from 1 to `max`.
    #[must_use]
    pub const fn new(days: u16, max: u16) -> Option<Self> {
        match NonZeroU16::new(days) {
            Some(d) if days <= max => Some(Self(d)),
            _ => None,
        }
    }

    /// The number of days.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    /// The span in seconds.
    #[must_use]
    pub fn secs(self) -> i64 {
        i64::from(self.get()) * 86_400
    }

    fn parse(raw: Option<&str>, default: Self, max: u16) -> Result<Self, String> {
        match raw.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(default),
            Some(v) => v
                .parse::<u16>()
                .ok()
                .and_then(|d| Self::new(d, max))
                .ok_or_else(|| format!("a whole number of days from 1 to {max}")),
        }
    }
}

/// Where the backups are stored, and the keys to it: all that `list` and
/// `fetch` need.
#[derive(Clone, Debug)]
pub struct Store {
    /// `R2_ENDPOINT`.
    pub endpoint: Endpoint,
    /// `R2_BUCKET`.
    pub bucket: Bucket,
    /// `BACKUP_PREFIX`.
    pub prefix: Prefix,
    /// `R2_ACCESS_KEY_ID`. Not secret by itself, and shown in R2's dashboard
    /// beside the token; never logged all the same.
    pub access_key_id: ApiKey,
    /// `R2_SECRET_ACCESS_KEY`.
    pub secret_access_key: ApiKey,
}

/// What a backup run does besides storing the dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// `BACKUP_KEEP_DAYS`.
    pub keep_days: Days,
    /// `BACKUP_MIN_BYTES`.
    pub min_bytes: u64,
    /// `BACKUP_KEEP_LOCAL`, as the process sees the filesystem (in the
    /// container, a path inside it).
    pub keep_local: Option<PathBuf>,
    /// `BACKUP_EVERY_DAYS` (the service's schedule; the script's is cron).
    pub every: Days,
}

/// `sslmode`, as libpq takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslMode {
    /// `disable`.
    Disable,
    /// `allow`.
    Allow,
    /// `prefer`.
    Prefer,
    /// `require`.
    Require,
    /// `verify-ca`.
    VerifyCa,
    /// `verify-full`.
    VerifyFull,
}

impl SslMode {
    const ALL: [Self; 6] = [
        Self::Disable,
        Self::Allow,
        Self::Prefer,
        Self::Require,
        Self::VerifyCa,
        Self::VerifyFull,
    ];

    /// The libpq spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }
}

/// The database `pg_dump` connects to, from `DATABASE_URL`. `pg_dump` gets
/// it as `PG*` variables in its environment rather than as an argument, so
/// the password is never in a process list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Database {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<ApiKey>,
    dbname: String,
    sslmode: Option<SslMode>,
    sslrootcert: Option<String>,
}

impl Database {
    /// Parse a `postgres://` URL: user, password, host, port and database,
    /// and the query parameters `host` (a socket directory), `sslmode` and
    /// `sslrootcert`.
    ///
    /// # Errors
    /// What the value must be, when it is not that, naming a query parameter
    /// this does not hand on (rather than dumping with it dropped).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let url = url::Url::parse(raw.trim())
            .map_err(|_| "a postgres:// URL, as judgebot reads it".to_owned())?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err("a postgres:// URL, as judgebot reads it".to_owned());
        }
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(std::borrow::Cow::into_owned)
                .map_err(|_| "a URL whose user, password and database are UTF-8".to_owned())
        };
        let mut db = Self {
            host: url.host_str().filter(|h| !h.is_empty()).map(|h| {
                // `[::1]` in a URL is `::1` to libpq.
                h.trim_start_matches('[').trim_end_matches(']').to_owned()
            }),
            port: url.port(),
            user: Some(decode(url.username())?).filter(|u| !u.is_empty()),
            password: url.password().map(decode).transpose()?.map(ApiKey::from),
            dbname: decode(url.path().trim_start_matches('/'))?,
            sslmode: None,
            sslrootcert: None,
        };
        for (k, v) in url.query_pairs() {
            match &*k {
                "host" => db.host = Some(v.into_owned()),
                "sslmode" => {
                    db.sslmode = Some(
                        SslMode::ALL
                            .into_iter()
                            .find(|m| m.as_str() == v)
                            .ok_or_else(|| format!("sslmode={v} is not a libpq sslmode"))?,
                    );
                }
                "sslrootcert" => db.sslrootcert = Some(v.into_owned()),
                other => {
                    return Err(format!(
                        "a URL whose query parameters are host, sslmode or sslrootcert \
                         (pg_dump would not be given `{other}`)"
                    ));
                }
            }
        }
        if db.dbname.is_empty() || db.dbname.contains('/') {
            return Err("a postgres:// URL naming the database (…/judgebot)".to_owned());
        }
        if db.host.is_none() {
            return Err("a postgres:// URL naming the host (…@db:5432/…)".to_owned());
        }
        Ok(db)
    }

    /// The database's name, for log lines.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.dbname
    }

    /// The host, for log lines.
    #[must_use]
    pub fn host(&self) -> &str {
        self.host.as_deref().unwrap_or_default()
    }

    /// The libpq environment `pg_dump` is started with. Carries the
    /// password: hand it to a child process, never to a log line.
    #[must_use]
    pub fn pg_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![("PGDATABASE", self.dbname.clone())];
        let mut put = |k, v: Option<String>| {
            if let Some(v) = v {
                env.push((k, v));
            }
        };
        put("PGHOST", self.host.clone());
        put("PGPORT", self.port.map(|p| p.to_string()));
        put("PGUSER", self.user.clone());
        put(
            "PGPASSWORD",
            self.password.as_ref().map(|p| p.expose().to_owned()),
        );
        put("PGSSLMODE", self.sslmode.map(|m| m.as_str().to_owned()));
        put("PGSSLROOTCERT", self.sslrootcert.clone());
        put("PGAPPNAME", Some("judgebot backup".to_owned()));
        put("PGCONNECT_TIMEOUT", Some("30".to_owned()));
        env
    }
}

/// Everything a backup run and the schedule need.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Where the backups go.
    pub store: Store,
    /// What a run does besides storing the dump.
    pub policy: Policy,
    /// `JUDGE_ALERT_WEBHOOK`, where a failure and a recovery are reported.
    pub alert: Option<AlertWebhook>,
    /// `DATABASE_URL`.
    pub database: Database,
}

/// Collects every problem while the parts are parsed.
#[derive(Default)]
struct Collect(Vec<Problem>);

impl Collect {
    fn take<T>(&mut self, var: &'static str, r: Result<T, String>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(expected) => {
                self.0.push(problem(var, expected));
                None
            }
        }
    }

    fn finish<T>(self, value: Option<T>) -> Result<T, Problems> {
        match (NonEmpty::from_vec(self.0), value) {
            (None, Some(v)) => Ok(v),
            (Some(problems), _) => Err(Problems(problems)),
            // Every `None` came with a problem; this cannot be reached.
            (None, None) => Err(Problems(NonEmpty::new(problem(
                "?",
                "an internal error in the settings parser",
            )))),
        }
    }
}

/// `env(var)`, blank meaning unset.
fn get(env: &impl Fn(&str) -> Option<String>, var: &str) -> Option<String> {
    env(var).filter(|v| !v.trim().is_empty())
}

fn required(env: &impl Fn(&str) -> Option<String>, var: &str) -> Result<String, String> {
    get(env, var).ok_or_else(|| "is not set".to_owned())
}

fn store_parts(env: &impl Fn(&str) -> Option<String>, c: &mut Collect) -> Option<Store> {
    let endpoint = c.take(
        ENDPOINT_ENV,
        required(env, ENDPOINT_ENV).and_then(|v| Endpoint::parse(&v)),
    );
    let bucket = c.take(
        BUCKET_ENV,
        required(env, BUCKET_ENV).and_then(|v| Bucket::parse(&v)),
    );
    let prefix = c.take(PREFIX_ENV, Prefix::parse(get(env, PREFIX_ENV).as_deref()));
    let key = c.take(ACCESS_KEY_ID_ENV, required(env, ACCESS_KEY_ID_ENV));
    let secret = c.take(SECRET_ACCESS_KEY_ENV, required(env, SECRET_ACCESS_KEY_ENV));
    Some(Store {
        endpoint: endpoint?,
        bucket: bucket?,
        prefix: prefix?,
        access_key_id: ApiKey::from(key?.trim()),
        secret_access_key: ApiKey::from(secret?.trim()),
    })
}

impl Store {
    /// The store's settings from `env`: what `list` and `fetch` read.
    ///
    /// # Errors
    /// Every variable that is missing or malformed.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, Problems> {
        let mut c = Collect::default();
        let store = store_parts(&env, &mut c);
        c.finish(store)
    }
}

impl Settings {
    /// Every setting a backup run reads, from `env`.
    ///
    /// # Errors
    /// Every variable that is missing or malformed, at once.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, Problems> {
        let mut c = Collect::default();
        let store = store_parts(&env, &mut c);
        let keep_days = c.take(
            KEEP_DAYS_ENV,
            Days::parse(
                get(&env, KEEP_DAYS_ENV).as_deref(),
                DEFAULT_KEEP_DAYS,
                MAX_KEEP_DAYS,
            ),
        );
        let every = c.take(
            EVERY_DAYS_ENV,
            Days::parse(
                get(&env, EVERY_DAYS_ENV).as_deref(),
                DEFAULT_EVERY_DAYS,
                MAX_EVERY_DAYS,
            ),
        );
        let min_bytes = c.take(
            MIN_BYTES_ENV,
            get(&env, MIN_BYTES_ENV).map_or(Ok(DEFAULT_MIN_BYTES), |v| {
                v.trim()
                    .parse::<u64>()
                    .map_err(|_| "a whole number of bytes".to_owned())
            }),
        );
        let keep_local = c.take(
            KEEP_LOCAL_ENV,
            get(&env, KEEP_LOCAL_ENV)
                .map(|v| {
                    let p = PathBuf::from(v.trim());
                    if p.is_absolute() {
                        Ok(p)
                    } else {
                        Err("an absolute directory path".to_owned())
                    }
                })
                .transpose(),
        );
        let alert = c.take(
            ALERT_WEBHOOK_ENV,
            get(&env, ALERT_WEBHOOK_ENV)
                .map(|v| AlertWebhook::parse(&v))
                .transpose(),
        );
        let database = c.take(
            DATABASE_URL_ENV,
            required(&env, DATABASE_URL_ENV).and_then(|v| Database::parse(&v)),
        );
        let settings = (|| {
            Some(Self {
                store: store?,
                policy: Policy {
                    keep_days: keep_days?,
                    min_bytes: min_bytes?,
                    keep_local: keep_local?,
                    every: every?,
                },
                alert: alert?,
                database: database?,
            })
        })();
        c.finish(settings)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const FULL: &[(&str, &str)] = &[
        (ENDPOINT_ENV, "https://abc123.r2.cloudflarestorage.com"),
        (BUCKET_ENV, "judgebot-backups"),
        (ACCESS_KEY_ID_ENV, "AKIDEXAMPLE"),
        (SECRET_ACCESS_KEY_ENV, "very-secret"),
        (
            DATABASE_URL_ENV,
            "postgres://judgebot:p%40ss@db:5432/judgebot",
        ),
    ];

    #[test]
    fn the_scripts_defaults_apply_when_unset_or_blank() -> Result<(), Problems> {
        let s = Settings::from_env(env(FULL))?;
        assert_eq!(s.store.prefix.as_str(), "db");
        assert_eq!(s.policy.keep_days.get(), 60);
        assert_eq!(s.policy.every.get(), 7);
        assert_eq!(s.policy.min_bytes, 20_000_000);
        assert_eq!(s.policy.keep_local, None);
        assert_eq!(s.alert, None);

        let mut blank = FULL.to_vec();
        blank.extend([
            (PREFIX_ENV, " "),
            (KEEP_DAYS_ENV, ""),
            (MIN_BYTES_ENV, ""),
            (EVERY_DAYS_ENV, ""),
            (KEEP_LOCAL_ENV, ""),
            (ALERT_WEBHOOK_ENV, ""),
        ]);
        let b = Settings::from_env(env(&blank))?;
        assert_eq!(b.store.prefix, s.store.prefix);
        assert_eq!(b.policy, s.policy);
        Ok(())
    }

    #[test]
    fn set_values_are_read() -> Result<(), Problems> {
        let mut vars = FULL.to_vec();
        vars.extend([
            (PREFIX_ENV, "/backups/judgebot/"),
            (KEEP_DAYS_ENV, "90"),
            (MIN_BYTES_ENV, "1000"),
            (EVERY_DAYS_ENV, "1"),
            (KEEP_LOCAL_ENV, "/var/backups/judgebot"),
            (ALERT_WEBHOOK_ENV, "https://discord.com/api/webhooks/1/tok"),
        ]);
        let s = Settings::from_env(env(&vars))?;
        assert_eq!(s.store.prefix.as_str(), "backups/judgebot");
        assert_eq!(
            s.store.prefix.key("x.dump.gz"),
            "backups/judgebot/x.dump.gz"
        );
        assert_eq!(s.policy.keep_days.get(), 90);
        assert_eq!(s.policy.min_bytes, 1000);
        assert_eq!(s.policy.every.get(), 1);
        assert_eq!(
            s.policy.keep_local.as_deref(),
            Some(std::path::Path::new("/var/backups/judgebot"))
        );
        assert!(s.alert.is_some());
        Ok(())
    }

    #[test]
    fn every_problem_is_named_at_once_and_no_value_is_quoted() -> Result<(), &'static str> {
        let r = Settings::from_env(env(&[
            (ENDPOINT_ENV, "https://x.example/very-secret-path"),
            (BUCKET_ENV, "Not_A_Bucket"),
            (SECRET_ACCESS_KEY_ENV, "very-secret"),
            (KEEP_DAYS_ENV, "0"),
            (EVERY_DAYS_ENV, "400"),
            (MIN_BYTES_ENV, "-1"),
            (PREFIX_ENV, "db/../etc"),
            (KEEP_LOCAL_ENV, "relative/dir"),
            (ALERT_WEBHOOK_ENV, "http://hooks.example/very-secret"),
            (
                DATABASE_URL_ENV,
                "postgres://u:very-secret@db/judgebot?pool_size=3",
            ),
        ]));
        let problems = r.err().ok_or("accepted")?;
        let vars: Vec<&str> = problems.0.iter().map(|p| p.var).collect();
        assert_eq!(
            vars,
            [
                ENDPOINT_ENV,
                BUCKET_ENV,
                PREFIX_ENV,
                ACCESS_KEY_ID_ENV,
                KEEP_DAYS_ENV,
                EVERY_DAYS_ENV,
                MIN_BYTES_ENV,
                KEEP_LOCAL_ENV,
                ALERT_WEBHOOK_ENV,
                DATABASE_URL_ENV,
            ]
        );
        let text = problems.to_string();
        assert!(!text.contains("very-secret"), "{text}");
        assert!(text.contains("pool_size"), "names the parameter: {text}");
        Ok(())
    }

    #[test]
    fn list_and_fetch_need_only_the_store() -> Result<(), Problems> {
        let s = Store::from_env(env(&FULL.iter().copied().take(4).collect::<Vec<_>>()))?;
        assert_eq!(s.bucket.as_str(), "judgebot-backups");
        let shown = format!("{s:?}");
        assert!(
            !shown.contains("very-secret") && !shown.contains("AKIDEXAMPLE"),
            "{shown}"
        );
        Ok(())
    }

    #[test]
    fn endpoints_are_bare_hosts() {
        for ok in [
            "https://abc.r2.cloudflarestorage.com",
            "https://abc.eu.r2.cloudflarestorage.com/",
            "http://minio:9000",
        ] {
            assert!(Endpoint::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "abc.r2.cloudflarestorage.com",
            "ftp://x.example",
            "https://x.example/bucket",
            "https://key:secret@x.example",
            "https://x.example?x=1",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
        assert!(Endpoint::parse("http://minio:9000").is_ok_and(|e| !e.is_tls()));
    }

    #[test]
    fn buckets_and_prefixes_are_safe_path_segments() {
        for ok in ["abc", "judgebot-backups", "a.b.c", "0db"] {
            assert!(Bucket::parse(ok).is_ok(), "{ok}");
        }
        for bad in ["ab", "-abc", "abc-", "ABC", "a_b", "a/b", &"a".repeat(64)] {
            assert!(Bucket::parse(bad).is_err(), "{bad}");
        }
        for (raw, prefix) in [
            (None, "db"),
            (Some("db/"), "db"),
            (Some("a/b_c/d-1.2"), "a/b_c/d-1.2"),
        ] {
            assert_eq!(Prefix::parse(raw).map(|p| p.0), Ok(prefix.to_owned()));
        }
        for bad in ["a//b", "..", "a/./b", "a b", "a%2Fb", "ü"] {
            assert!(Prefix::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_database_url_becomes_libpq_variables() -> Result<(), String> {
        let db = Database::parse(
            "postgresql://judge%20bot:p%40ss%2Fw@db.local:5433/judgebot?sslmode=verify-full&sslrootcert=/ca.pem",
        )?;
        let env: HashMap<_, _> = db.pg_env().into_iter().collect();
        for (k, v) in [
            ("PGHOST", "db.local"),
            ("PGPORT", "5433"),
            ("PGUSER", "judge bot"),
            ("PGPASSWORD", "p@ss/w"),
            ("PGDATABASE", "judgebot"),
            ("PGSSLMODE", "verify-full"),
            ("PGSSLROOTCERT", "/ca.pem"),
        ] {
            assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
        }
        assert!(!format!("{db:?}").contains("p@ss"));

        let socket = Database::parse("postgres:///judgebot?host=/var/run/postgresql")?;
        assert_eq!(socket.host(), "/var/run/postgresql");
        let bare = Database::parse("postgres://db/judgebot")?;
        let env: HashMap<_, _> = bare.pg_env().into_iter().collect();
        assert!(!env.contains_key("PGPASSWORD") && !env.contains_key("PGPORT"));

        for bad in [
            "mysql://db/judgebot",
            "postgres://db",
            "postgres://db/",
            "postgres:///judgebot",
            "postgres://db/judgebot?sslmode=sometimes",
            "postgres://db/judgebot?statement-cache-capacity=0",
        ] {
            assert!(Database::parse(bad).is_err(), "{bad}");
        }
        Ok(())
    }
}
