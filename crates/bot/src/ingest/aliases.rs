//! Hand-curated nickname loader: YAML (`alias: Card Name`) -> `card_aliases`
//! (lowercased). Names are resolved case-insensitively against `cards.name`,
//! then `card_faces.name`; unresolved names are reported and skipped. The
//! table is replaced wholesale, and where the list came from is recorded in
//! the same transaction ([`super::lists`]).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context as _, Result};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::{
    RefreshLease,
    lists::{self, List, ListText, Loaded, Resolved, Source, Warnings},
    scryfall::BATCH,
};

/// `data/aliases.yaml` as of this build. The list is a few kilobytes of repo
/// data, so the binary carries it: a first load needs no checkout, and the
/// image needs no `data/` directory.
pub const BUILTIN: &str = include_str!("../../../../data/aliases.yaml");

/// Earlier releases' built-in copies ([`super::lists::List::legacy`]). A
/// change to the list's rows appends the text it replaces.
pub const LEGACY: &[&str] = &[include_str!("../../../../data/legacy/aliases-v1.2.0.yaml")];

/// A `card_aliases` row: the lowercased alias and its card.
pub(super) type Row = (String, Uuid);

/// Parse a flat YAML mapping (`alias: Card Name`). Returns `(alias_lowercased,
/// canonical_name)` pairs sorted by alias; entries with an empty alias or name
/// are reported and dropped.
///
/// # Errors
/// If the text is not a YAML mapping of string to string.
pub(crate) fn parse_alias_yaml(text: &str) -> Result<Vec<(String, String)>> {
    let map: BTreeMap<String, String> = serde_yaml_ng::from_str(text)
        .context("aliases: expected a flat `alias: Card Name` mapping")?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| {
            let (alias, name) = (k.trim().to_lowercase(), v.trim().to_owned());
            if alias.is_empty() || name.is_empty() {
                tracing::warn!(alias = k, name = v, "aliases: ignoring empty alias or name");
                return None;
            }
            Some((alias, name))
        })
        .collect())
}

/// The cards each lowercased name means, from `rows` of
/// `(lower(name), oracle_id)`.
fn by_name(rows: Vec<(String, Uuid)>) -> BTreeMap<String, BTreeSet<Uuid>> {
    let mut by_name: BTreeMap<String, BTreeSet<Uuid>> = BTreeMap::new();
    for (name, id) in rows {
        by_name.entry(name).or_default().insert(id);
    }
    by_name
}

/// The rows YAML `text` loads against today's cards, without writing them.
///
/// A name is looked up among card names first and face names only when no
/// card has it, so a card's own name wins over the same name on another
/// card's face (Lightning Bolt, and the Lightning Bolt face of an Emeritus).
/// A name that means several cards at the level it is found (Un-cards share
/// names across variants) is ambiguous and left unresolved, as the notes
/// loader and the card resolver do, rather than given to whichever row the
/// database returned first.
///
/// # Errors
/// On parse or database failure.
pub(super) async fn resolve(
    pool: &PgPool,
    text: &str,
    warnings: Warnings,
) -> Result<Resolved<Row>> {
    let pairs = parse_alias_yaml(text)?;
    let wanted: Vec<String> = pairs
        .iter()
        .map(|(_, n)| n.to_lowercase())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let cards = by_name(
        sqlx::query_as("SELECT lower(name), oracle_id FROM cards WHERE lower(name) = ANY($1)")
            .bind(&wanted)
            .fetch_all(pool)
            .await
            .context("resolving alias names against cards")?,
    );
    let faces = by_name(
        sqlx::query_as(
            "SELECT DISTINCT lower(name), oracle_id FROM card_faces WHERE lower(name) = ANY($1)",
        )
        .bind(&wanted)
        .fetch_all(pool)
        .await
        .context("resolving alias names against card_faces")?,
    );

    let mut resolved = Resolved::default();
    let mut seen_alias: HashSet<String> = HashSet::new();
    for (alias, name) in pairs {
        if !seen_alias.insert(alias.clone()) {
            if warnings == Warnings::Log {
                tracing::warn!(alias, "aliases: duplicate alias; first wins");
            }
            continue;
        }
        let key = name.to_lowercase();
        let ids = cards.get(&key).or_else(|| faces.get(&key));
        match ids.map(|ids| (ids.len(), ids.first())) {
            Some((1, Some(id))) => resolved.rows.push((alias, *id)),
            Some((n, _)) if n > 1 => {
                if warnings == Warnings::Log {
                    tracing::warn!(
                        alias,
                        name,
                        candidates = n,
                        "aliases: card name is ambiguous; use the full name"
                    );
                }
                resolved.unresolved.push(name);
            }
            _ => resolved.unresolved.push(name),
        }
    }
    Ok(resolved)
}

