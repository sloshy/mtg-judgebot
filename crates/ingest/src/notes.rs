//! Hand-written "nightmare card" notes: YAML (`Card Name: markdown note`) -> `card_notes`.
//!
//! Names are resolved case-insensitively through `printed_names` (so old and face
//! names work); unresolved names are reported as warnings and skipped. The table is
//! replaced wholesale, mirroring the alias loader.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Parse the notes file: a flat mapping of card name to note text.
///
/// # Errors
/// If the text is not a YAML mapping of string to string.
pub(crate) fn parse_notes_yaml(text: &str) -> Result<Vec<(String, String)>> {
    let map: BTreeMap<String, String> = serde_yaml_ng::from_str(text)
        .context("notes: expected a flat `Card Name: note` mapping")?;
    Ok(map
        .into_iter()
        .filter_map(|(name, note)| {
            let (name, note) = (name.trim().to_owned(), note.trim().to_owned());
            if name.is_empty() || note.is_empty() {
                tracing::warn!(name, "notes: ignoring empty card name or note");
                return None;
            }
            Some((name, note))
        })
        .collect())
}

/// `data/notes.yaml` as of this build (see [`crate::aliases::BUILTIN`]).
pub const BUILTIN: &str = include_str!("../../../data/notes.yaml");

/// Load notes from YAML `text`, replacing the `card_notes` table contents.
///
/// # Errors
/// On parse or database failure.
pub async fn run(pool: &PgPool, text: &str) -> Result<()> {
    let pairs = parse_notes_yaml(text)?;
    let wanted: Vec<String> = pairs.iter().map(|(n, _)| n.to_lowercase()).collect();
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT ON (lower(printed_name)) lower(printed_name) AS "name!", oracle_id
        FROM printed_names
        WHERE lower(printed_name) = ANY($1)
        ORDER BY lower(printed_name), oracle_id
        "#,
        &wanted
    )
    .fetch_all(pool)
    .await
    .context("resolving note card names")?;
    // A name shared by several cards (a face name) is ambiguous: refuse rather than guess.
    let counts = sqlx::query!(
        "SELECT lower(printed_name) AS \"name!\", count(DISTINCT oracle_id) AS \"n!\" FROM printed_names WHERE lower(printed_name) = ANY($1) GROUP BY 1",
        &wanted
    )
    .fetch_all(pool)
    .await
    .context("counting note card names")?;
    let by_name: BTreeMap<String, Uuid> = rows.into_iter().map(|r| (r.name, r.oracle_id)).collect();

    let mut resolved: Vec<(Uuid, String)> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for (name, note) in pairs {
        let key = name.to_lowercase();
        let n = counts.iter().find(|c| c.name == key).map_or(0, |c| c.n);
        match by_name.get(&key) {
            Some(id) if n == 1 => resolved.push((*id, note)),
            Some(_) => {
                tracing::warn!(
                    name,
                    candidates = n,
                    "notes: card name is ambiguous; use the full name"
                );
                unresolved.push(name);
            }
            None => {
                tracing::warn!(name, "notes: card name not found in printed_names");
                unresolved.push(name);
            }
        }
    }

    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM card_notes")
        .execute(&mut *tx)
        .await
        .context("clearing card_notes")?;
    if !resolved.is_empty() {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO card_notes (oracle_id, note) ");
        qb.push_values(&resolved, |mut b, (id, note)| {
            b.push_bind(id).push_bind(note);
        });
        qb.build()
            .execute(&mut *tx)
            .await
            .context("inserting card_notes")?;
    }
    tx.commit().await?;
    tracing::info!(
        loaded = resolved.len(),
        unresolved = unresolved.len(),
        "card_notes replaced"
    );
    if !unresolved.is_empty() {
        tracing::warn!(names = ?unresolved, "notes: unresolved card names (run `ingest cards` first?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_notes_yaml;

    #[test]
    fn parses_mapping_and_drops_empties() -> anyhow::Result<()> {
        let pairs = parse_notes_yaml("Blood Moon: |\n  Layer 4.\n\"\": x\nHumility: ''\n")?;
        assert_eq!(
            pairs,
            vec![("Blood Moon".to_owned(), "Layer 4.".to_owned())]
        );
        assert!(parse_notes_yaml("- list").is_err());
        Ok(())
    }
}
