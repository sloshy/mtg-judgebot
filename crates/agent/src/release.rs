//! A client tool's release against the instance's (`judge_core::release`):
//! what `judge-cli` and a stdio `judge-mcp` tell their user when the
//! database was migrated by another release.
//!
//! A newer release of the same major is a notice, and the tool goes on
//! working. A different major, a different pre-release, or a database that
//! lacks a migration the tool carries stops every operation with a message
//! naming what to update. `judgebot --mcp` is part of the instance and does
//! no check.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

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

/// What `tool` (`judge-cli`, `judge-mcp`) says about `c`, if anything.
#[must_use]
pub fn notice(c: &Compatibility, tool: &str) -> Option<Notice> {
    match c {
        Compatibility::Current | Compatibility::AheadOfInstance { .. } => None,
        Compatibility::UpdateAvailable { client, instance } => Some(Notice::Info(format!(
            "this instance runs judgebot {instance} and this {tool} is {client}. It keeps working; \
             update it to get what changed ({REBUILD})."
        ))),
        Compatibility::Incompatible(why) => Some(Notice::Stop(stop(why, tool))),
    }
}

fn stop(why: &Incompatible, tool: &str) -> String {
    let fix = |instance: &str| {
        if why.instance_is_older() {
            format!(
                "upgrade the instance (scripts/deploy.sh), or use the {tool} built from its release ({instance})"
            )
        } else {
            format!("update {tool} ({REBUILD})")
        }
    };
    match why {
        Incompatible::Major { client, instance } => format!(
            "this {tool} is {client}, but the instance runs judgebot {instance}, another major release, \
             whose database it cannot use: {}.",
            fix(&instance.to_string())
        ),
        Incompatible::Prerelease { client, instance } => format!(
            "this {tool} is {client}, but the instance runs judgebot {instance}, and a pre-release works \
             only with its own build: {}.",
            fix(&instance.to_string())
        ),
        Incompatible::UnknownBuild { client, instance } => {
            let by = instance.as_ref().map_or_else(
                || "a release it does not record".to_owned(),
                |r| format!("another build of {r}"),
            );
            format!(
                "this {tool} is {client}, but the instance's database holds migrations it does not \
                 know, made by {by}: rebuild {tool} from the instance's commit ({REBUILD})."
            )
        }
        Incompatible::InstanceBehind { client, instance } => {
            let at = instance
                .as_ref()
                .map_or_else(|| "an older release".to_owned(), ToString::to_string);
            format!(
                "this {tool} is {client}, but the instance's database ({at}) lacks migrations it needs: {}.",
                fix(&at)
            )
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
        Ok(notice(
            &compatibility(
                &Release::parse(client)?,
                Some(&Release::parse(instance)?),
                judge_core::release::Ledger::default(),
            ),
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
            Some(Notice::Stop(t)) if t.contains("update judge-cli") && t.contains("3.0.0")
        ));
        assert!(matches!(
            says("3.0.0", "2.4.0")?,
            Some(Notice::Stop(t)) if t.contains("upgrade the instance")
        ));
        assert!(matches!(says("2.0.0-dev", "2.0.0")?, Some(Notice::Stop(_))));
        Ok(())
    }
}
