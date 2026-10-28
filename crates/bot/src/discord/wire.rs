//! Discord's wire format, as much of it as the bot reads and writes (API
//! v10). The types are ours, as `judge-anthropic`'s are: an interaction is
//! read from Discord's own JSON with only the fields the bot uses, and every
//! response, message and command registration is written as Discord
//! documents it.
//!
//! Reading keeps what a library would drop: a modal submission's
//! `data.components` stays the JSON Discord sent ([`ModalData::components`]),
//! so a Label (component type 18) arrives whole however the interaction was
//! delivered ([`super::feedback`]). An unknown field is ignored, never an
//! error, so Discord adding one breaks nothing.

use std::{collections::BTreeMap, fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;

/// Discord ids: snowflakes, sent as decimal strings, never zero.
macro_rules! id {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// The id `n`.
            #[must_use]
            pub const fn new(n: NonZeroU64) -> Self {
                Self(n)
            }

            /// The id as a number.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl From<NonZeroU64> for $name {
            fn from(n: NonZeroU64) -> Self {
                Self(n)
            }
        }

        impl From<$name> for NonZeroU64 {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                snowflake(d).map(Self)
            }
        }
    )+};
}

id! {
    /// An application (the bot's).
    ApplicationId,
    /// A channel.
    ChannelId,
    /// An application emoji.
    EmojiId,
    /// A server.
    GuildId,
    /// One interaction.
    InteractionId,
    /// A message.
    MessageId,
    /// A role.
    RoleId,
    /// A user.
    UserId,
}

/// A snowflake as Discord sends it (a string), or as a number.
fn snowflake<'de, D: Deserializer<'de>>(d: D) -> Result<NonZeroU64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Number(u64),
    }
    let n = match Raw::deserialize(d)? {
        Raw::Text(s) => s
            .parse::<u64>()
            .map_err(|_| D::Error::custom(format!("not a snowflake: {s:?}")))?,
        Raw::Number(n) => n,
    };
    NonZeroU64::new(n).ok_or_else(|| D::Error::custom("a snowflake of zero"))
}

/// An interaction's token: what its replies are addressed with, for 15
/// minutes. Never printed.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// The token, for the URL of a reply.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// A user, as an interaction names them.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct User {
    /// Their id.
    pub id: UserId,
    /// Their unique username.
    #[serde(rename = "username")]
    pub name: String,
}

/// The invoking member, in a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The roles they hold.
    pub roles: Vec<RoleId>,
}

/// What every interaction carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    /// The interaction.
    pub id: InteractionId,
    /// The application it is for: replies are sent as it.
    pub application_id: ApplicationId,
    /// What its replies are addressed with.
    pub token: Token,
    /// The server, when it was invoked in one.
    pub guild_id: Option<GuildId>,
    /// The channel it was invoked in.
    pub channel_id: Option<ChannelId>,
    /// The invoking user's client language (`en-US`), or empty.
    pub locale: String,
    /// Who invoked it: the member's user in a server, the user elsewhere.
    pub user: User,
    /// The member, in a server.
    pub member: Option<Member>,
}

/// The fields of [`Base`] as Discord sends them: the user is inside the
/// member in a server, and top-level elsewhere.
#[derive(Deserialize)]
struct RawBase {
    id: InteractionId,
    application_id: ApplicationId,
    token: Token,
    #[serde(default)]
    guild_id: Option<GuildId>,
    #[serde(default)]
    channel_id: Option<ChannelId>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    member: Option<RawMember>,
}

#[derive(Deserialize)]
struct RawMember {
    user: User,
    #[serde(default)]
    roles: Vec<RoleId>,
}

impl TryFrom<RawBase> for Base {
    type Error = String;

    fn try_from(r: RawBase) -> Result<Self, String> {
        let (user, member) = match (r.member, r.user) {
            (Some(m), _) => (m.user, Some(Member { roles: m.roles })),
            (None, Some(u)) => (u, None),
            (None, None) => return Err("an interaction with neither a member nor a user".into()),
        };
        Ok(Self {
            id: r.id,
            application_id: r.application_id,
            token: r.token,
            guild_id: r.guild_id,
            channel_id: r.channel_id,
            locale: r.locale.unwrap_or_default(),
            user,
            member,
        })
    }
}

