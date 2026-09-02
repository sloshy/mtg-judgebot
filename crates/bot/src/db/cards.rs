//! Loading full [`Card`]s (all faces) by oracle id.

use std::collections::HashMap;

use judge_core::{Card, CardId, Face, JudgeError, Layout};
use nonempty::NonEmpty;
use uuid::Uuid;

use super::{bad_row, upstream};

struct FaceRow {
    oracle_id: Uuid,
    name: String,
    layout: String,
    face_name: String,
    oracle_text: String,
    mana_cost: String,
    type_line: String,
}

/// Load the cards with these oracle ids, each with all its faces, in the
/// order of `ids` (duplicates collapse to the first occurrence). Ids without
/// a `cards` row are skipped; a card without any `card_faces` row is a data
/// error.
pub(super) async fn load_cards(pool: impl sqlx::PgExecutor<'_>, ids: &[Uuid]) -> Result<Vec<Card>, JudgeError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        FaceRow,
        r#"
        SELECT c.oracle_id, c.name, c.layout,
               f.name AS face_name, f.oracle_text, f.mana_cost, f.type_line
        FROM cards c
        JOIN card_faces f ON f.oracle_id = c.oracle_id
        WHERE c.oracle_id = ANY($1)
        ORDER BY c.oracle_id, f.face_idx
        "#,
        ids
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("load cards"))?;

    let mut by_id: HashMap<Uuid, (String, String, Vec<Face>)> = HashMap::new();
    for r in rows {
        let face = Face {
            name: r.face_name,
            oracle_text: r.oracle_text,
            mana_cost: r.mana_cost,
            type_line: r.type_line,
        };
        by_id
            .entry(r.oracle_id)
            .or_insert_with(|| (r.name, r.layout, Vec::new()))
            .2
            .push(face);
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some((name, layout, faces)) = by_id.remove(id) else {
            continue;
        };
        let faces = NonEmpty::from_vec(faces)
            .ok_or_else(|| bad_row(format!("card {id} ({name}) has no faces")))?;
        out.push(Card {
            id: CardId::new(*id),
            name,
            layout: parse_layout(&layout, *id),
            faces,
        });
    }
    Ok(out)
}

/// Scryfall `layout` string → [`Layout`]; unknown values degrade to `Normal` with a warning.
fn parse_layout(raw: &str, id: Uuid) -> Layout {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).unwrap_or_else(|_| {
        tracing::warn!(%id, layout = raw, "unknown Scryfall layout; treating as normal");
        Layout::Normal
    })
}
