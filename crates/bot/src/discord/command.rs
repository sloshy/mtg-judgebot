//! The slash commands: what is registered with Discord ([`definitions`]) and
//! what an invocation parses into ([`Command::parse`]). Pure: both are built
//! from the names below, so a registered command always parses.

use serenity::all::{
    CommandData, CommandDataOption, CommandDataOptionValue, CommandOptionType, CreateCommand,
    CreateCommandOption, InteractionContext,
};

use judge_core::Locale;

use super::{
    Audience,
    catalog::{self, Commands},
    render,
};

/// The longest `/judge` question, in characters: the `max_length` of its
/// `question` option. At most [`render::PICK_QUESTION_LIMIT`], so a "did you
/// mean…?" always has room for the whole question, and a quarter of
/// `u16::MAX`, so its byte offsets fit a pick's `custom_id`.
pub const QUESTION_MAX_CHARS: u16 = 1300;
const _: () = assert!(QUESTION_MAX_CHARS as usize <= render::PICK_QUESTION_LIMIT);
const _: () = assert!(QUESTION_MAX_CHARS as usize <= render::RETRY_QUESTION_LIMIT);
const _: () = assert!(QUESTION_MAX_CHARS as u32 * 4 <= u16::MAX as u32);

const JUDGE: &str = "judge";
const CARD: &str = "card";
const RULE: &str = "rule";
const HELP: &str = "help";
const LICENSE: &str = "license";
const FORGET: &str = "forget";

const QUESTION: &str = "question";
const PRIVATE: &str = "private";
const NAME: &str = "name";
const ID: &str = "id";

/// A slash command as invoked, its options read and typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `/judge`: a rules question, asked in a server.
    Judge {
        /// The question, as typed.
        question: String,
        /// Who sees the answer.
        audience: Audience,
    },
    /// `/card`: Oracle text and rulings.
    Card {
        /// The card's name or nickname, as typed.
        name: String,
        /// Whether the reply is shown to the asker alone.
        private: bool,
    },
    /// `/rule`: Comprehensive Rules text.
    Rule {
        /// The rule number, as typed.
        id: String,
        /// Whether the reply is shown to the asker alone.
        private: bool,
    },
    /// `/help`.
    Help,
    /// `/license`.
    License,
    /// `/forget`.
    Forget,
}

/// Why an invocation is not a [`Command`]. Discord checks the options against
/// the registration, so these come from a registration other than this
/// build's (an older or newer release's, during a deploy) or from a DM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unparsed {
    /// No command of that name in this build.
    Unknown(String),
    /// A required option is missing, or an option has the wrong type.
    BadOption {
        /// The command.
        command: &'static str,
        /// The option.
        option: &'static str,
    },
    /// A server-only command invoked outside a server.
    NotInGuild,
}

impl std::fmt::Display for Unparsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no command named {name:?} in this build"),
            Self::BadOption { command, option } => {
                write!(
                    f,
                    "/{command}: option {option:?} is missing or of the wrong type"
                )
            }
            Self::NotInGuild => f.write_str("a server-only command outside a server"),
        }
    }
}

impl Command {
    /// The command `data` invokes. `in_guild` is whether it was invoked in
    /// a server (the interaction carries a guild id).
    ///
    /// # Errors
    /// [`Unparsed`]: an unknown name, a missing or mistyped option, or
    /// `/judge` outside a server.
    pub fn parse(data: &CommandData, in_guild: bool) -> Result<Self, Unparsed> {
        let options = Options(&data.options);
        match data.name.as_str() {
            JUDGE => {
                if !in_guild {
                    return Err(Unparsed::NotInGuild);
                }
                Ok(Self::Judge {
                    question: options.string(JUDGE, QUESTION)?,
                    audience: Audience::from_option(options.flag(JUDGE, PRIVATE)?),
                })
            }
            CARD => Ok(Self::Card {
                name: options.string(CARD, NAME)?,
                private: options.flag(CARD, PRIVATE)? == Some(true),
            }),
            RULE => Ok(Self::Rule {
                id: options.string(RULE, ID)?,
                private: options.flag(RULE, PRIVATE)? == Some(true),
            }),
            HELP => Ok(Self::Help),
            LICENSE => Ok(Self::License),
            FORGET => Ok(Self::Forget),
            other => Err(Unparsed::Unknown(other.to_owned())),
        }
    }