impl<'de> Deserialize<'de> for Base {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(RawBase::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// An interaction the bot answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Interaction {
    /// Discord checking the endpoint (type 1).
    Ping,
    /// A slash command (type 2).
    Command(CommandInteraction),
    /// A button or menu (type 3).
    Component(ComponentInteraction),
    /// A submitted modal (type 5).
    Modal(ModalInteraction),
    /// Another type (autocomplete, which is never registered, or a new one).
    Other(u8),
}

impl Interaction {
    /// The interaction in `value`, Discord's JSON.
    ///
    /// # Errors
    /// No `type`, or a known type whose fields do not read.
    pub fn from_value(value: Value) -> serde_json::Result<Self> {
        #[derive(Deserialize)]
        struct Kind {
            #[serde(rename = "type")]
            kind: u8,
        }
        let Kind { kind } = Kind::deserialize(&value)?;
        Ok(match kind {
            1 => Self::Ping,
            2 => Self::Command(serde_json::from_value(value)?),
            3 => Self::Component(serde_json::from_value(value)?),
            5 => Self::Modal(serde_json::from_value(value)?),
            other => Self::Other(other),
        })
    }

    /// The interaction in `body`, a request's bytes.
    ///
    /// # Errors
    /// As [`Self::from_value`], or no JSON.
    pub fn from_slice(body: &[u8]) -> serde_json::Result<Self> {
        Self::from_value(serde_json::from_slice(body)?)
    }
}

/// A slash command invocation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CommandInteraction {
    /// What every interaction carries.
    #[serde(flatten)]
    pub base: Base,
    /// The command and its options.
    pub data: CommandData,
}

/// Which command, with which options.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CommandData {
    /// The command's name.
    pub name: String,
    /// Its options, as typed.
    #[serde(default)]
    pub options: Vec<CommandOption>,
}

/// One option of an invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandOption {
    /// The option's name.
    pub name: String,
    /// Its value.
    pub value: OptionValue,
}

/// An option's value, by its type.
#[derive(Clone, Debug, PartialEq)]
pub enum OptionValue {
    /// Type 3.
    String(String),
    /// Type 5.
    Boolean(bool),
    /// Any other type, or a value that does not match its type.
    Other,
}

impl<'de> Deserialize<'de> for CommandOption {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            #[serde(rename = "type")]
            kind: u8,
            #[serde(default)]
            value: Value,
        }
        let r = Raw::deserialize(d)?;
        let value = match (r.kind, r.value) {
            (3, Value::String(s)) => OptionValue::String(s),
            (5, Value::Bool(b)) => OptionValue::Boolean(b),
            _ => OptionValue::Other,
        };
        Ok(Self {
            name: r.name,
            value,
        })
    }
}

/// A button or menu clicked on a message.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ComponentInteraction {
    /// What every interaction carries.
    #[serde(flatten)]
    pub base: Base,
    /// The component.
    pub data: ComponentData,
    /// The message it is on.
    pub message: ComponentMessage,
}

/// Which component, and what a menu chose.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ComponentData {
    /// The component's `custom_id`.
    pub custom_id: String,
    /// The component's type (2 a button, 3 a string menu).
    pub component_type: u8,
    /// A menu's chosen values; empty for a button (sent absent or `null`).
    #[serde(default, deserialize_with = "null_is_empty")]
    pub values: Vec<String>,
}

/// A list that may come as `null`: empty.
fn null_is_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// A bot token as an operator may write it: trimmed, and without the
/// `Bot ` prefix the `Authorization` header adds (Discord's own examples
/// show it), so `Bot xyz` and `xyz` are the same token.
#[must_use]
pub fn bare_token(raw: &str) -> &str {
    let t = raw.trim();
    // "Bot " trimmed to "Bot" is a prefix with no token: empty.
    if t == "Bot" {
        return "";
    }
    t.strip_prefix("Bot ").map_or(t, str::trim_start)
}

/// The message a component is on: the fields a click is checked against.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ComponentMessage {
    /// The message.
    pub id: MessageId,
    /// Its text. The bot's own message arrives with it, without the
    /// message-content intent.
    #[serde(default)]
    pub content: String,
    /// Its flags, when sent.
    #[serde(default)]
    pub flags: Option<u64>,
    /// When it was last edited (ISO 8601), if ever.
    #[serde(default)]
    pub edited_timestamp: Option<String>,
}

impl ComponentMessage {
    /// Whether the message is shown to one user alone; `None` when its
    /// flags were not sent.
    #[must_use]
    pub fn ephemeral(&self) -> Option<bool> {
        self.flags.map(|f| f & EPHEMERAL != 0)
    }

    /// When it was last edited, in Unix milliseconds; `None` when it never
    /// was (or the time does not read).
    #[must_use]
    pub fn edited_ms(&self) -> Option<i64> {
        let at = self.edited_timestamp.as_deref()?;
        let parsed =
            time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()?;
        i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
    }
}

