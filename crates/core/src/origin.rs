//! Where a judge run came from: the interface, who asked (on Discord), and
//! whether the call is private.
//!
//! "Private" means not public-facing, not hidden from the operator: a private
//! call is stored with its text and asker like any other, and kept out of
//! everything another asker can see. It is never a prior-call example, and it
//! is never read as a public call's thread history. Only the admin surfaces
//! show it.
//!
//! Every interface states its [`Origin`] when it stores a call or a failed
//! call ([`crate::CallStore`]), so the store cannot guess it from a thread id.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::DiscordUsername;

/// The interface a question came in through. Stored as `calls.interface`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interface {
    /// `/judge` on Discord (a "did you mean?" pick included).
    Discord,
    /// The web app or `POST /api/judge`.
    Web,
    /// The MCP `judge` tool, over stdio `judge-mcp` or `/mcp`.
    Mcp,
    /// `judge-cli judge`.
    Cli,
    /// An agent-driven session (`begin_session` … `persist_session`), over
    /// MCP or `judge-cli`.
    AgentSession,
}

impl Interface {
    /// Every variant, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Discord,
        Self::Web,
        Self::Mcp,
        Self::Cli,
        Self::AgentSession,
    ];

    /// The stored id (`calls.interface`), equal to the serde name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Web => "web",
            Self::Mcp => "mcp",
            Self::Cli => "cli",
            Self::AgentSession => "agent_session",
        }
    }
}

impl fmt::Display for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Whether a call may appear where people other than the asker and the
/// operator can see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Thread history, prior-call examples, and the public pages.
    #[default]
    Public,
    /// The asker's reply and the admin surfaces only.
    Private,
}

impl Visibility {
    /// `private: true` is [`Visibility::Private`]; `false` or absent is public.
    #[must_use]
    pub const fn from_private(private: bool) -> Self {
        if private { Self::Private } else { Self::Public }
    }

    /// Whether this is [`Visibility::Private`].
    #[must_use]
    pub const fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }
}

/// The Discord user who asked, as of asking.
///
/// The id is what `/forget` matches on. The name is the unique username (not
/// the display name or a server nickname), snapshotted so that a later rename
/// does not rewrite who asked. It is `None` when Discord's value is not a
/// username by Discord's own rule (an account still on a legacy `name#1234`
/// tag): the call is then shown as anonymous, and `/forget` still finds it by
/// id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asker {
    /// The user's snowflake.
    pub id: NonZeroU64,
    /// The username at asking time, when it is one.
    pub name: Option<DiscordUsername>,
}

impl Asker {
    /// The asker `id`, named `username` if that parses as a
    /// [`DiscordUsername`].
    #[must_use]
    pub fn new(id: NonZeroU64, username: &str) -> Self {
        Self {
            id,
            name: DiscordUsername::try_new(username.to_owned()).ok(),
        }
    }

    /// The id as stored (`calls.asker_id`, `ratings.user_id`): decimal.
    #[must_use]
    pub fn id_string(&self) -> String {
        self.id.to_string()
    }
}

/// Where a run came from. Only Discord knows who asked; the web has no
/// private option (it has no accounts to show a private answer to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `/judge` or `/judge-private`, by `asker`.
    Discord {
        /// Who asked.
        asker: Asker,
        /// `/judge-private` or not.
        visibility: Visibility,
    },
    /// The web app or `POST /api/judge`: anonymous and public.
    Web,
    /// The MCP `judge` tool.
    Mcp(Visibility),
    /// `judge-cli judge`.
    Cli(Visibility),
    /// An agent-driven session.
    AgentSession(Visibility),
}

/// Which earlier calls of a thread a question may read as history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    /// Public calls only.
    Public,
    /// Public and private calls: the thread is the caller's own.
    WithPrivate,
}

impl Origin {
    /// The interface.
    #[must_use]
    pub const fn interface(&self) -> Interface {
        match self {
            Self::Discord { .. } => Interface::Discord,
            Self::Web => Interface::Web,
            Self::Mcp(_) => Interface::Mcp,
            Self::Cli(_) => Interface::Cli,
            Self::AgentSession(_) => Interface::AgentSession,
        }
    }

