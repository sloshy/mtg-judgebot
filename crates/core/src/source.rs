//! The source offer: what every remote interface tells its users about the
//! program they are talking to, so that whoever runs it meets the AGPL's
//! network clause (section 13) without doing anything but naming their
//! repository.
//!
//! Three facts make the offer: where the source is ([`RepositoryUrl`]), which
//! revision was built ([`Commit`]), and the licence and copyright notice
//! ([`COPYRIGHT`], [`LICENSE_SPDX`]). The repository is the operator's to
//! override — a fork that changed anything must point at *its* source — and
//! the commit is stamped at build time, so the two together identify exactly
//! the program that answered. This module is pure: reading the environment
//! and the build stamp is the loader's job (`judge_bot::config`), rendering
//! for Discord, the MCP instructions, the CLI and `GET /api/about` all start
//! from [`SourceOffer::about`].

use std::{borrow::Cow, fmt, sync::LazyLock};

use nutype::nutype;
use regex::Regex;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// The upstream repository: the default offer for an unmodified build.
pub const DEFAULT_REPOSITORY: &str = "https://github.com/sloshy/mtg-judgebot";
/// The variable an operator sets to point the offer at their own repository.
pub const SOURCE_URL_ENV: &str = "JUDGE_SOURCE_URL";
/// The build-time variable the commit is read from (set by the Docker build
/// and the CI workflow; a checkout falls back to `git rev-parse HEAD`).
pub const COMMIT_ENV: &str = "JUDGE_COMMIT";
/// The copyright line of the original work.
pub const COPYRIGHT: &str = "Copyright (C) 2026 Ryan Peters";
/// The licence, as an SPDX identifier.
pub const LICENSE_SPDX: &str = "AGPL-3.0-or-later";
/// The licence, in words.
pub const LICENSE_NAME: &str = "GNU Affero General Public License, version 3 or later";
/// Where to read the licence.
pub const LICENSE_URL: &str = "https://www.gnu.org/licenses/agpl-3.0.html";
/// The program's name in the notice.
pub const PROGRAM: &str = "MTG Judgebot";

/// A hosted repository: `http(s)://` and nothing that could not be a URL.
/// Trailing slashes are dropped so the commit link composes.
#[nutype(
    sanitize(trim, with = |s: String| s.trim_end_matches('/').to_owned()),
    validate(predicate = is_repository_url),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct RepositoryUrl(String);

fn is_repository_url(s: &str) -> bool {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"));
    match rest {
        Some(rest) => {
            !rest.is_empty() && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        None => false,
    }
}

impl JsonSchema for RepositoryUrl {
    fn schema_name() -> Cow<'static, str> {
        "RepositoryUrl".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "The http(s) URL of the repository holding this program's source"
        })
    }
}

/// Regex for [`CommitHash`]: an abbreviated or full git object id.
pub const COMMIT_HASH_PATTERN: &str = r"^[0-9a-f]{7,40}$";

static COMMIT_HASH_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(COMMIT_HASH_PATTERN).expect("COMMIT_HASH_PATTERN is a valid regex")
});

/// A git commit id, lower-case hex, 7 to 40 characters.
#[nutype(
    sanitize(trim, lowercase),
    validate(regex = COMMIT_HASH_RE),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct CommitHash(String);

impl JsonSchema for CommitHash {
    fn schema_name() -> Cow<'static, str> {
        "CommitHash".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A git commit id, 7 to 40 lower-case hex characters"
        })
    }
}

impl CommitHash {
    /// The first seven characters, as git abbreviates.
    #[must_use]
    pub fn short(&self) -> &str {
        self.as_ref().get(..7).unwrap_or_else(|| self.as_ref())
    }
}

/// Which revision was built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// A checkout at `hash`.
    Known {
        /// The commit.
        hash: CommitHash,
        /// Whether the tree carried uncommitted changes, which the hash
        /// then does not describe.
        dirty: bool,
    },
    /// Built outside a checkout without [`COMMIT_ENV`] set. The offer says so
    /// rather than pretending: the operator can still name the repository,
    /// and the docs tell them how to stamp the next build.
    Unknown,
}

impl Commit {
    /// The hash, when there is one.
    #[must_use]
    pub fn hash(&self) -> Option<&CommitHash> {
        match self {
            Self::Known { hash, .. } => Some(hash),
            Self::Unknown => None,
        }
    }

    /// Whether the build carried uncommitted changes.
    #[must_use]
    pub fn dirty(&self) -> bool {
        matches!(self, Self::Known { dirty: true, .. })
    }
}

/// The offer an instance makes: its repository and its revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceOffer {
    repository: RepositoryUrl,
    commit: Commit,
}

/// The offer as data, the same on every interface (`GET /api/about`, the MCP
/// `about` tool, `judge-cli about`). Strings a client can show verbatim
/// (`notice`) sit beside the parts it might lay out itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct About {
    /// The program's name.
    pub program: String,
    /// Where the source of this instance is.
    pub repository: RepositoryUrl,
    /// The commit that was built, when the build knew it.
    pub commit: Option<CommitHash>,
    /// A link to that commit in the repository (`<repository>/commit/<hash>`,
    /// the path GitHub, GitLab, Gitea, Forgejo and Codeberg all serve).
    pub commit_url: Option<String>,
    /// Whether the build carried changes the commit does not describe.
    pub dirty: bool,
    /// The licence's SPDX identifier.
    pub license: String,
    /// The licence, in words.
    pub license_name: String,
    /// Where to read the licence.
    pub license_url: String,
    /// The copyright line.
    pub copyright: String,
    /// The whole offer as one paragraph of plain text.
    pub notice: String,
}

impl SourceOffer {
    /// An offer for `repository` at `commit`.
    #[must_use]
    pub fn new(repository: RepositoryUrl, commit: Commit) -> Self {
        Self { repository, commit }
    }