/// What `card_aliases` holds now.
///
/// # Errors
/// On a database failure.
pub(super) async fn loaded(pool: &PgPool) -> Result<BTreeSet<Row>> {
    let rows: Vec<Row> = sqlx::query_as("SELECT alias, oracle_id FROM card_aliases")
        .fetch_all(pool)
        .await
        .context("reading card_aliases")?;
    Ok(rows.into_iter().collect())
}

/// Replace `card_aliases` with `rows`, inside the caller's transaction.
async fn replace(tx: &mut PgConnection, rows: &[Row]) -> Result<()> {
    sqlx::query!("DELETE FROM card_aliases")
        .execute(&mut *tx)
        .await
        .context("clearing card_aliases")?;
    for chunk in rows.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO card_aliases (alias, oracle_id) ");
        qb.push_values(chunk, |mut b, (alias, id)| {
            b.push_bind(alias).push_bind(id);
        });
        qb.build()
            .execute(&mut *tx)
            .await
            .context("inserting card_aliases")?;
    }
    Ok(())
}

/// Load aliases from `text` (the built-in copy or an operator's file),
/// replacing the table contents and recording the source in the same
/// transaction. Unresolved card names are reported as warnings and skipped.
///
/// # Errors
/// On parse or database failure.
pub async fn run(lease: &mut RefreshLease, text: &ListText) -> Result<Loaded> {
    let pool = lease.pool();
    if text.source() == Source::Builtin {
        lists::ensure_cards(pool, List::Aliases).await?;
    }
    let resolved = resolve(pool, text.yaml(List::Aliases), Warnings::Log).await?;
    let mut tx = pool.begin().await?;
    replace(&mut tx, &resolved.rows).await?;
    lists::record(&mut tx, List::Aliases, text).await?;
    tx.commit().await?;
    let loaded = resolved.loaded();
    tracing::info!(
        rows = loaded.rows,
        unresolved = loaded.unresolved,
        source = text.source().as_str(),
        "card_aliases replaced"
    );
    if !resolved.unresolved.is_empty() {
        tracing::warn!(names = ?resolved.unresolved, "aliases: unresolved card names (run `judgebot ingest cards` first?)");
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{Warnings, parse_alias_yaml, resolve};

    #[test]
    fn alias_yaml_parses_quotes_and_comments() -> anyhow::Result<()> {
        let text = "# nicknames\n---\nBob: Dark Confidant\n\"Jace TMS\": 'Jace, the Mind Sculptor'  # comment\nurborg: Urborg, Tomb of Yawgmoth # note\n\"\": Nothing\n";
        let pairs = parse_alias_yaml(text)?;
        assert_eq!(
            pairs,
            vec![
                ("bob".to_owned(), "Dark Confidant".to_owned()),
                ("jace tms".to_owned(), "Jace, the Mind Sculptor".to_owned()),
                ("urborg".to_owned(), "Urborg, Tomb of Yawgmoth".to_owned()),
            ]
        );
        assert!(parse_alias_yaml("- not\n- a mapping\n").is_err());
        Ok(())
    }

    /// A card's own name beats a face of another card; a name several
    /// cards share is unresolved, never given to one of them; a face name
    /// alone resolves to its card.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_shared_name_is_ambiguous_and_a_card_name_beats_a_face(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let card = async |name: &str, faces: &[&str]| -> anyhow::Result<Uuid> {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO cards (oracle_id, name, layout) VALUES ($1, $2, 'normal')")
                .bind(id)
                .bind(name)
                .execute(&pool)
                .await?;
            for (idx, face) in (0_i16..).zip(faces) {
                sqlx::query(
                    "INSERT INTO card_faces (oracle_id, face_idx, name) VALUES ($1, $2, $3)",
                )
                .bind(id)
                .bind(idx)
                .bind(face)
                .execute(&pool)
                .await?;
            }
            Ok(id)
        };
        let bolt = card("Lightning Bolt", &["Lightning Bolt"]).await?;
        let emeritus = card(
            "Emeritus of Conflict // Lightning Bolt",
            &["Emeritus of Conflict", "Lightning Bolt"],
        )
        .await?;
        card("Sly Spy", &["Sly Spy"]).await?;
        card("Sly Spy", &["Sly Spy"]).await?;
        let r = resolve(
            &pool,
            "bolt: Lightning Bolt\nspy: Sly Spy\nemeritus: Emeritus of Conflict\n",
            Warnings::Quiet,
        )
        .await?;
        let mut rows = r.rows;
        rows.sort();
        let mut want = vec![("bolt".to_owned(), bolt), ("emeritus".to_owned(), emeritus)];
        want.sort();
        assert_eq!(rows, want);
        assert_eq!(r.unresolved, vec!["Sly Spy".to_owned()]);
        Ok(())
    }
}