    /// Who asked, where the interface knows.
    #[must_use]
    pub const fn asker(&self) -> Option<&Asker> {
        match self {
            Self::Discord { asker, .. } => Some(asker),
            Self::Web | Self::Mcp(_) | Self::Cli(_) | Self::AgentSession(_) => None,
        }
    }

    /// Public or private.
    #[must_use]
    pub const fn visibility(&self) -> Visibility {
        match self {
            Self::Discord { visibility, .. }
            | Self::Mcp(visibility)
            | Self::Cli(visibility)
            | Self::AgentSession(visibility) => *visibility,
            Self::Web => Visibility::Public,
        }
    }

    /// Whether the call is private.
    #[must_use]
    pub const fn is_private(&self) -> bool {
        self.visibility().is_private()
    }

    /// What this question may read of its thread's history, or `None` for
    /// nothing.
    ///
    /// A Discord thread is a channel, shared by everyone in it: a private
    /// question there reads no history, so that one user's private calls never
    /// reach another's answer, and a public one reads public calls only. An
    /// agent thread (`agent:<uuid>`) is known only to the caller it was
    /// returned to, so a private agent question reads its own thread whole;
    /// a public one still reads only public calls, because its answer is
    /// public.
    #[must_use]
    pub const fn history_scope(&self) -> Option<HistoryScope> {
        match self {
            Self::Discord {
                visibility: Visibility::Private,
                ..
            } => None,
            Self::Discord {
                visibility: Visibility::Public,
                ..
            }
            | Self::Web
            | Self::Mcp(Visibility::Public)
            | Self::Cli(Visibility::Public)
            | Self::AgentSession(Visibility::Public) => Some(HistoryScope::Public),
            Self::Mcp(Visibility::Private)
            | Self::Cli(Visibility::Private)
            | Self::AgentSession(Visibility::Private) => Some(HistoryScope::WithPrivate),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker() -> Asker {
        Asker::new(NonZeroU64::MIN, "urza")
    }

    #[test]
    fn interface_ids_are_the_serde_names() -> Result<(), serde_json::Error> {
        for i in Interface::ALL {
            assert_eq!(serde_json::to_value(i)?, serde_json::json!(i.id()));
        }
        Ok(())
    }

    #[test]
    fn only_discord_has_an_asker_and_only_the_web_is_always_public() {
        let discord = Origin::Discord {
            asker: asker(),
            visibility: Visibility::Private,
        };
        assert_eq!(discord.asker(), Some(&asker()));
        assert!(discord.is_private());
        assert_eq!(Origin::Web.asker(), None);
        assert_eq!(Origin::Web.visibility(), Visibility::Public);
        for o in [
            Origin::Mcp(Visibility::Private),
            Origin::Cli(Visibility::Private),
            Origin::AgentSession(Visibility::Private),
        ] {
            assert!(o.is_private());
            assert_eq!(o.asker(), None);
        }
    }

    #[test]
    fn a_private_discord_question_reads_no_history_and_a_public_one_no_private_calls() {
        let ask = |visibility| Origin::Discord {
            asker: asker(),
            visibility,
        };
        assert_eq!(ask(Visibility::Private).history_scope(), None);
        assert_eq!(
            ask(Visibility::Public).history_scope(),
            Some(HistoryScope::Public)
        );
        assert_eq!(Origin::Web.history_scope(), Some(HistoryScope::Public));
    }

    #[test]
    fn a_private_agent_question_reads_its_own_thread_whole() {
        for o in [
            Origin::Mcp(Visibility::Private),
            Origin::Cli(Visibility::Private),
            Origin::AgentSession(Visibility::Private),
        ] {
            assert_eq!(o.history_scope(), Some(HistoryScope::WithPrivate));
        }
        for o in [
            Origin::Mcp(Visibility::Public),
            Origin::Cli(Visibility::Public),
            Origin::AgentSession(Visibility::Public),
        ] {
            assert_eq!(o.history_scope(), Some(HistoryScope::Public));
        }
    }

    #[test]
    fn a_legacy_tag_is_no_username() {
        assert_eq!(Asker::new(NonZeroU64::MIN, "Urza#1234").name, None);
        assert_eq!(
            Asker::new(NonZeroU64::MIN, "urza")
                .name
                .map(|n| n.to_string()),
            Some("urza".to_owned())
        );
    }
}
