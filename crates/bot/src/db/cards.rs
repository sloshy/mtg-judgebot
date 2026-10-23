//! Loading full [`Card`]s (all faces) by oracle id, with whether each is out
//! yet and whether the loaded CR covers it (D42, `judge_core::preview`).

use std::collections::HashMap;

use judge_core::preview::{COVER_WINDOW_DAYS, Term, rules_terms};
use judge_core::{Card, CardId, Face, JudgeError, Layout, Release};
use nonempty::NonEmpty;
use sqlx::PgPool;
use uuid::Uuid;

use super::{bad_row, upstream};

struct FaceRow {
    oracle_id: Uuid,
    name: String,
    layout: String,
    keywords: Vec<String>,
    /// The first printing's date when it is after today (the database's).
    upcoming_on: Option<String>,
    /// The loaded CR is dated more than `COVER_WINDOW_DAYS` before the first printing.
    cr_predates: bool,
    face_name: String,
    oracle_text: String,
    mana_cost: String,
    type_line: String,
}

/// A loaded card before its release is assessed.
struct Loaded {
    card: Card,
    keywords: Vec<String>,
    upcoming_on: Option<String>,
    cr_predates: bool,
}

/// Load the cards with these oracle ids, each with all its faces and its
/// [`Release`], in the order of `ids` (duplicates collapse to the first
/// occurrence). Ids without a `cards` row are skipped; a card without any
/// `card_faces` row is a data error.
///
/// "Today" is the database's `current_date`. Only an upcoming card costs a
/// second query: its rules terms are looked up in the loaded CR's rules and
/// glossary.
pub(super) async fn load_cards(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<Card>, JudgeError> {
    let loaded = load(pool, ids).await?;
    let mut out = Vec::with_capacity(loaded.len());
    for l in loaded {
        let mut card = l.card;
        card.release = match l.upcoming_on {
            None => Release::Released,
            Some(on) => {
                let faces: Vec<Face> = card.faces.clone().into();
                let terms = rules_terms(&l.keywords, &faces);
                let unknown = unknown_terms(pool, &terms).await?;
                Release::assess(Some(on), l.cr_predates, unknown)
            }
        };
        out.push(card);
    }
    Ok(out)
}

/// [`load_cards`] without the release assessment: every card is
/// [`Release::Released`]. For the retirement pass, which reads Oracle text
/// inside its own transaction and never shows a card.
pub(super) async fn load_card_text(
    executor: impl sqlx::PgExecutor<'_>,
    ids: &[Uuid],
) -> Result<Vec<Card>, JudgeError> {
    Ok(load(executor, ids)
        .await?
        .into_iter()
        .map(|l| l.card)
        .collect())
}

async fn load(
    executor: impl sqlx::PgExecutor<'_>,
    ids: &[Uuid],
) -> Result<Vec<Loaded>, JudgeError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        FaceRow,
        r#"
        SELECT c.oracle_id, c.name, c.layout, c.keywords,
               CASE WHEN c.first_printed > current_date
                    THEN to_char(c.first_printed, 'YYYY-MM-DD') END AS upcoming_on,
               -- An uncorrelated subquery is an init plan, run only when a
               -- row reaches it: never for a released card.
               CASE WHEN c.first_printed > current_date
                    THEN COALESCE((SELECT to_date(max(cr_version), 'YYYYMMDD') FROM rules)
                                  < c.first_printed - $2::int, true)
                    ELSE false END AS "cr_predates!",
               f.name AS face_name, f.oracle_text, f.mana_cost, f.type_line
        FROM cards c
        JOIN card_faces f ON f.oracle_id = c.oracle_id
        WHERE c.oracle_id = ANY($1)
        ORDER BY c.oracle_id, f.face_idx
        "#,
        ids,
        COVER_WINDOW_DAYS,
    )
    .fetch_all(executor)
    .await
    .map_err(upstream("load cards"))?;

    let mut by_id: HashMap<Uuid, (FaceRow, Vec<Face>)> = HashMap::new();
    for r in rows {
        let face = Face {
            name: r.face_name.clone(),
            oracle_text: r.oracle_text.clone(),
            mana_cost: r.mana_cost.clone(),
            type_line: r.type_line.clone(),
        };
        by_id
            .entry(r.oracle_id)
            .or_insert_with(|| (r, Vec::new()))
            .1
            .push(face);
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some((head, faces)) = by_id.remove(id) else {
            continue;
        };
        let faces = NonEmpty::from_vec(faces)
            .ok_or_else(|| bad_row(format!("card {id} ({}) has no faces", head.name)))?;
        out.push(Loaded {
            card: Card {
                id: CardId::new(*id),
                layout: parse_layout(&head.layout, *id),
                name: head.name,
                faces,
                release: Release::Released,
            },
            keywords: head.keywords,
            upcoming_on: head.upcoming_on,
            cr_predates: head.cr_predates,
        });
    }
    Ok(out)
}

