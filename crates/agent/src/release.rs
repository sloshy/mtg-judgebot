//! A client tool's release against the instance's (`judge_core::release`):
//! what `judge-cli` and a stdio `judge-mcp` tell their user when the
//! database was migrated by another release.
//!
//! A newer release of the same major is a notice, and the tool goes on
//! working. A different major, another build of a new major's pre-release,
//! or a database that
//! lacks a migration the tool carries stops every operation with a message
//! naming what to update. `judgebot --mcp` is part of the instance and does
//! no check.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use judge_bot::db::migrate::{ClientCheck, InstanceRelease};
use judge_core::release::{Compatibility, Incompatible};
use sqlx::PgPool;

/// How long a check stands before the next operation reads the database
/// again, so a long-running `judge-mcp` notices an upgrade under it.
pub const CHECK_EVERY: Duration = Duration::from_mins(1);

/// How to rebuild the tools from source.
const REBUILD: &str = "git pull, then cargo build --release -p judge-agent";

/// What a tool says about the instance's release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Worth knowing; the tool works.
    Info(String),
    /// The tool must not run against this database.
    Stop(String),
}

impl Notice {
    /// The text.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Info(t) | Self::Stop(t) => t,
        }
    }
}

/// What `tool` (`judge-cli`, `judge-mcp`) says about `check`, if anything.
#[must_use]
pub fn notice(check: &ClientCheck, tool: &str) -> Option<Notice> {
    let instance = check.instance.as_ref();
    let runs = || {
        instance.map_or_else(
            || "a release it does not record".to_owned(),
            InstanceRelease::describe,
        )
    };
    let update = || match instance.and_then(|i| i.commit.as_ref()) {
        Some(c) => format!(
            "build {tool} from the instance's commit: git fetch, git checkout {}, then \
             cargo build --release -p judge-agent",
            c.short()
        ),
        None => format!("update {tool} ({REBUILD})"),
    };
    match &check.compatibility {
        Compatibility::Current | Compatibility::AheadOfInstance { .. } => None,
        Compatibility::UpdateAvailable { client, .. } => Some(Notice::Info(format!(
            "this instance runs judgebot {} and this {tool} is {client}. It keeps working; \
             update it to get what changed ({REBUILD}).",
            runs()
        ))),
        Compatibility::Incompatible(why) => {
            let fix = if why.instance_is_older() {
                format!(
                    "upgrade the instance (scripts/deploy.sh), or use the {tool} built from its release"
                )
            } else {
                update()
            };
            let (client, what) = match why {
                Incompatible::Major { client, .. } => (
                    client,
                    format!(
                        "the instance runs judgebot {}, another major release",
                        runs()
                    ),
                ),
                Incompatible::Prerelease { client, .. } => (
                    client,
                    format!(
                        "the instance runs judgebot {}, and a new major's pre-release works only with its own build",
                        runs()
                    ),
                ),
                Incompatible::UnknownBuild { client, .. } => (
                    client,
                    format!(
                        "the instance's database holds migrations it does not know, made by {}",
                        runs()
                    ),
                ),
                Incompatible::InstanceBehind { client, .. } => (
                    client,
                    format!(
                        "the instance's database ({}) lacks migrations it needs",
                        runs()
                    ),
                ),
            };
            Some(Notice::Stop(format!(
                "this {tool} is {client}, but {what}: {fix}."
            )))
        }
    }
}

/// The check, cached for [`CHECK_EVERY`].
pub(crate) struct ReleaseCheck {
    pool: PgPool,
    tool: &'static str,
    last: Mutex<Option<(Instant, Option<Notice>)>>,
}

impl ReleaseCheck {
    pub(crate) fn new(pool: PgPool, tool: &'static str) -> Self {
        Self {
            pool,
            tool,
            last: Mutex::new(None),
        }
    }

    /// The last result, without reading the database.
    pub(crate) fn cached(&self) -> Option<Notice> {
        self.lock().as_ref().and_then(|(_, n)| n.clone())
    }

    /// The current notice, read again once [`CHECK_EVERY`] has passed. A
    /// release that cannot be read is a stop, and is not kept: the next
    /// operation reads again.
    pub(crate) async fn notice(&self) -> Option<Notice> {
        if let Some((at, n)) = self.lock().as_ref()
            && at.elapsed() < CHECK_EVERY
        {
            return n.clone();
        }
        let n = match judge_bot::db::migrate::client_compatibility(&self.pool).await {
            Ok(c) => notice(&c, self.tool),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the instance's release");
                return Some(Notice::Stop(format!(
                    "could not check that this {} works with the instance's database ({e})",
                    self.tool
                )));
            }
        };
        *self.lock() = Some((Instant::now(), n.clone()));
        n
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Instant, Option<Notice>)>> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use judge_core::release::{Release, compatibility};

    use super::*;

    fn says(client: &str, instance: &str) -> anyhow::Result<Option<Notice>> {
        let instance = InstanceRelease {
            release: Release::parse(instance)?,
            commit: Some(judge_core::CommitHash::try_new("6573c59".to_owned())?),
            dirty: false,
        };
        Ok(notice(
            &ClientCheck {
                compatibility: compatibility(
                    &Release::parse(client)?,
                    Some(&instance.release),
                    judge_core::release::Ledger::default(),
                ),
                instance: Some(instance),
            },
            "judge-cli",
        ))
    }

    #[test]
    fn a_minor_behind_is_a_note_and_a_major_a_stop_naming_the_fix() -> anyhow::Result<()> {
        assert!(
            matches!(says("2.1.0", "2.2.0")?, Some(Notice::Info(t)) if t.contains("keeps working"))
        );
        assert_eq!(says("2.2.0", "2.1.0")?, None);
        assert_eq!(says("2.1.0", "2.1.0")?, None);
        assert!(matches!(
            says("2.4.0", "3.0.0")?,
            Some(Notice::Stop(t)) if t.contains("git checkout 6573c59") && t.contains("3.0.0 (commit 6573c59)")
        ));
        assert!(matches!(
            says("3.0.0", "2.4.0")?,
            Some(Notice::Stop(t)) if t.contains("upgrade the instance")
        ));
        assert!(matches!(says("2.0.0-dev", "2.0.0")?, Some(Notice::Stop(_))));
        Ok(())
    }
}
