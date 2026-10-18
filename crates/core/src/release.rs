//! Which releases work together: a client tool (`judge-cli`, a stdio
//! `judge-mcp`) built from one release, against a database an instance of
//! another release migrated.
//!
//! The rule follows the release policy (CONTRIBUTING, Migrations):
//!
//! * Within a major version, a release keeps the previous one working, so an
//!   older client goes on working against a newer instance and is told an
//!   update exists.
//! * Across a major version, the schema may break, so a client of another
//!   major stops.
//! * A pre-release of a new major (`3.0.0-dev`, `3.0.0-rc.1`) is where that
//!   major's breaking changes are made, so it promises nothing to any other
//!   version: a client and an instance that are not the same one stop. A
//!   pre-release of a minor or patch (`2.1.0-dev`) only adds, like the release
//!   it leads to, and is ordered among its major's releases.
//! * Whatever the versions say, a database that lacks a migration the client
//!   carries cannot hold what the client reads and writes, so the client
//!   stops.
//! * A database holding a migration the client does not know stops it when
//!   nothing says those migrations only add: no release is recorded, or the
//!   same major pre-release is (another build of `3.0.0-dev` made them).

use std::{cmp::Ordering, fmt};

/// A release version: `MAJOR.MINOR.PATCH`, optionally `-pre`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Release(semver::Version);

impl Release {
    /// The release this binary was built as (the workspace version).
    ///
    /// `0.0.0` if the workspace version is not semver, which a test rules out.
    #[must_use]
    pub fn this_build() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Self(semver::Version::new(0, 0, 0)))
    }

    /// Parse `s` as a release version.
    ///
    /// # Errors
    /// When `s` is not semver.
    pub fn parse(s: &str) -> Result<Self, semver::Error> {
        semver::Version::parse(s.trim()).map(Self)
    }

    /// The major version.
    #[must_use]
    pub const fn major(&self) -> u64 {
        self.0.major
    }

    /// Whether this is a pre-release (`2.0.0-dev`, `2.1.0-rc.1`).
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.0.pre.is_empty()
    }

    /// Whether this is a pre-release of a new major (`3.0.0-dev`): the
    /// builds in which that major's breaking migrations are made.
    #[must_use]
    pub fn is_major_prerelease(&self) -> bool {
        self.is_prerelease() && self.0.minor == 0 && self.0.patch == 0
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// How a client release stands against the instance's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// The same release, or the instance's is not recorded and nothing is
    /// missing: nothing to say.
    Current,
    /// The instance runs a newer release of the same major. The client works;
    /// an update brings what changed.
    UpdateAvailable {
        /// The client's release.
        client: Release,
        /// The instance's.
        instance: Release,
    },
    /// The client is a newer release of the same major than the instance, and
    /// the database has every migration it needs. It works.
    AheadOfInstance {
        /// The client's release.
        client: Release,
        /// The instance's.
        instance: Release,
    },
    /// The client must not run against this database.
    Incompatible(Incompatible),
}

/// Why a client must stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incompatible {
    /// A different major version.
    Major {
        /// The client's release.
        client: Release,
        /// The instance's.
        instance: Release,
    },
    /// Different releases, at least one a pre-release of a new major.
    Prerelease {
        /// The client's release.
        client: Release,
        /// The instance's.
        instance: Release,
    },
    /// The database holds migrations the client does not know, and nothing
    /// says they only add: no release is recorded (a database migrated with
    /// `sqlx migrate run`), or another build of the same major pre-release
    /// made them.
    UnknownBuild {
        /// The client's release.
        client: Release,
        /// The instance's, when recorded.
        instance: Option<Release>,
    },
    /// The database lacks migrations the client carries: the instance has not
    /// been upgraded to the client's release.
    InstanceBehind {
        /// The client's release.
        client: Release,
        /// The instance's, when recorded.
        instance: Option<Release>,
    },
}

