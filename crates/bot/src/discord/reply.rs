//! Replying to a slash command: the initial response, then follow-ups, all
//! over Discord's REST API, which needs no gateway connection.

use serenity::all::{
    CommandInteraction, CreateActionRow, CreateAllowedMentions, CreateEmbed,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    Http,
};

/// A message for a command's reply. A follow-up after a deferred response
/// replaces the "thinking…" placeholder, and Discord fixed its visibility
/// when it was deferred.
#[derive(Clone, Debug, Default)]
pub struct Message {
    content: Option<String>,
    embeds: Vec<CreateEmbed>,
    components: Option<Vec<CreateActionRow>>,
    ephemeral: Option<bool>,
    allowed_mentions: Option<CreateAllowedMentions>,
}

impl Message {
    /// Plain text that pings nobody.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self::default().content(content).no_pings()
    }

    /// The message's text.
    #[must_use]
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// Add an embed.
    #[must_use]
    pub fn embed(mut self, embed: CreateEmbed) -> Self {
        self.embeds.push(embed);
        self
    }

    /// The message's button rows.
    #[must_use]
    pub fn components(mut self, rows: Vec<CreateActionRow>) -> Self {
        self.components = Some(rows);
        self
    }

    /// Whether the message is shown to the asker alone, overriding the
    /// command's default.
    #[must_use]
    pub const fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = Some(ephemeral);
        self
    }

    /// Allowed-mentions with an empty parse list: a mention in the text
    /// renders but pings nobody.
    #[must_use]
    pub fn no_pings(mut self) -> Self {
        self.allowed_mentions = Some(CreateAllowedMentions::new());
        self
    }

    /// What a [`Reply`] sends: the command's visibility when the message
    /// sets none, and no pings when it sets no mention rule.
    fn defaults(self, ephemeral: bool) -> Self {
        Self {
            ephemeral: self.ephemeral.or(Some(ephemeral)),
            allowed_mentions: self
                .allowed_mentions
                .or_else(|| Some(CreateAllowedMentions::new())),
            ..self
        }
    }

    fn initial(self) -> CreateInteractionResponseMessage {
        let mut b = CreateInteractionResponseMessage::new();
        if let Some(c) = self.content {
            b = b.content(c);
        }
        if let Some(m) = self.allowed_mentions {
            b = b.allowed_mentions(m);
        }
        if let Some(rows) = self.components {
            b = b.components(rows);
        }
        if let Some(e) = self.ephemeral {
            b = b.ephemeral(e);
        }
        b.embeds(self.embeds)
    }

    fn followup(self) -> CreateInteractionResponseFollowup {
        let mut b = CreateInteractionResponseFollowup::new();
        if let Some(c) = self.content {
            b = b.content(c);
        }
        b = b.embeds(self.embeds);
        if let Some(rows) = self.components {
            b = b.components(rows);
        }
        if let Some(m) = self.allowed_mentions {
            b = b.allowed_mentions(m);
        }
        if let Some(e) = self.ephemeral {
            b = b.ephemeral(e);
        }
        b
    }
}

/// Tells whoever is waiting that an interaction's initial response has
/// been sent: the interactions endpoint ([`super::endpoint`]) answers
/// Discord's POST only then. [`Ack::none`] for an interaction from the
/// gateway, where nobody waits.
#[derive(Clone, Debug, Default)]
pub struct Ack(Option<std::sync::Arc<tokio::sync::Notify>>);

impl Ack {
    /// Nobody waits.
    #[must_use]
    pub const fn none() -> Self {
        Self(None)
    }

    /// An acknowledgement [`Self::wait`] can wait for.
    #[must_use]
    pub fn new() -> Self {
        Self(Some(std::sync::Arc::default()))
    }

    /// The initial response has been sent, or its sending failed: either way
    /// nothing more will come of it.
    pub fn sent(&self) {
        if let Some(n) = &self.0 {
            // A permit is stored when nobody is waiting yet.
            n.notify_one();
        }
    }