    /// The command's name, for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Judge { .. } => JUDGE,
            Self::Card { .. } => CARD,
            Self::Rule { .. } => RULE,
            Self::Help => HELP,
            Self::License => LICENSE,
            Self::Forget => FORGET,
        }
    }

    /// Whether the command's replies are private unless a reply says
    /// otherwise.
    #[must_use]
    pub const fn ephemeral(&self) -> bool {
        matches!(self, Self::Help | Self::License | Self::Forget)
    }

    /// Whether the command is answered while a major upgrade migrates: only
    /// those that need no table of the schema being changed.
    #[must_use]
    pub const fn answers_in_maintenance(&self) -> bool {
        match self {
            Self::Help | Self::License => true,
            Self::Judge { .. } | Self::Card { .. } | Self::Rule { .. } | Self::Forget => false,
        }
    }
}

/// A command's options, by name.
struct Options<'a>(&'a [CommandDataOption]);

impl Options<'_> {
    fn get(&self, name: &str) -> Option<&CommandDataOptionValue> {
        self.0.iter().find(|o| o.name == name).map(|o| &o.value)
    }

    /// A required string option.
    fn string(&self, command: &'static str, option: &'static str) -> Result<String, Unparsed> {
        match self.get(option) {
            Some(CommandDataOptionValue::String(s)) => Ok(s.clone()),
            _ => Err(Unparsed::BadOption { command, option }),
        }
    }

    /// An optional boolean option.
    fn flag(&self, command: &'static str, option: &'static str) -> Result<Option<bool>, Unparsed> {
        match self.get(option) {
            None => Ok(None),
            Some(CommandDataOptionValue::Boolean(b)) => Ok(Some(*b)),
            Some(_) => Err(Unparsed::BadOption { command, option }),
        }
    }
}

/// The description of every locale but the default, under each of its
/// Discord tags: Discord shows a user the one for their client's language,
/// and the base description (the default locale's) otherwise.
fn localized(text: fn(&Commands) -> &'static str) -> Vec<(&'static str, &'static str)> {
    Locale::ALL
        .into_iter()
        .filter(|l| *l != Locale::default())
        .flat_map(|l| {
            let description = text(&catalog::messages(l).commands);
            catalog::discord_tags(l)
                .iter()
                .map(move |tag| (*tag, description))
        })
        .collect()
}

/// A command with its description in every locale.
fn command(name: &str, text: fn(&Commands) -> &'static str) -> CreateCommand {
    localized(text).into_iter().fold(
        CreateCommand::new(name).description(text(&BASE.commands)),
        |c, (tag, d)| c.description_localized(tag, d),
    )
}

/// An option with its description in every locale.
fn option(
    kind: CommandOptionType,
    name: &str,
    text: fn(&Commands) -> &'static str,
) -> CreateCommandOption {
    localized(text).into_iter().fold(
        CreateCommandOption::new(kind, name, text(&BASE.commands)),
        |o, (tag, d)| o.description_localized(tag, d),
    )
}

fn private_option(text: fn(&Commands) -> &'static str) -> CreateCommandOption {
    option(CommandOptionType::Boolean, PRIVATE, text).required(false)
}

fn text_option(name: &str, text: fn(&Commands) -> &'static str) -> CreateCommandOption {
    option(CommandOptionType::String, name, text).required(true)
}

/// The default locale's text: the descriptions Discord falls back to.
const BASE: &catalog::Messages = catalog::messages(Locale::En);
const _: () = assert!(matches!(Locale::ALL[0], Locale::En));

