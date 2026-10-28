//! Replying to a slash command: the initial response, then follow-ups, all
//! over Discord's REST API, which needs no gateway connection.

use super::{
    rest::{Rest, RestError},
    wire::{AllowedMentions, Body, CommandInteraction, Embed, Response, Row},
};

/// A message for a command's reply. A follow-up after a deferred response
/// replaces the "thinking…" placeholder, and Discord fixed its visibility
/// when it was deferred.
#[derive(Clone, Debug, Default)]
pub struct Message {
    content: Option<String>,
    embeds: Vec<Embed>,
    components: Option<Vec<Row>>,
    ephemeral: Option<bool>,
    allowed_mentions: Option<AllowedMentions>,
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
    pub fn embed(mut self, embed: Embed) -> Self {
        self.embeds.push(embed);
        self
    }

    /// The message's button rows.
    #[must_use]
    pub fn components(mut self, rows: Vec<Row>) -> Self {
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
    pub const fn no_pings(mut self) -> Self {
        self.allowed_mentions = Some(AllowedMentions::NONE);
        self
    }

    /// What a [`Reply`] sends: the command's visibility when the message
    /// sets none, and no pings when it sets no mention rule.
    fn defaults(self, ephemeral: bool) -> Self {
        Self {
            ephemeral: self.ephemeral.or(Some(ephemeral)),
            allowed_mentions: self.allowed_mentions.or(Some(AllowedMentions::NONE)),
            ..self
        }
    }

    /// The message as sent, an initial response's or a follow-up's alike.
    fn body(self) -> Body {
        let mut b = Body::default().embeds(self.embeds);
        b.content = self.content;
        b.components = self.components;
        b.allowed_mentions = self.allowed_mentions;
        match self.ephemeral {
            Some(e) => b.ephemeral(e),
            None => b,
        }
    }
}

/// Tells whoever is waiting that an interaction's initial response has
/// been sent: the interactions endpoint ([`super::endpoint`]) answers
/// Discord's POST only then. [`Ack::none`] for an interaction from the
/// gateway, where nobody waits. It also knows when an interaction posted to
/// the endpoint arrived, so a handler can fit its work before the initial
/// response inside Discord's 3 s ([`Self::deadline`]).
#[derive(Clone, Debug, Default)]
pub struct Ack {
    notify: Option<std::sync::Arc<tokio::sync::Notify>>,
    arrived: Option<tokio::time::Instant>,
}

impl Ack {
    /// Nobody waits.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            notify: None,
            arrived: None,
        }
    }

    /// An acknowledgement [`Self::wait`] can wait for, for an interaction
    /// that arrived at `arrived`.
    #[must_use]
    pub fn arrived_at(arrived: tokio::time::Instant) -> Self {
        Self {
            notify: Some(std::sync::Arc::default()),
            arrived: Some(arrived),
        }
    }

    /// `budget` after the interaction arrived, or after now when its
    /// arrival is unknown (over the gateway).
    #[must_use]
    pub fn deadline(&self, budget: std::time::Duration) -> tokio::time::Instant {
        self.arrived.unwrap_or_else(tokio::time::Instant::now) + budget
    }

    /// The initial response has been sent, or its sending failed: either way
    /// nothing more will come of it.
    pub fn sent(&self) {
        if let Some(n) = &self.notify {
            // A permit is stored when nobody is waiting yet.
            n.notify_one();
        }
    }

    /// Until [`Self::sent`]; forever for [`Self::none`].
    pub async fn wait(&self) {
        match &self.notify {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    }
}

/// The replies to one slash command invocation.
pub struct Reply<'a> {
    rest: &'a Rest,
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
        rest: &'a Rest,
        interaction: &'a CommandInteraction,
        ephemeral: bool,
        ack: &'a Ack,
    ) -> Self {
        Self {
            rest,
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

    /// The initial response, told to the [`Ack`] whether or not it was sent.
    async fn initial(&mut self, response: &Response) -> Result<(), RestError> {
        let base = &self.interaction.base;
        let sent = self.rest.respond(base.id, &base.token, response).await;
        self.ack.sent();
        sent?;
        self.responded = true;
        Ok(())
    }

    /// Acknowledge the command, showing "thinking…" until the first
    /// [`send`](Self::send), to the asker alone when `ephemeral`. Does nothing
    /// once a response has been sent.
    ///
    /// # Errors
    /// Discord refused the acknowledgement or could not be reached.
    pub async fn defer(&mut self, ephemeral: bool) -> Result<(), RestError> {
        if !self.responded {
            self.initial(&Response::Defer { ephemeral }).await?;
        }
        Ok(())
    }

    /// Send `message`: as the initial response, or as a follow-up once one
    /// has been sent (a deferral included).
    ///
    /// # Errors
    /// Discord refused the message or could not be reached.
    pub async fn send(&mut self, message: Message) -> Result<(), RestError> {
        let body = message.defaults(self.ephemeral).body();
        if self.responded {
            let base = &self.interaction.base;
            self.rest
                .followup(base.application_id, &base.token, &body)
                .await
        } else {
            self.initial(&Response::Message(body)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn to_json(v: &impl serde::Serialize) -> Value {
        serde_json::to_value(v).unwrap_or_default()
    }

    /// A message sends what it was given, and a [`Reply`] makes one that
    /// sets no mention rule ping nobody.
    #[test]
    fn a_message_sets_only_what_it_was_given() {
        let none = json!({"parse": [], "users": [], "roles": []});
        let m = Message::text("hi").ephemeral(true);
        assert_eq!(
            to_json(&Response::Message(m.body())),
            json!({"type": 4, "data": {
                "content": "hi", "embeds": [], "allowed_mentions": none, "flags": 64
            }})
        );
        let bare = Message::default().content("hi").ephemeral(false);
        assert_eq!(
            to_json(&bare.body()),
            json!({"content": "hi", "embeds": [], "flags": 0})
        );
    }

    #[test]
    fn a_reply_fills_in_visibility_and_no_pings() {
        let sent = Message::default().content("hi").defaults(true).body();
        assert_eq!(
            to_json(&sent),
            json!({
                "content": "hi",
                "embeds": [],
                "allowed_mentions": {"parse": [], "users": [], "roles": []},
                "flags": 64
            })
        );
        let public = Message::default().ephemeral(false).defaults(true).body();
        assert_eq!(to_json(&public).get("flags"), Some(&json!(0)));
    }
}
