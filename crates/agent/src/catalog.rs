//! `judge-cli`'s text for people, one [`Messages`] per [`Locale`]
//! (`judge_core::locale`): the usage `--help` prints. Every other output is
//! JSON for an agent or a script, and stays as it is: its keys are an
//! interface, and the operations' text is model-facing
//! ([`crate::ops::AGENT_LOCALE`]). Errors stay English, as logs do: the
//! usage an error quotes is [`EN`]'s.
//!
//! `judge-mcp` prints nothing for people: stdout is the protocol, stderr
//! the log.

use std::fmt;

use judge_core::{Locale, locale::Catalogs};

/// One locale's `judge-cli` text.
#[derive(Clone, Copy)]
pub struct Messages {
    /// The usage, as `judge-cli --help` prints it.
    pub usage: &'static str,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("agent::catalog::Messages")
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

/// The `judge-cli` text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    CATALOGS.get(locale)
}

/// English.
pub const EN: Messages = Messages {
    usage: "usage: judge-cli <judge <question> [--thread T] [--pin span=Name]... [--private] \
| begin <question> [--thread T] [--private] | prompt <session> | status <session> \
| extract <session> <file|-> | rules <session> <id>... | verdict <session> <file|-> [--persist] \
| persist <session> | card <name> | card-info <uuid> | get-rules <id>... | search <query> [--limit N] \
| glossary <term> | config | about \
| stats [--days N | --from YYYY-MM-DD] [--to YYYY-MM-DD] [--interface I] [--asker A] [--model M] \
| failures [--limit N] \
| calls [--interface I] [--asker A] [--model M] [--private | --public] [--search Q] [--cursor C] \
| call <id> | queue [--status S] [--category C] [--interface I] [--rater R] [--cursor C] \
| queue-show <item> | queue-resolve <item> --note N [--fixed-by F] | queue-dismiss <item> --note N \
| admin-url>",
};

/// The stub locale's (`judge_core::locale`, tests only): each message its
/// name in `⟦…⟧`.
pub const STUB: Messages = Messages {
    usage: "⟦usage⟧"
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_locale_renders_every_message() {
        for l in Locale::ALL {
            assert!(!messages(l).usage.trim().is_empty(), "{l}");
        }
    }
}
