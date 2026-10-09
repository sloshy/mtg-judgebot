//! Hand-written "nightmare card" notes: YAML (`Card Name: markdown note`) -> `card_notes`.
//!
//! Names are resolved case-insensitively through `printed_names` (so old and face
//! names work); unresolved names are reported as warnings and skipped. The table is
//! replaced wholesale, mirroring the alias loader, and where the list came from
//! is recorded in the same transaction ([`super::lists`]).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::{
    RefreshLease,
    lists::{self, List, ListText, Loaded, Resolved, Source, Warnings},
};

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

/// `data/notes.yaml` as of this build (see [`super::aliases::BUILTIN`]).
pub const BUILTIN: &str = include_str!("../../../../data/notes.yaml");

/// Earlier releases' built-in copies (see [`super::aliases::LEGACY`]).
pub const LEGACY: &[&str] = &[include_str!("../../../../data/legacy/notes-v1.2.0.yaml")];

/// A `card_notes` row: the card and its note.
pub(super) type Row = (Uuid, String);

/// The rows YAML `text` loads against today's cards, without writing them.
///
/// # Errors
/// On parse or database failure.
pub(super) async fn resolve(
    pool: &PgPool,
    text: &str,
    warnings: Warnings,
) -> Result<Resolved<Row>> {
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

    let mut resolved = Resolved::default();
    for (name, note) in pairs {
        let key = name.to_lowercase();
        let n = counts.iter().find(|c| c.name == key).map_or(0, |c| c.n);
        match by_name.get(&key) {
            Some(id) if n == 1 => resolved.rows.push((*id, note)),
            Some(_) => {
                if warnings == Warnings::Log {
                    tracing::warn!(
                        name,
                        candidates = n,
                        "notes: card name is ambiguous; use the full name"
                    );
                }
                resolved.unresolved.push(name);
            }
            None => resolved.unresolved.push(name),
        }
    }
    Ok(resolved)
}

/// What `card_notes` holds now.
///
/// # Errors
/// On a database failure.
pub(super) async fn loaded(pool: &PgPool) -> Result<BTreeSet<Row>> {
    let rows: Vec<Row> = sqlx::query_as("SELECT oracle_id, note FROM card_notes")
        .fetch_all(pool)
        .await
        .context("reading card_notes")?;
    Ok(rows.into_iter().collect())
}

/// Replace `card_notes` with `rows`, inside the caller's transaction.
async fn replace(tx: &mut PgConnection, rows: &[Row]) -> Result<()> {
    sqlx::query!("DELETE FROM card_notes")
        .execute(&mut *tx)
        .await
        .context("clearing card_notes")?;
    if !rows.is_empty() {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO card_notes (oracle_id, note) ");
        qb.push_values(rows, |mut b, (id, note)| {
            b.push_bind(id).push_bind(note);
        });
        qb.build()
            .execute(&mut *tx)
            .await
            .context("inserting card_notes")?;
    }
    Ok(())
}

/// Load notes from `text` (the built-in copy or an operator's file),
/// replacing the `card_notes` table contents and recording the source in the
/// same transaction.
///
/// # Errors
/// On parse or database failure.
pub async fn run(lease: &mut RefreshLease, text: &ListText) -> Result<Loaded> {
    let pool = lease.pool();
    if text.source() == Source::Builtin {
        lists::ensure_cards(pool, List::Notes).await?;
    }
    let resolved = resolve(pool, text.yaml(List::Notes), Warnings::Log).await?;
    let mut tx = pool.begin().await?;
    replace(&mut tx, &resolved.rows).await?;
    lists::record(&mut tx, List::Notes, text).await?;
    tx.commit().await?;
    let loaded = resolved.loaded();
    tracing::info!(
        rows = loaded.rows,
        unresolved = loaded.unresolved,
        source = text.source().as_str(),
        "card_notes replaced"
    );
    if !resolved.unresolved.is_empty() {
        tracing::warn!(names = ?resolved.unresolved, "notes: unresolved card names (run `judgebot ingest cards` first?)");
    }
    Ok(loaded)
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