    /// Until [`Self::sent`]; forever for [`Self::none`].
    pub async fn wait(&self) {
        match &self.0 {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    }
}

/// The replies to one slash command invocation.
pub struct Reply<'a> {
    http: &'a Http,
    interaction: &'a CommandInteraction,
    ack: &'a Ack,
    /// The visibility of a reply that does not set its own.
    ephemeral: bool,
    /// Whether the initial response (a message or a deferral) has been sent:
    /// every reply after it is a follow-up.
    responded: bool,
}

impl<'a> Reply<'a> {
    /// Replies to `interaction`, private by default when `ephemeral`; `ack`
    /// hears of the initial response.
    #[must_use]
    pub const fn new(
        http: &'a Http,
        interaction: &'a CommandInteraction,
        ephemeral: bool,
        ack: &'a Ack,
    ) -> Self {
        Self {
            http,
            interaction,
            ack,
            ephemeral,
            responded: false,
        }
    }

    /// Treat the initial response as sent already (by another `Reply` for
    /// the same interaction), so the next [`send`](Self::send) is a
    /// follow-up.
    pub const fn responded(&mut self) {
        self.responded = true;
    }

    /// Acknowledge the command, showing "thinking…" until the first
    /// [`send`](Self::send), to the asker alone when `ephemeral`. Does nothing
    /// once a response has been sent.
    ///
    /// # Errors
    /// Discord refused the acknowledgement or could not be reached.
    pub async fn defer(&mut self, ephemeral: bool) -> serenity::Result<()> {
        if !self.responded {
            let defer = CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(ephemeral),
            );
            let sent = self.interaction.create_response(self.http, defer).await;
            self.ack.sent();
            sent?;
            self.responded = true;
        }
        Ok(())
    }

    /// Send `message`: as the initial response, or as a follow-up once one
    /// has been sent (a deferral included).
    ///
    /// # Errors
    /// Discord refused the message or could not be reached.
    pub async fn send(&mut self, message: Message) -> serenity::Result<()> {
        let message = message.defaults(self.ephemeral);
        if self.responded {
            self.interaction
                .create_followup(self.http, message.followup())
                .await?;
        } else {
            let sent = self
                .interaction
                .create_response(
                    self.http,
                    CreateInteractionResponse::Message(message.initial()),
                )
                .await;
            self.ack.sent();
            sent?;
            self.responded = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn to_json(v: &impl serde::Serialize) -> Value {
        serde_json::to_value(v).unwrap_or_default()
    }

    /// The shapes poise 0.7's `CreateReply` produced for the same settings
    /// (compared across every combination of them before the switch). One
    /// difference is on purpose: [`Reply::send`] makes a message that sets
    /// no mention rule ping nobody, where poise allowed user mentions.
    #[test]
    fn a_message_sets_only_what_it_was_given() {
        let m = Message::text("hi").ephemeral(true);
        let none = json!({"parse": [], "users": [], "roles": []});
        assert_eq!(
            to_json(&CreateInteractionResponse::Message(m.clone().initial())),
            json!({"type": 4, "data": {
                "content": "hi",
                "embeds": [],
                "allowed_mentions": none,
                "flags": 64,
                "attachments": []
            }})
        );
        assert_eq!(
            to_json(&m.followup()),
            json!({
                "content": "hi",
                "embeds": [],
                "allowed_mentions": none,
                "flags": 64,
                "attachments": []
            })
        );
        let bare = Message::default().content("hi").ephemeral(false);
        assert_eq!(
            to_json(&bare.followup()),
            json!({"content": "hi", "embeds": [], "flags": 0, "attachments": []})
        );
    }

    #[test]
    fn a_reply_fills_in_visibility_and_no_pings() {
        let sent = Message::default().content("hi").defaults(true).followup();
        assert_eq!(
            to_json(&sent),
            json!({
                "content": "hi",
                "embeds": [],
                "allowed_mentions": {"parse": [], "users": [], "roles": []},
                "flags": 64,
                "attachments": []
            })
        );
        let public = Message::default()
            .ephemeral(false)
            .defaults(true)
            .followup();
        assert_eq!(to_json(&public).get("flags"), Some(&json!(0)));
    }
}