/// A submitted modal.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModalInteraction {
    /// What every interaction carries.
    #[serde(flatten)]
    pub base: Base,
    /// The form.
    pub data: ModalData,
}

/// Which form, and what it holds.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModalData {
    /// The form's `custom_id`.
    pub custom_id: String,
    /// The submitted components, as Discord sent them.
    #[serde(default)]
    pub components: Value,
}

/// Each interaction reads as what every interaction carries
/// (`c.user`, `c.token`).
macro_rules! base {
    ($($t:ty),+) => {$(
        impl std::ops::Deref for $t {
            type Target = Base;

            fn deref(&self) -> &Base {
                &self.base
            }
        }
    )+};
}

base!(CommandInteraction, ComponentInteraction, ModalInteraction);

impl Base {
    /// The thread its history is kept under: the channel, else (no channel
    /// sent) the interaction alone, so it shares no history.
    #[must_use]
    pub fn thread_id(&self) -> String {
        self.channel_id
            .map_or_else(|| format!("interaction:{}", self.id), |c| c.to_string())
    }
}

/// The message flag that shows a message to one user alone.
pub const EPHEMERAL: u64 = 1 << 6;

/// An interaction's initial response.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    /// A message (type 4).
    Message(Body),
    /// "Thinking…" until a follow-up or an edit (type 5), to the invoker
    /// alone when `ephemeral`.
    Defer {
        /// Shown to the invoker alone.
        ephemeral: bool,
    },
    /// A click acknowledged, its message left as it is for now (type 6).
    Acknowledge,
    /// The clicked message replaced (type 7).
    Update(Body),
    /// A modal, as written by [`super::feedback::modal`]: the whole
    /// response, `type` 9 included.
    Modal(Value),
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a, T: Serialize> {
            #[serde(rename = "type")]
            kind: u8,
            #[serde(skip_serializing_if = "Option::is_none")]
            data: Option<&'a T>,
        }
        match self {
            Self::Message(b) => Out {
                kind: 4,
                data: Some(b),
            }
            .serialize(s),
            Self::Defer { ephemeral } => Out {
                kind: 5,
                data: Some(&Body::default().ephemeral(*ephemeral)),
            }
            .serialize(s),
            Self::Acknowledge => Out::<Body> {
                kind: 6,
                data: None,
            }
            .serialize(s),
            Self::Update(b) => Out {
                kind: 7,
                data: Some(b),
            }
            .serialize(s),
            Self::Modal(v) => v.serialize(s),
        }
    }
}

/// A message's fields, for a response, a follow-up or an edit. A field
/// left `None` is not sent: on an edit or update that keeps what the
/// message has, and `Some(vec![])` clears it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Body {
    /// The text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The embeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeds: Option<Vec<Embed>>,
    /// The component rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<Row>>,
    /// The flags ([`EPHEMERAL`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u64>,
    /// Who a mention may ping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
}

impl Body {
    /// The text.
    #[must_use]
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// The embeds (an empty list clears them on an edit).
    #[must_use]
    pub fn embeds(mut self, embeds: Vec<Embed>) -> Self {
        self.embeds = Some(embeds);
        self
    }

    /// The component rows (an empty list clears them on an edit).
    #[must_use]
    pub fn components(mut self, rows: Vec<Row>) -> Self {
        self.components = Some(rows);
        self
    }

    /// Shown to the invoker alone, or to the channel. Discord reads the flag
    /// on a new message only.
    #[must_use]
    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.flags = Some(if ephemeral { EPHEMERAL } else { 0 });
        self
    }

    /// A mention in the text renders but pings nobody.
    #[must_use]
    pub fn no_pings(mut self) -> Self {
        self.allowed_mentions = Some(AllowedMentions::NONE);
        self
    }
}

/// Who a message's mentions may ping. Only "nobody" is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AllowedMentions {
    parse: [&'static str; 0],
    users: [u64; 0],
    roles: [u64; 0],
}

impl AllowedMentions {
    /// Nobody: an empty parse list and no users or roles.
    pub const NONE: Self = Self {
        parse: [],
        users: [],
        roles: [],
    };
}

/// An embed: the parts the bot fills in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Embed {
    /// The title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Where the title links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The footer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<Footer>,
}

/// An embed's footer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Footer {
    /// Its text.
    pub text: String,
}

/// A row of components (type 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Up to five buttons.
    Buttons(Vec<Button>),
}

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a, T: Serialize> {
            #[serde(rename = "type")]
            kind: u8,
            components: &'a [T],
        }
        match self {
            Self::Buttons(b) => Out {
                kind: 1,
                components: b,
            }
            .serialize(s),
        }
    }
}

