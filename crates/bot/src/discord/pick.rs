//! The clock side of a "did you mean…?" pick, kept pure. A pick is answered
//! from the message its buttons sit on ([`super::render::PickPrompt`]) and
//! the button's `custom_id` ([`super::ids::Pick`]), so how old it is comes
//! from the message too: Discord's own timestamps, which no user can set.
//!
//! A prompt was shown when its message was last edited, or, never edited,
//! when it was created (the time in its snowflake id). The last edit is the
//! one that counts because one message can show several prompts in turn: a
//! question with two ambiguous names is edited from the first prompt to
//! "Working on it…" to the second. The same time keys the single-use claim
//! (`pick_claims`), so the second prompt on a message is not refused as the
//! first one's double click.
//!
//! Neither depends on Discord. A [`Digest`] of the prompt's content is in
//! every button's `custom_id` and in the claim key: a click is answered only
//! from the prompt its button was made for (not from a later prompt on the
//! same message, nor from content the button was never on), and a second
//! prompt is a new claim even if Discord left its edit time unset. The
//! answer's [`audience`] must agree with the message's own visibility.

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest as _, Sha256};

use super::Audience;

/// How long a "did you mean…?" stays answerable, by default.
pub const DEFAULT_TTL: Duration = Duration::from_mins(10);

/// How long a claim row is kept (`pick_claims`, pruned as claims are taken).
/// Longer than any prompt is answerable, so a pruned claim's buttons have
/// expired by then and the expiry refuses them before the claim is tried.
pub const CLAIM_KEEP: Duration = Duration::from_hours(24);

const _: () = assert!(DEFAULT_TTL.as_secs() < CLAIM_KEEP.as_secs());

/// Discord's epoch, the first second of 2015, in Unix milliseconds.
pub const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// When the snowflake `id` was made, in Unix milliseconds: its top 42 bits
/// count milliseconds since [`DISCORD_EPOCH_MS`].
#[must_use]
pub fn snowflake_ms(id: u64) -> i64 {
    // 42 bits always fit in an i64.
    i64::try_from(id >> 22)
        .unwrap_or(i64::MAX)
        .saturating_add(DISCORD_EPOCH_MS)
}

/// When the prompt on message `id` was shown, in Unix milliseconds: its last
/// edit (`edited_ms`) if it has one, else its creation.
#[must_use]
pub fn shown_ms(id: u64, edited_ms: Option<i64>) -> i64 {
    edited_ms.unwrap_or_else(|| snowflake_ms(id))
}

/// Whether a prompt shown at `shown_ms` is past `ttl` at `now_ms`. A prompt
/// stamped ahead of this clock (skew between Discord and this host) is
/// fresh, not expired.
#[must_use]
pub fn expired(shown_ms: i64, now_ms: i64, ttl: Duration) -> bool {
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    now_ms.saturating_sub(shown_ms) >= ttl_ms
}

/// This host's clock in Unix milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The first 8 bytes of the SHA-256 of a prompt's message content: tells two
/// prompts apart and ties a button to the one it was made for. It is not the
/// text and cannot be turned back into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Digest([u8; 8]);

/// Hex digits in a [`Digest`] as a `custom_id` writes it.
pub const DIGEST_HEX: usize = 16;

impl Digest {
    /// The digest of `content`, exactly as sent and as Discord hands it back.
    #[must_use]
    pub fn of(content: &str) -> Self {
        let full = Sha256::digest(content.as_bytes());
        let mut first = [0u8; 8];
        for (to, from) in first.iter_mut().zip(full.iter()) {
            *to = *from;
        }
        Self(first)
    }

    /// Read [`DIGEST_HEX`] lowercase hex digits back.
    #[must_use]
    pub fn parse(hex: &str) -> Option<Self> {
        if hex.len() != DIGEST_HEX
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        u64::from_str_radix(hex, 16)
            .ok()
            .map(|n| Self(n.to_be_bytes()))
    }

    /// The same bits as a signed integer, for a `bigint` column.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        i64::from_be_bytes(self.0)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", u64::from_be_bytes(self.0))
    }
}

/// Who a pick answers for: the button's `audience` when the message's own
/// visibility agrees with it (`ephemeral`: its EPHEMERAL flag), `None` when
/// they disagree (the click is refused). A message whose flags did not arrive
/// is answered privately whatever the button says, so a private question can
/// never be read as a channel one: at worst a channel question goes
/// unrecorded.
#[must_use]
pub const fn audience(button: Audience, ephemeral: Option<bool>) -> Option<Audience> {
    match (button, ephemeral) {
        (Audience::Private, Some(true)) | (_, None) => Some(Audience::Private),
        (Audience::Channel, Some(false)) => Some(Audience::Channel),
        (Audience::Private, Some(false)) | (Audience::Channel, Some(true)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_sixteen_hex_digits_and_reads_back() {
        let d = Digest::of("<@1> asked: q\n\nI'm not sure");
        let hex = d.to_string();
        assert_eq!(hex.len(), DIGEST_HEX);
        assert_eq!(Digest::parse(&hex), Some(d));
        assert_ne!(Digest::of("a"), Digest::of("b"));
        // SHA-256("") begins e3b0c442 98fc1c14.
        assert_eq!(Digest::of("").to_string(), "e3b0c44298fc1c14");
        assert_eq!(
            Digest::of("").as_i64(),
            0xe3b0_c442_98fc_1c14_u64.cast_signed()
        );
        for bad in [
            "",
            "e3b0c44298fc1c1",
            "e3b0c44298fc1c145",
            "E3B0C44298FC1C14",
            "+3b0c44298fc1c14",
            "g3b0c44298fc1c14",
        ] {
            assert_eq!(Digest::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_audience_must_agree_with_the_message() {
        use Audience::{Channel, Private};
        assert_eq!(audience(Private, Some(true)), Some(Private));
        assert_eq!(audience(Channel, Some(false)), Some(Channel));
        assert_eq!(audience(Private, Some(false)), None);
        assert_eq!(audience(Channel, Some(true)), None);
        // Unknown visibility: never recorded.
        assert_eq!(audience(Channel, None), Some(Private));
        assert_eq!(audience(Private, None), Some(Private));
    }

    #[test]
    fn a_snowflake_carries_its_creation_time() {
        // Discord's documented example: 175928847299117063 was made at
        // 2016-04-30 11:18:25.796 UTC.
        assert_eq!(snowflake_ms(175_928_847_299_117_063), 1_462_015_105_796);
        assert_eq!(snowflake_ms(0), DISCORD_EPOCH_MS);
        assert!(snowflake_ms(u64::MAX) > DISCORD_EPOCH_MS);
    }

    #[test]
    fn the_last_edit_is_when_the_prompt_was_shown() {
        let id = 175_928_847_299_117_063;
        assert_eq!(shown_ms(id, None), 1_462_015_105_796);
        assert_eq!(shown_ms(id, Some(1_462_015_200_000)), 1_462_015_200_000);
    }

    #[test]
    fn a_prompt_expires_at_its_ttl() {
        let shown = snowflake_ms(175_928_847_299_117_063);
        let ttl = DEFAULT_TTL;
        let ms = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        assert!(!expired(shown, shown, ttl));
        assert!(!expired(shown, shown + ms(ttl) - 1, ttl));
        assert!(expired(shown, shown + ms(ttl), ttl));
        assert!(expired(shown, i64::MAX, ttl));
        // Stamped ahead of this clock: fresh.
        assert!(!expired(shown, shown - 60_000, ttl));
        assert!(!expired(i64::MAX, i64::MIN, ttl));
    }
}