    /// The offer for the upstream repository.
    ///
    /// # Panics
    /// Never: [`DEFAULT_REPOSITORY`] is a valid URL (pinned by a test).
    #[must_use]
    pub fn upstream(commit: Commit) -> Self {
        #[allow(clippy::expect_used)]
        let repository =
            RepositoryUrl::try_new(DEFAULT_REPOSITORY).expect("DEFAULT_REPOSITORY is a valid URL");
        Self::new(repository, commit)
    }

    /// Where the source is.
    #[must_use]
    pub fn repository(&self) -> &RepositoryUrl {
        &self.repository
    }

    /// Which revision was built.
    #[must_use]
    pub fn commit(&self) -> &Commit {
        &self.commit
    }

    /// `<repository>/commit/<hash>`, when the hash is known.
    #[must_use]
    pub fn commit_url(&self) -> Option<String> {
        self.commit
            .hash()
            .map(|h| format!("{}/commit/{h}", self.repository))
    }

    /// The revision as a phrase: `commit a37d495`, `commit a37d495, built
    /// with uncommitted changes` or `commit unknown`.
    #[must_use]
    pub fn revision(&self) -> String {
        match &self.commit {
            Commit::Known { hash, dirty: false } => format!("commit {}", hash.short()),
            Commit::Known { hash, dirty: true } => {
                format!("commit {}, built with uncommitted changes", hash.short())
            }
            Commit::Unknown => "commit unknown".to_owned(),
        }
    }

    /// The whole offer as one paragraph: name, copyright, licence, and where
    /// the source of this very build is. Every interface shows this text or
    /// a link-decorated rendering of exactly its facts.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "{PROGRAM} — {COPYRIGHT}. Free software under the {LICENSE_NAME} ({LICENSE_SPDX}, \
             {LICENSE_URL}): you may run, study, share and modify it, and anyone offering a \
             modified version over a network must offer its source under the same licence. \
             The source code of this instance is at {} ({}).",
            self.repository,
            self.revision()
        )
    }

    /// The offer as data.
    #[must_use]
    pub fn about(&self) -> About {
        About {
            program: PROGRAM.to_owned(),
            repository: self.repository.clone(),
            commit: self.commit.hash().cloned(),
            commit_url: self.commit_url(),
            dirty: self.commit.dirty(),
            license: LICENSE_SPDX.to_owned(),
            license_name: LICENSE_NAME.to_owned(),
            license_url: LICENSE_URL.to_owned(),
            copyright: COPYRIGHT.to_owned(),
            notice: self.notice(),
        }
    }
}

impl fmt::Display for SourceOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.repository, self.revision())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = anyhow::Result<()>;

    fn known(dirty: bool) -> anyhow::Result<Commit> {
        Ok(Commit::Known {
            hash: CommitHash::try_new("a37d495c937819de39a30ba0624f9bffbfc494d2")?,
            dirty,
        })
    }

    #[test]
    fn the_default_repository_is_a_valid_url() {
        assert_eq!(
            SourceOffer::upstream(Commit::Unknown).repository().as_ref(),
            DEFAULT_REPOSITORY
        );
    }

    #[test]
    fn repository_urls_are_http_and_lose_their_trailing_slash() -> R {
        let ok = RepositoryUrl::try_new("  https://codeberg.org/me/judge/ ")?;
        assert_eq!(ok.as_ref(), "https://codeberg.org/me/judge");
        for bad in [
            "",
            "   ",
            "github.com/me/judge",
            "ftp://x/y",
            "https://",
            "https://a b",
        ] {
            assert!(RepositoryUrl::try_new(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn commit_hashes_are_hex_and_abbreviate_to_seven() -> R {
        let h = CommitHash::try_new(" A37D495C ")?;
        assert_eq!(h.as_ref(), "a37d495c");
        assert_eq!(h.short(), "a37d495");
        for bad in [
            "",
            "a37d49",
            "not-a-hash",
            "a37d495c937819de39a30ba0624f9bffbfc494d2ff",
        ] {
            assert!(CommitHash::try_new(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn the_notice_names_every_fact_and_the_revision_is_honest() -> R {
        let clean = SourceOffer::upstream(known(false)?);
        let n = clean.notice();
        for needle in [
            PROGRAM,
            COPYRIGHT,
            LICENSE_SPDX,
            LICENSE_URL,
            DEFAULT_REPOSITORY,
            "commit a37d495)",
        ] {
            assert!(n.contains(needle), "{needle}\n{n}");
        }
        assert_eq!(
            clean.commit_url().as_deref(),
            Some(
                "https://github.com/sloshy/mtg-judgebot/commit/a37d495c937819de39a30ba0624f9bffbfc494d2"
            )
        );
        assert!(
            SourceOffer::upstream(known(true)?)
                .notice()
                .contains("commit a37d495, built with uncommitted changes")
        );
        let unknown = SourceOffer::upstream(Commit::Unknown);
        assert!(unknown.notice().contains("(commit unknown)"));
        assert_eq!(unknown.commit_url(), None);
        Ok(())
    }

    #[test]
    fn about_carries_the_same_facts_as_the_notice() -> R {
        let offer = SourceOffer::upstream(known(true)?);
        let a = offer.about();
        assert_eq!(a.license, LICENSE_SPDX);
        assert_eq!(a.copyright, COPYRIGHT);
        assert!(a.dirty);
        assert_eq!(a.commit.as_ref().map(CommitHash::short), Some("a37d495"));
        assert_eq!(a.notice, offer.notice());
        let json = serde_json::to_string(&a)?;
        let back: About = serde_json::from_str(&json)?;
        assert_eq!(back, a);
        Ok(())
    }
}