/// How a button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Blurple.
    Primary = 1,
    /// Grey.
    Secondary = 2,
    /// Green.
    Success = 3,
    /// Red.
    Danger = 4,
}

impl Serialize for Style {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

/// A button (type 2) that sends its `custom_id` back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    /// What comes back on a click.
    pub custom_id: String,
    /// Its text.
    pub label: String,
    /// How it looks.
    pub style: Style,
}

impl Button {
    /// A button.
    #[must_use]
    pub fn new(custom_id: impl Into<String>, label: impl Into<String>, style: Style) -> Self {
        Self {
            custom_id: custom_id.into(),
            label: label.into(),
            style,
        }
    }
}

impl Serialize for Button {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            #[serde(rename = "type")]
            kind: u8,
            style: Style,
            label: &'a str,
            custom_id: &'a str,
        }
        Out {
            kind: 2,
            style: self.style,
            label: &self.label,
            custom_id: &self.custom_id,
        }
        .serialize(s)
    }
}

/// A slash command as registered (`PUT …/commands`), in the shape the
/// registration fixture pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommandDef {
    /// The command's name.
    pub name: &'static str,
    /// Names by Discord locale tag (none: the name is not translated).
    pub name_localizations: BTreeMap<&'static str, &'static str>,
    /// The default description.
    pub description: &'static str,
    /// Descriptions by Discord locale tag.
    pub description_localizations: BTreeMap<&'static str, &'static str>,
    /// Its options.
    pub options: Vec<OptionDef>,
    /// Where it may be invoked (0: a server); not sent when anywhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contexts: Option<Vec<u8>>,
    /// Never age-restricted.
    pub nsfw: bool,
}