impl Incompatible {
    /// Whether the instance is the one to upgrade, rather than the client.
    #[must_use]
    pub fn instance_is_older(&self) -> bool {
        match self {
            Self::InstanceBehind { .. } => true,
            Self::UnknownBuild { .. } => false,
            Self::Major { client, instance } | Self::Prerelease { client, instance } => {
                client > instance
            }
        }
    }
}

/// How the database's migration ledger compares with the client's
/// migrations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// The database lacks a migration the client carries.
    pub lacks: bool,
    /// The database holds a migration the client does not carry.
    pub unknown: bool,
}

/// Where `client` stands against an instance of release `instance` (`None`
/// when the database records none), given how the database's migrations
/// compare with the client's.
#[must_use]
pub fn compatibility(
    client: &Release,
    instance: Option<&Release>,
    ledger: Ledger,
) -> Compatibility {
    if ledger.lacks {
        return Compatibility::Incompatible(Incompatible::InstanceBehind {
            client: client.clone(),
            instance: instance.cloned(),
        });
    }
    let unknown_build = || {
        Compatibility::Incompatible(Incompatible::UnknownBuild {
            client: client.clone(),
            instance: instance.cloned(),
        })
    };
    let Some(instance) = instance else {
        return if ledger.unknown {
            unknown_build()
        } else {
            Compatibility::Current
        };
    };
    let pair = || (client.clone(), instance.clone());
    if client.major() != instance.major() {
        let (client, instance) = pair();
        Compatibility::Incompatible(Incompatible::Major { client, instance })
    } else if client.is_major_prerelease() || instance.is_major_prerelease() {
        // Breaking migrations are being made: only the very same build.
        if client != instance {
            let (client, instance) = pair();
            Compatibility::Incompatible(Incompatible::Prerelease { client, instance })
        } else if ledger.unknown {
            unknown_build()
        } else {
            Compatibility::Current
        }
    } else {
        // Within a major every migration only adds, so migrations the client
        // does not know cannot break it, whoever made them.
        let (client, instance) = pair();
        match client.cmp(&instance) {
            Ordering::Less => Compatibility::UpdateAvailable { client, instance },
            Ordering::Greater => Compatibility::AheadOfInstance { client, instance },
            Ordering::Equal => Compatibility::Current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), semver::Error>;

    const LACKS: Ledger = Ledger {
        lacks: true,
        unknown: false,
    };
    const UNKNOWN: Ledger = Ledger {
        lacks: false,
        unknown: true,
    };

    fn r(s: &str) -> Result<Release, semver::Error> {
        Release::parse(s)
    }

    /// Migrations the client does not know stop it only when nothing says
    /// they only add.
    #[test]
    fn unknown_migrations_stop_the_client_only_when_nothing_says_they_add() -> Res {
        let unknown = |c: &str, i: Option<&str>| -> Result<Compatibility, semver::Error> {
            let i = i.map(r).transpose()?;
            Ok(compatibility(&r(c)?, i.as_ref(), UNKNOWN))
        };
        for (c, i) in [
            ("2.1.0", Some("2.3.0")),
            ("2.1.0", Some("2.1.0")),
            ("2.3.0", Some("2.1.0")),
            ("2.1.0-dev", Some("2.1.0-dev")),
            ("2.0.0", Some("2.1.0-dev")),
        ] {
            assert!(
                !matches!(unknown(c, i)?, Compatibility::Incompatible(_)),
                "{c} vs {i:?}"
            );
        }
        for (c, i) in [("3.0.0-dev", Some("3.0.0-dev")), ("2.1.0", None)] {
            assert!(
                matches!(
                    unknown(c, i)?,
                    Compatibility::Incompatible(Incompatible::UnknownBuild { .. })
                ),
                "{c} vs {i:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn this_build_is_semver() -> Res {
        semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
        assert_ne!(Release::this_build(), r("0.0.0")?);
        Ok(())
    }

    #[test]
    fn a_newer_minor_is_a_notice_and_a_newer_major_a_stop() -> Res {
        assert_eq!(
            compatibility(&r("2.1.0")?, Some(&r("2.3.1")?), Ledger::default()),
            Compatibility::UpdateAvailable {
                client: r("2.1.0")?,
                instance: r("2.3.1")?
            }
        );
        assert_eq!(
            compatibility(&r("2.3.1")?, Some(&r("2.1.0")?), Ledger::default()),
            Compatibility::AheadOfInstance {
                client: r("2.3.1")?,
                instance: r("2.1.0")?
            }
        );
        assert_eq!(
            compatibility(&r("2.4.0")?, Some(&r("3.0.0")?), Ledger::default()),
            Compatibility::Incompatible(Incompatible::Major {
                client: r("2.4.0")?,
                instance: r("3.0.0")?
            })
        );
        assert_eq!(
            compatibility(&r("2.1.0")?, Some(&r("2.1.0")?), Ledger::default()),
            Compatibility::Current
        );
        Ok(())
    }

    #[test]
    fn a_new_majors_pre_releases_promise_nothing_to_other_versions() -> Res {
        for (client, instance) in [
            ("2.0.0-dev", "2.0.0"),
            ("2.0.0", "2.0.0-dev"),
            ("3.0.0-rc.1", "3.0.0-rc.2"),
            ("3.0.0-dev", "3.0.0"),
        ] {
            assert!(
                matches!(
                    compatibility(&r(client)?, Some(&r(instance)?), Ledger::default()),
                    Compatibility::Incompatible(Incompatible::Prerelease { .. })
                ),
                "{client} vs {instance}"
            );
        }
        assert_eq!(
            compatibility(&r("2.0.0-dev")?, Some(&r("2.0.0-dev")?), Ledger::default()),
            Compatibility::Current
        );
        // A different major is reported as such, pre-release or not.
        assert!(matches!(
            compatibility(&r("2.0.0-dev")?, Some(&r("1.2.0")?), Ledger::default()),
            Compatibility::Incompatible(Incompatible::Major { .. })
        ));
        // A minor's pre-release orders among its major's releases.
        assert!(matches!(
            compatibility(&r("2.0.0")?, Some(&r("2.1.0-dev")?), Ledger::default()),
            Compatibility::UpdateAvailable { .. }
        ));
        assert!(matches!(
            compatibility(&r("2.1.0-dev")?, Some(&r("2.1.0")?), Ledger::default()),
            Compatibility::UpdateAvailable { .. }
        ));
        Ok(())
    }

    #[test]
    fn missing_migrations_stop_the_client_whatever_the_versions_say() -> Res {
        for instance in [None, Some(r("2.0.0")?), Some(r("2.5.0")?)] {
            assert_eq!(
                compatibility(&r("2.0.0")?, instance.as_ref(), LACKS),
                Compatibility::Incompatible(Incompatible::InstanceBehind {
                    client: r("2.0.0")?,
                    instance: instance.clone()
                })
            );
        }
        assert_eq!(
            compatibility(&r("2.0.0")?, None, Ledger::default()),
            Compatibility::Current
        );
        Ok(())
    }

    #[test]
    fn the_older_side_is_named() -> Res {
        let older = |c: &str, i: &str| -> Result<Option<bool>, semver::Error> {
            Ok(
                match compatibility(&r(c)?, Some(&r(i)?), Ledger::default()) {
                    Compatibility::Incompatible(why) => Some(why.instance_is_older()),
                    _ => None,
                },
            )
        };
        assert_eq!(older("1.2.0", "2.0.0")?, Some(false));
        assert_eq!(older("3.0.0", "2.0.0")?, Some(true));
        assert_eq!(older("2.0.0-dev", "2.0.0")?, Some(false));
        Ok(())
    }
}