/// The terms the loaded CR never mentions: no rule body, glossary term or
/// glossary text has the term, or its family's word, as a whole word (any case).
async fn unknown_terms(pool: &PgPool, terms: &[Term]) -> Result<Vec<String>, JudgeError> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = terms.iter().map(|t| t.name.clone()).collect();
    let patterns: Vec<String> = terms.iter().map(word_pattern).collect();
    let unknown = sqlx::query_scalar!(
        r#"
        SELECT t.name AS "name!"
        FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS t(name, pattern, n)
        WHERE NOT EXISTS (SELECT 1 FROM rules r WHERE r.body ~* t.pattern)
          AND NOT EXISTS (SELECT 1 FROM glossary g WHERE g.term ~* t.pattern OR g.text ~* t.pattern)
        ORDER BY t.n
        "#,
        &names,
        &patterns,
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("look up card terms"))?;
    Ok(unknown)
}

/// A Postgres regular expression matching the term, or its family's word, as
/// a whole word: a word boundary on each side that is a letter or digit (so
/// `For Mirrodin!` can match), any run of whitespace between words, and either
/// apostrophe, because the CR prints `Urza’s` where Scryfall has `Urza's`.
/// `regex::escape` backslashes only punctuation, which a Postgres ARE reads as
/// the literal character.
fn word_pattern(t: &Term) -> String {
    let words: Vec<String> = std::iter::once(t.name.as_str())
        .chain(t.family)
        .map(|w| {
            let body = regex::escape(w).replace(' ', "\\s+").replace('\'', "['’]");
            let open = if w.starts_with(char::is_alphanumeric) {
                "\\m"
            } else {
                ""
            };
            let close = if w.ends_with(char::is_alphanumeric) {
                "\\M"
            } else {
                ""
            };
            format!("{open}{body}{close}")
        })
        .collect();
    format!("(?:{})", words.join("|"))
}

/// Scryfall `layout` string → [`Layout`]; unknown values degrade to `Normal` with a warning.
fn parse_layout(raw: &str, id: Uuid) -> Layout {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).unwrap_or_else(|_| {
        tracing::warn!(%id, layout = raw, "unknown Scryfall layout; treating as normal");
        Layout::Normal
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(name: &str, family: Option<&'static str>) -> Term {
        Term {
            name: name.into(),
            family,
        }
    }

    #[test]
    fn a_pattern_bounds_only_word_edges_and_folds_apostrophes() {
        assert_eq!(word_pattern(&term("Vulcan", None)), "(?:\\mVulcan\\M)");
        assert_eq!(
            word_pattern(&term("For Mirrodin!", None)),
            "(?:\\mFor\\s+Mirrodin!)"
        );
        assert_eq!(word_pattern(&term("Urza's", None)), "(?:\\mUrza['’]s\\M)");
        assert_eq!(
            word_pattern(&term("El-Aurian", Some("typecycling"))),
            "(?:\\mEl\\-Aurian\\M|\\mtypecycling\\M)"
        );
    }
}