/// One option of a registered command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OptionDef {
    /// Its type (3: a string).
    #[serde(rename = "type")]
    pub kind: u8,
    /// The option's name.
    pub name: &'static str,
    /// The default description.
    pub description: &'static str,
    /// Descriptions by Discord locale tag (sent only when there are some).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub description_localizations: BTreeMap<&'static str, &'static str>,
    /// Whether it must be given.
    pub required: bool,
    /// No fixed choices.
    pub choices: [u8; 0],
    /// No sub-options.
    pub options: [u8; 0],
    /// Any channel.
    pub channel_types: [u8; 0],
    /// No numeric bounds.
    pub min_value: Option<u8>,
    /// No numeric bounds.
    pub max_value: Option<u8>,
    /// No minimum length.
    pub min_length: Option<u16>,
    /// The longest a string may be.
    pub max_length: Option<u16>,
    /// No autocomplete.
    pub autocomplete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(kind: u8) -> Value {
        json!({
            "id": "11", "application_id": "22", "type": kind, "token": "secret-token",
            "version": 1, "guild_id": "33", "channel_id": "44", "locale": "fr",
            "member": {"user": {"id": "55", "username": "alice", "global_name": "A"},
                       "roles": ["66", "77"], "nick": null},
            "a_field_from_the_future": {"x": 1}
        })
    }

    fn with(mut v: Value, key: &str, value: Value) -> Value {
        if let Some(o) = v.as_object_mut() {
            o.insert(key.into(), value);
        }
        v
    }

    #[test]
    fn a_command_reads_its_member_options_and_ids() {
        let v = with(
            base(2),
            "data",
            json!({"id": "9", "name": "judge", "type": 1, "options": [
                {"name": "question", "type": 3, "value": "Does it die?"},
                {"name": "private", "type": 5, "value": true},
                {"name": "n", "type": 4, "value": 3}
            ]}),
        );
        let Ok(Interaction::Command(c)) = Interaction::from_value(v) else {
            unreachable!("a command");
        };
        assert_eq!(c.base.user.id.get(), 55);
        assert_eq!(c.base.user.name, "alice");
        assert_eq!(c.base.guild_id.map(GuildId::get), Some(33));
        assert_eq!(c.base.channel_id.map(|i| i.to_string()), Some("44".into()));
        assert_eq!(c.base.locale, "fr");
        assert_eq!(
            c.base
                .member
                .map(|m| m.roles.into_iter().map(RoleId::get).collect::<Vec<_>>()),
            Some(vec![66, 77])
        );
        assert_eq!(c.base.token.expose(), "secret-token");
        assert!(!format!("{:?}", c.base.token).contains("secret"));
        let values: Vec<OptionValue> = c.data.options.into_iter().map(|o| o.value).collect();
        assert_eq!(
            values,
            [
                OptionValue::String("Does it die?".into()),
                OptionValue::Boolean(true),
                OptionValue::Other
            ]
        );
    }

    #[test]
    fn a_dm_names_its_user_at_the_top_level() {
        let mut v = with(base(2), "user", json!({"id": "88", "username": "bob"}));
        if let Some(o) = v.as_object_mut() {
            o.remove("member");
            o.remove("guild_id");
        }
        let v = with(v, "data", json!({"name": "card"}));
        let Ok(Interaction::Command(c)) = Interaction::from_value(v) else {
            unreachable!("a command");
        };
        assert_eq!((c.base.user.id.get(), c.base.member), (88, None));
        assert!(c.data.options.is_empty());
    }

    #[test]
    fn a_click_reads_its_message() {
        let v = with(
            with(
                base(3),
                "data",
                json!({"custom_id": "rate:x", "component_type": 3, "values": ["wrong"]}),
            ),
            "message",
            json!({"id": "99", "content": "<@55> asked: q", "flags": 64,
                   "edited_timestamp": "2026-10-28T12:34:56.789000+00:00"}),
        );
        let Ok(Interaction::Component(c)) = Interaction::from_value(v) else {
            unreachable!("a component");
        };
        assert_eq!(c.data.values, ["wrong"]);
        let button: ComponentData =
            serde_json::from_value(json!({"custom_id": "b", "component_type": 2, "values": null}))
                .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(button.values.is_empty());
        assert_eq!(bare_token(" Bot  abc \r"), "abc");
        assert_eq!(bare_token("abc"), "abc");
        assert_eq!(bare_token("Bot "), "");
        assert_eq!(c.message.ephemeral(), Some(true));
        assert_eq!(c.message.edited_ms(), Some(1_793_190_896_789));
        let unedited = ComponentMessage {
            id: c.message.id,
            content: String::new(),
            flags: None,
            edited_timestamp: None,
        };
        assert_eq!((unedited.ephemeral(), unedited.edited_ms()), (None, None));
    }

    #[test]
    fn a_modal_keeps_its_components_as_sent() {
        let components = json!([{"type": 18, "component": {"type": 3, "custom_id": "category",
                                 "values": ["wrong_rule"]}}]);
        let v = with(
            base(5),
            "data",
            json!({"custom_id": "rate-form:x", "components": components}),
        );
        let Ok(Interaction::Modal(m)) = Interaction::from_value(v) else {
            unreachable!("a modal");
        };
        assert_eq!(m.data.components, components);
    }

    #[test]
    fn other_kinds_and_bad_bodies() {
        assert_eq!(
            Interaction::from_value(json!({"type": 1})).ok(),
            Some(Interaction::Ping)
        );
        assert_eq!(
            Interaction::from_value(base(4)).ok(),
            Some(Interaction::Other(4))
        );
        assert!(Interaction::from_value(json!({"id": "1"})).is_err());
        assert!(Interaction::from_value(with(base(2), "id", json!("0"))).is_err());
        assert!(Interaction::from_slice(b"not json").is_err());
    }

    #[test]
    fn responses_are_written_as_discord_documents_them() {
        let body = Body::default()
            .content("hi")
            .ephemeral(true)
            .no_pings()
            .components(vec![Row::Buttons(vec![Button::new(
                "a",
                "A",
                Style::Danger,
            )])]);
        assert_eq!(
            serde_json::to_value(Response::Message(body)).ok(),
            Some(json!({"type": 4, "data": {
                "content": "hi", "flags": 64,
                "allowed_mentions": {"parse": [], "users": [], "roles": []},
                "components": [{"type": 1, "components": [
                    {"type": 2, "style": 4, "label": "A", "custom_id": "a"}]}]
            }}))
        );
        assert_eq!(
            serde_json::to_value(Response::Defer { ephemeral: false }).ok(),
            Some(json!({"type": 5, "data": {"flags": 0}}))
        );
        assert_eq!(
            serde_json::to_value(Response::Acknowledge).ok(),
            Some(json!({"type": 6}))
        );
        assert_eq!(
            serde_json::to_value(Response::Update(
                Body::default().embeds(vec![]).components(vec![])
            ))
            .ok(),
            Some(json!({"type": 7, "data": {"embeds": [], "components": []}}))
        );
        let modal = json!({"type": 9, "data": {"custom_id": "f"}});
        assert_eq!(
            serde_json::to_value(Response::Modal(modal.clone())).ok(),
            Some(modal)
        );
    }
}
