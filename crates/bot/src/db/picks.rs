//! The single-use claim on a "did you mean?" prompt (`pick_claims`). The
//! prompt itself is in Discord (`discord::render::PickPrompt`); this is the
//! only state a pick keeps, and it holds no text.

use judge_core::JudgeError;
use sqlx::PgPool;

use super::upstream;

/// Which prompt a claim is for: the message it was on, when it was shown
/// (`discord::pick::shown_ms`) and the digest of its content
/// (`discord::pick::Digest::as_i64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickKey {
    /// The Discord message id.
    pub message_id: u64,
    /// When the prompt was shown, in Unix milliseconds.
    pub shown_ms: i64,
    /// The digest of the prompt's content.
    pub digest: i64,
}

impl PickKey {
    fn message(self) -> Result<i64, JudgeError> {
        i64::try_from(self.message_id).map_err(|_| {
            JudgeError::Upstream(anyhow::anyhow!(
                "message id {} is past i64",
                self.message_id
            ))
        })
    }
}

/// Take the pick of the prompt `key`. `true` for the first claim, `false` for
/// every later one. The same statement deletes claims older than a day
/// (`discord::pick::CLAIM_KEEP`), so the table holds about a day of picks.
///
/// # Errors
/// `Upstream` from sqlx, or a message id past `i64` (Discord's never are).
pub async fn claim_pick(pool: &PgPool, key: PickKey) -> Result<bool, JudgeError> {
    let message_id = key.message()?;
    let claimed = sqlx::query_scalar!(
        r#"
        WITH pruned AS (
            DELETE FROM pick_claims WHERE claimed_at < now() - interval '1 day'
        )
        INSERT INTO pick_claims (message_id, shown_ms, digest) VALUES ($1, $2, $3)
        ON CONFLICT DO NOTHING
        RETURNING message_id
        "#,
        message_id,
        key.shown_ms,
        key.digest,
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("claim a pick"))?;
    Ok(claimed.is_some())
}

/// Give a claim back: the pick could not be acknowledged, so its buttons are
/// still on the message and the next click must be able to claim it.
///
/// # Errors
/// As [`claim_pick`].
pub async fn release_pick(pool: &PgPool, key: PickKey) -> Result<(), JudgeError> {
    let message_id = key.message()?;
    sqlx::query!(
        "DELETE FROM pick_claims WHERE message_id = $1 AND shown_ms = $2 AND digest = $3",
        message_id,
        key.shown_ms,
        key.digest,
    )
    .execute(pool)
    .await
    .map_err(upstream("release a pick"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn key(message_id: u64, shown_ms: i64, digest: i64) -> PickKey {
        PickKey {
            message_id,
            shown_ms,
            digest,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_first_claim_wins_and_a_new_prompt_is_a_new_claim(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        assert!(claim_pick(&pool, key(7, 1_000, 1)).await?);
        assert!(
            !claim_pick(&pool, key(7, 1_000, 1)).await?,
            "a second click"
        );
        // The same message showing its next prompt, with or without a new
        // edit time.
        assert!(claim_pick(&pool, key(7, 2_000, 2)).await?);
        assert!(claim_pick(&pool, key(7, 1_000, 3)).await?);
        assert!(
            claim_pick(&pool, key(8, 1_000, 1)).await?,
            "another message"
        );
        assert!(claim_pick(&pool, key(u64::from(u32::MAX) << 31, 1, i64::MIN)).await?);
        assert!(claim_pick(&pool, key(u64::MAX, 1, 1)).await.is_err());
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_released_claim_can_be_claimed_again(pool: PgPool) -> anyhow::Result<()> {
        assert!(claim_pick(&pool, key(7, 1, 1)).await?);
        release_pick(&pool, key(7, 1, 2)).await?;
        assert!(
            !claim_pick(&pool, key(7, 1, 1)).await?,
            "another key was released"
        );
        release_pick(&pool, key(7, 1, 1)).await?;
        assert!(claim_pick(&pool, key(7, 1, 1)).await?);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_clicks_claim_once(pool: PgPool) -> anyhow::Result<()> {
        let mut clicks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let pool = pool.clone();
            clicks.spawn(async move { claim_pick(&pool, key(9, 5, 5)).await });
        }
        let mut won = Vec::new();
        while let Some(click) = clicks.join_next().await {
            won.push(click??);
        }
        assert_eq!(won.iter().filter(|w| **w).count(), 1, "{won:?}");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_claim_prunes_claims_older_than_a_day(pool: PgPool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO pick_claims (message_id, shown_ms, digest, claimed_at) VALUES
             (1, 1, 1, now() - interval '25 hours'), (2, 1, 1, now() - interval '23 hours')",
        )
        .execute(&pool)
        .await?;
        assert!(claim_pick(&pool, key(3, 1, 1)).await?);
        let left: Vec<i64> =
            sqlx::query_scalar("SELECT message_id FROM pick_claims ORDER BY message_id")
                .fetch_all(&pool)
                .await?;
        assert_eq!(left, [2, 3]);
        Ok(())
    }
}
