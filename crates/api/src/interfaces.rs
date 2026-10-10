//! Which interfaces a process serves.
//!
//! `judgebot` names its interfaces as roles (`--api`, `--web`, `--mcp`, each
//! a [`Interface`]), and every one is opt-in: an operator who never asked to
//! publish a page never gets one.
//!
//! The set is a [`NonEmpty`], so "a listener bound to no interface at all" is
//! not a state this program can reach. `GET /api/health` is outside the set —
//! it reports on the process, not on a interface, and a container healthcheck
//! must be able to reach it whatever else is switched off.

use std::fmt;

use nonempty::NonEmpty;

/// One interface of the HTTP adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    /// `POST /api/judge` — the anonymous question route.
    Api,
    /// The built web client, served from `WEB_DIST` as the router's fallback.
    Web,
    /// The MCP transport at `/mcp`, behind `MCP_TOKEN`.
    Mcp,
}

impl Interface {
    /// Every interface, in the order the usage text and the logs list them.
    /// A new variant belongs here as well as in the matches below; nothing but
    /// this comment enforces that, so [`Interfaces`] is written not to depend
    /// on it — the set it reports is the set it holds.
    pub const ALL: [Self; 3] = [Self::Api, Self::Web, Self::Mcp];

    /// The flag that enables it.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Api => "--api",
            Self::Web => "--web",
            Self::Mcp => "--mcp",
        }
    }

    /// Its name in a log line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Web => "web",
            Self::Mcp => "mcp",
        }
    }
}

/// The interfaces this launch enables: never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interfaces(NonEmpty<Interface>);

impl Interfaces {
    /// The set of `interfaces`.
    ///
    /// Duplicates collapse and the order becomes [`Interface::ALL`]'s, so two
    /// ways of naming the same interfaces are the same value — equality here is set
    /// equality, not "typed in the same order".
    #[must_use]
    pub fn of(interfaces: &NonEmpty<Interface>) -> Self {
        let canonical: Vec<Interface> = Interface::ALL
            .into_iter()
            .filter(|i| interfaces.contains(i))
            .collect();
        // Every variant is in `ALL`, so the filter keeps at least the head and
        // the fallback is unreachable today. It is the whole input rather than
        // part of it, so a variant that went missing from `ALL` would cost the
        // canonical order and nothing else — never an interface dropped or added.
        Self(NonEmpty::from_vec(canonical).unwrap_or_else(|| interfaces.clone()))
    }

    /// The interfaces, in [`Interface::ALL`]'s order.
    pub fn iter(&self) -> impl Iterator<Item = Interface> + '_ {
        self.0.iter().copied()
    }

    /// Is `interface` enabled?
    #[must_use]
    pub fn has(&self, interface: Interface) -> bool {
        self.0.contains(&interface)
    }

    /// Is the anonymous question route mounted?
    #[must_use]
    pub fn api(&self) -> bool {
        self.has(Interface::Api)
    }

    /// Is the built web page served?
    #[must_use]
    pub fn web(&self) -> bool {
        self.has(Interface::Web)
    }

    /// Is the MCP transport mounted?
    #[must_use]
    pub fn mcp(&self) -> bool {
        self.has(Interface::Mcp)
    }

    /// The interfaces this launch left off, for the startup log: an operator
    /// looking for a page that is not there reads the reason here rather than
    /// guessing from a 404.
    #[must_use]
    pub fn disabled(&self) -> String {
        list(Interface::ALL.into_iter().filter(|i| !self.has(*i)))
    }
}

impl fmt::Display for Interfaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The set itself, not `ALL` filtered by it: the log then names what is
        // mounted even for a set `ALL` does not cover. `of` has already put it
        // in `ALL` order, so two equivalent launches still log identically.
        f.write_str(&list(self.0.iter().copied()))
    }
}

fn list(interfaces: impl Iterator<Item = Interface>) -> String {
    let names: Vec<&str> = interfaces.map(Interface::name).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_built_directly_dedupes_and_canonicalises() {
        use nonempty::nonempty;
        let typed_backwards =
            Interfaces::of(&nonempty![Interface::Web, Interface::Api, Interface::Web]);
        assert_eq!(typed_backwards.to_string(), "api, web");
        assert_eq!(
            typed_backwards,
            Interfaces::of(&nonempty![Interface::Api, Interface::Web])
        );
        assert!(typed_backwards.api() && typed_backwards.web() && !typed_backwards.mcp());
    }

    #[test]
    fn every_interface_has_a_distinct_flag_and_name() {
        let flags: std::collections::BTreeSet<&str> =
            Interface::ALL.into_iter().map(Interface::flag).collect();
        let names: std::collections::BTreeSet<&str> =
            Interface::ALL.into_iter().map(Interface::name).collect();
        assert_eq!(flags.len(), Interface::ALL.len());
        assert_eq!(names.len(), Interface::ALL.len());
        assert!(
            Interface::ALL
                .into_iter()
                .all(|i| i.flag().starts_with("--"))
        );
    }
}