/// The six commands as they are registered with Discord, in order.
#[must_use]
pub fn definitions() -> Vec<CreateCommand> {
    vec![
        command(JUDGE, |c| c.judge)
            .add_option(text_option(QUESTION, |c| c.judge_question).max_length(QUESTION_MAX_CHARS))
            .add_option(private_option(|c| c.judge_private))
            .contexts(vec![InteractionContext::Guild]),
        command(CARD, |c| c.card)
            .add_option(text_option(NAME, |c| c.card_name))
            .add_option(private_option(|c| c.card_private)),
        command(RULE, |c| c.rule)
            .add_option(text_option(ID, |c| c.rule_id))
            .add_option(private_option(|c| c.rule_private)),
        command(HELP, |c| c.help),
        command(LICENSE, |c| c.license),
        command(FORGET, |c| c.forget),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn data(name: &str, options: Value) -> Option<CommandData> {
        let mut data = json!({"id": "1", "name": name, "type": 1});
        data.as_object_mut()?.insert("options".into(), options);
        serde_json::from_value(data).ok()
    }

    fn parse(name: &str, options: Value, in_guild: bool) -> Option<Result<Command, Unparsed>> {
        data(name, options).map(|d| Command::parse(&d, in_guild))
    }

    /// The registration is the one poise 0.7 built from the attribute macros
    /// this module replaced, captured before the switch.
    #[test]
    fn the_registration_is_unchanged() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/discord/commands.json"))
                .unwrap_or_default();
        let built = serde_json::to_value(definitions()).unwrap_or_default();
        assert_eq!(built, fixture);
        assert!(!fixture.is_null());
    }

    /// Every registered command parses with its required options alone.
    #[test]
    fn every_registered_command_parses() {
        let built = serde_json::to_value(definitions()).unwrap_or_default();
        let commands = built.as_array().cloned().unwrap_or_default();
        assert_eq!(commands.len(), 6);
        for c in commands {
            let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
            let options: Vec<Value> = c
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|o| o.get("required").and_then(Value::as_bool) == Some(true))
                .map(|o| json!({"name": o.get("name"), "type": o.get("type"), "value": "x"}))
                .collect();
            let parsed = parse(name, Value::from(options), true);
            assert!(
                matches!(&parsed, Some(Ok(cmd)) if cmd.name() == name),
                "{name}: {parsed:?}"
            );
        }
    }

    #[test]
    fn options_are_read_and_typed() {
        let q = json!({"name": "question", "type": 3, "value": "Does it die?"});
        let private = |b: bool| json!({"name": "private", "type": 5, "value": b});
        assert_eq!(
            parse("judge", json!([q, private(true)]), true),
            Some(Ok(Command::Judge {
                question: "Does it die?".into(),
                audience: Audience::Private
            }))
        );
        assert_eq!(
            parse("judge", json!([q, private(false)]), true),
            Some(Ok(Command::Judge {
                question: "Does it die?".into(),
                audience: Audience::Channel
            }))
        );
        assert_eq!(
            parse("judge", json!([q]), false),
            Some(Err(Unparsed::NotInGuild))
        );
        assert_eq!(
            parse("judge", json!([private(true)]), true),
            Some(Err(Unparsed::BadOption {
                command: "judge",
                option: "question"
            }))
        );
        let name = json!({"name": "name", "type": 3, "value": "Bob"});
        assert_eq!(
            parse("card", json!([name, private(true)]), false),
            Some(Ok(Command::Card {
                name: "Bob".into(),
                private: true
            }))
        );
        let wrong = json!({"name": "private", "type": 3, "value": "yes"});
        assert_eq!(
            parse(
                "rule",
                json!([{"name": "id", "type": 3, "value": "702.19"}, wrong]),
                true
            ),
            Some(Err(Unparsed::BadOption {
                command: "rule",
                option: "private"
            }))
        );
        assert_eq!(
            parse("judgement", json!([]), true),
            Some(Err(Unparsed::Unknown("judgement".into())))
        );
    }

    #[test]
    fn only_help_license_and_forget_reply_privately_by_default() {
        let ephemeral: Vec<&str> = [
            Command::Judge {
                question: String::new(),
                audience: Audience::Channel,
            },
            Command::Card {
                name: String::new(),
                private: false,
            },
            Command::Rule {
                id: String::new(),
                private: false,
            },
            Command::Help,
            Command::License,
            Command::Forget,
        ]
        .iter()
        .filter(|c| c.ephemeral())
        .map(Command::name)
        .collect();
        assert_eq!(ephemeral, ["help", "license", "forget"]);
    }
}
