//! Scryfall bulk sync: `oracle_cards` -> `cards` + `card_faces`, `default_cards` ->
//! `printed_names`, `rulings` (bulk, keyed by `oracle_id`) -> `rulings`.
//! Downloads are cached under `cache_dir`.
//!
//! Also hosts the hand-curated alias loader (`aliases.rs` delegates here) since
//! both resolve against the same `cards` table.
//!
//! # Skip rule (which Scryfall objects become `cards` rows)
//!
//! A bulk card object is **skipped** when any of the following holds:
//!
//! 1. its `layout` is one of `planar`, `scheme`, `vanguard`, `token`,
//!    `double_faced_token`, `emblem`, `art_series`, `augment`, `host` (non-game
//!    objects, and Un-set augment/host cards whose text is not self-contained);
//! 2. its `set_type` is one of `token`, `memorabilia`, `minigame`, `alchemy`
//!    (Alchemy sets only contain rebalanced `A-` digital cards);
//! 3. its `games` list is present and does not contain `"paper"` **and** no
//!    printing in `default_cards` is on paper either (digital-only cards:
//!    Arena/MTGO exclusives and rebalanced `A-` cards). The representative
//!    printing in `oracle_cards` is often an MTGO Masters Edition one for cards
//!    that exist on paper (Black Lotus, Tundra, ...), so paper availability is
//!    decided per oracle id, not per printing. A missing `games` list is
//!    treated as paper;
//! 4. it has no `oracle_id` (neither top-level nor on any face).
//!
//! Everything else is kept, including `funny` sets: acorn cards are real cards a
//! user may ask about, and their presence costs nothing. The same rule filters
//! `default_cards` for `printed_names`, and rulings are only stored for
//! `oracle_id`s that exist in `cards` (foreign key).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use judge_core::{RulingKey, ruling_key};
use serde::Deserialize;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Scryfall bulk-data index endpoint.
const BULK_INDEX_URL: &str = "https://api.scryfall.com/bulk-data";
/// Scryfall asks every client to identify itself.
const USER_AGENT: &str = "mtg-judgebot-ingest/0.1 (https://github.com/sloshy/mtg-judgebot)";
/// Rows per multi-row INSERT (500 rows × ≤9 columns stays far below the 65535 bind limit).
const BATCH: usize = 500;

/// Layouts that never become `cards` rows (see module docs).
const SKIPPED_LAYOUTS: &[&str] = &[
    "planar",
    "scheme",
    "vanguard",
    "token",
    "double_faced_token",
    "emblem",
    "art_series",
    "augment",
    "host",
];
/// Set types that never become `cards` rows (see module docs).
const SKIPPED_SET_TYPES: &[&str] = &["token", "memorabilia", "minigame", "alchemy"];

// ---------------------------------------------------------------------------
// Scryfall JSON shapes (only the fields we read)
// ---------------------------------------------------------------------------

/// One entry of `GET /bulk-data`.
#[derive(Debug, Deserialize)]
struct BulkEntry {
    #[serde(rename = "type")]
    kind: String,
    updated_at: String,
    #[serde(default)]
    jsonl_download_uri: Option<String>,
    #[serde(default)]
    download_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BulkIndex {
    data: Vec<BulkEntry>,
}

/// A face of a multi-faced card object.
#[derive(Debug, Default, Deserialize)]
struct ScryfallFace {
    #[serde(default)]
    name: String,
    #[serde(default)]
    oracle_text: String,
    #[serde(default)]
    mana_cost: String,
    #[serde(default)]
    type_line: String,
    #[serde(default)]
    oracle_id: Option<Uuid>,
    #[serde(default)]
    flavor_name: Option<String>,
    #[serde(default)]
    printed_name: Option<String>,
}

/// A card object from `oracle_cards` / `default_cards`.
#[derive(Debug, Default, Deserialize)]
struct ScryfallCard {
    #[serde(default)]
    id: Option<Uuid>,
    #[serde(default)]
    oracle_id: Option<Uuid>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    layout: String,
    #[serde(default)]
    type_line: Option<String>,
    #[serde(default)]
    cmc: f32,
    #[serde(default)]
    color_identity: Vec<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    legalities: serde_json::Value,
    #[serde(default)]
    oracle_text: String,
    #[serde(default)]
    mana_cost: String,
    #[serde(default)]
    card_faces: Vec<ScryfallFace>,
    #[serde(default)]
    flavor_name: Option<String>,
    #[serde(default)]
    printed_name: Option<String>,
    #[serde(default)]
    set_type: String,
    #[serde(default)]
    games: Option<Vec<String>>,
}

/// One entry of the `rulings` bulk file.
#[derive(Debug, Deserialize)]
struct ScryfallRuling {
    oracle_id: Uuid,
    published_at: String,
    comment: String,
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// A `cards` row.
#[derive(Debug, Clone, PartialEq)]
struct CardRow {
    oracle_id: Uuid,
    name: String,
    layout: String,
    type_line: String,
    cmc: f32,
    color_identity: Vec<String>,
    keywords: Vec<String>,
    scryfall_id: Option<Uuid>,
    legalities: serde_json::Value,
}

/// A `card_faces` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FaceRow {
    oracle_id: Uuid,
    face_idx: i16,
    name: String,
    oracle_text: String,
    mana_cost: String,
    type_line: String,
}

/// A `rulings` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RulingRow {
    oracle_id: Uuid,
    /// `judge_core::ruling_key` of (`published_at`, `text`).
    key: RulingKey,
    /// `YYYY-MM-DD`, cast to `date` in SQL.
    published_at: String,
    text: String,
}

impl ScryfallCard {
    /// The card's oracle id: top-level, or (reversible cards) the first face's.
    fn effective_oracle_id(&self) -> Option<Uuid> {
        self.oracle_id.or_else(|| self.card_faces.iter().find_map(|f| f.oracle_id))
    }

    /// Skip-rule parts 1, 2 and 4: non-game layouts, non-game set types, no oracle id.
    fn is_skipped_shape(&self) -> bool {
        SKIPPED_LAYOUTS.contains(&self.layout.as_str())
            || SKIPPED_SET_TYPES.contains(&self.set_type.as_str())
            || self.effective_oracle_id().is_none()
    }

    /// Skip-rule part 3: this *printing* is not available on paper.
    fn is_digital_only(&self) -> bool {
        self.games.as_ref().is_some_and(|g| !g.iter().any(|x| x == "paper"))
    }

    /// Module-level skip rule for a card whose oracle id is known to have a
    /// paper printing (`has_paper`); the representative printing in
    /// `oracle_cards` is often an MTGO Masters Edition one (`games:["mtgo"]`)
    /// for cards that very much exist on paper (Black Lotus, Tundra, ...).
    fn is_skipped(&self, has_paper: bool) -> bool {
        self.is_skipped_shape() || (self.is_digital_only() && !has_paper)
    }

    /// Map to `cards` + `card_faces` rows, or `None` when skipped.
    fn to_rows(&self, has_paper: bool) -> Option<(CardRow, Vec<FaceRow>)> {
        if self.is_skipped(has_paper) {
            return None;
        }
        let oracle_id = self.effective_oracle_id()?;
        let faces: Vec<FaceRow> = if self.card_faces.is_empty() {
            vec![FaceRow {
                oracle_id,
                face_idx: 0,
                name: self.name.clone(),
                oracle_text: self.oracle_text.clone(),
                mana_cost: self.mana_cost.clone(),
                type_line: self.type_line.clone().unwrap_or_default(),
            }]
        } else {
            self.card_faces
                .iter()
                .enumerate()
                .map(|(i, f)| FaceRow {
                    oracle_id,
                    face_idx: i16::try_from(i).unwrap_or(i16::MAX),
                    name: f.name.clone(),
                    oracle_text: f.oracle_text.clone(),
                    mana_cost: f.mana_cost.clone(),
                    type_line: f.type_line.clone(),
                })
                .collect()
        };
        // Reversible cards carry no top-level type_line; join the faces'.
        let type_line = self.type_line.clone().unwrap_or_else(|| {
            faces.iter().map(|f| f.type_line.as_str()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" // ")
        });
        let card = CardRow {
            oracle_id,
            name: self.name.clone(),
            layout: self.layout.clone(),
            type_line,
            cmc: self.cmc,
            color_identity: self.color_identity.clone(),
            keywords: self.keywords.clone(),
            scryfall_id: self.id,
            legalities: if self.legalities.is_object() { self.legalities.clone() } else { serde_json::json!({}) },
        };
        Some((card, faces))
    }

    /// Every printed/flavor/face name of this printing, lowercased and deduplicated.
    fn printed_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| {
            let s = s.trim().to_lowercase();
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        };
        push(&self.name);
        if let Some(p) = &self.printed_name {
            push(p);
        }
        if let Some(f) = &self.flavor_name {
            push(f);
        }
        for face in &self.card_faces {
            push(&face.name);
            if let Some(p) = &face.printed_name {
                push(p);
            }
            if let Some(f) = &face.flavor_name {
                push(f);
            }
        }
        out
    }
}

/// Group raw rulings by oracle id, one row per distinct (`published_at`, text), keyed by content.
fn key_rulings(raw: HashMap<Uuid, Vec<(String, String)>>) -> Vec<RulingRow> {
    let mut rows = Vec::new();
    for (oracle_id, mut list) in raw {
        list.sort();
        // Two rulings with the same date and text under one card are one row;
        // the key is a function of exactly those two fields, so this dedup is
        // what makes (oracle_id, key) a primary key.
        list.dedup();
        rows.extend(list.into_iter().map(|(published_at, text)| RulingRow {
            oracle_id,
            key: ruling_key(&published_at, &text),
            published_at,
            text,
        }));
    }
    rows
}

// ---------------------------------------------------------------------------
// Download & cache
// ---------------------------------------------------------------------------

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder().user_agent(USER_AGENT).build().context("building HTTP client")
}

/// Fetch the bulk index and return the entries we need, keyed by type.
async fn bulk_index(client: &reqwest::Client) -> Result<HashMap<String, BulkEntry>> {
    let index: BulkIndex = client
        .get(BULK_INDEX_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("GET /bulk-data")?
        .error_for_status()
        .context("GET /bulk-data status")?
        .json()
        .await
        .context("parsing /bulk-data")?;
    Ok(index.data.into_iter().map(|e| (e.kind.clone(), e)).collect())
}

/// Return the cached JSONL path for `kind`, downloading only if `updated_at` changed.
async fn fetch_bulk(client: &reqwest::Client, cache_dir: &Path, entry: &BulkEntry) -> Result<PathBuf> {
    std::fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    let path = cache_dir.join(format!("{}.jsonl", entry.kind));
    let stamp = cache_dir.join(format!("{}.updated_at", entry.kind));
    if path.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == entry.updated_at) {
        tracing::info!(kind = %entry.kind, path = %path.display(), "bulk file unchanged; using cache");
        return Ok(path);
    }
    let uri = entry
        .jsonl_download_uri
        .as_deref()
        .or(entry.download_uri.as_deref())
        .ok_or_else(|| anyhow::anyhow!("bulk entry {} has no download uri", entry.kind))?;
    tracing::info!(kind = %entry.kind, %uri, "downloading bulk file");
    let mut resp = client
        .get(uri)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .with_context(|| format!("GET {uri}"))?
        .error_for_status()
        .with_context(|| format!("GET {uri} status"))?;
    let tmp = cache_dir.join(format!("{}.jsonl.part", entry.kind));
    let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let mut bytes: u64 = 0;
    while let Some(chunk) = resp.chunk().await.with_context(|| format!("reading {uri}"))? {
        file.write_all(&chunk)?;
        bytes += chunk.len() as u64;
    }
    file.flush()?;
    drop(file);
    std::fs::rename(&tmp, &path)?;
    std::fs::write(&stamp, &entry.updated_at)?;
    tracing::info!(kind = %entry.kind, bytes, "downloaded");
    Ok(path)
}

/// Iterate over the non-empty lines of a JSONL file, parsing each as `T`.
/// Lines that fail to parse are logged and skipped (Scryfall occasionally adds
/// object types we do not model); the total skipped is logged once at EOF. A
/// *read* error (bad UTF-8, truncated or corrupt stream) is not recoverable
/// line by line: it is yielded as the final `Err` item, so callers abort the
/// sync instead of treating a partial file as complete.
///
/// Scryfall's `jsonl_download_uri` files are gzip-compressed (`.jsonl.gz`);
/// the cached file is decompressed transparently when it starts with the gzip
/// magic bytes, so both plain and gzipped caches work.
fn jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<impl Iterator<Item = Result<T>>> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 2];
    let gzipped = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
    file.seek(std::io::SeekFrom::Start(0)).with_context(|| format!("rewinding {}", path.display()))?;
    let raw: Box<dyn std::io::Read> = if gzipped {
        tracing::info!(path = %path.display(), "gzip-compressed bulk file; decompressing on the fly");
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let shown = path.display().to_string();
    let mut lines = std::io::BufReader::with_capacity(1 << 20, raw).lines().enumerate();
    let mut bad: u32 = 0;
    let mut done = false;
    Ok(std::iter::from_fn(move || {
        while !done {
            let Some((n, line)) = lines.next() else {
                done = true;
                if bad > 0 {
                    tracing::warn!(path = %shown, skipped = bad, "unparseable lines skipped");
                }
                return None;
            };
            let line = match line {
                Ok(l) => l,
                Err(err) => {
                    done = true; // stop pulling from the broken reader
                    return Some(Err(anyhow::Error::new(err).context(format!("reading {shown} line {}", n + 1))));
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<T>(trimmed) {
                Ok(v) => return Some(Ok(v)),
                Err(err) => {
                    bad += 1;
                    if bad <= 5 {
                        tracing::warn!(path = %shown, line = n + 1, %err, "skipping unparseable line");
                    }
                }
            }
        }
        None
    }))
}

// ---------------------------------------------------------------------------
// Database writes
// ---------------------------------------------------------------------------

async fn insert_cards(pool: &PgPool, cards: &[CardRow], faces: &[FaceRow]) -> Result<()> {
    if cards.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO cards (oracle_id, name, layout, type_line, cmc, color_identity, keywords, scryfall_id, legalities) ",
    );
    qb.push_values(cards, |mut b, c| {
        b.push_bind(c.oracle_id)
            .push_bind(&c.name)
            .push_bind(&c.layout)
            .push_bind(&c.type_line)
            .push_bind(c.cmc)
            .push_bind(&c.color_identity)
            .push_bind(&c.keywords)
            .push_bind(c.scryfall_id)
            .push_bind(&c.legalities);
    });
    qb.push(
        " ON CONFLICT (oracle_id) DO UPDATE SET name = EXCLUDED.name, layout = EXCLUDED.layout, \
         type_line = EXCLUDED.type_line, cmc = EXCLUDED.cmc, color_identity = EXCLUDED.color_identity, \
         keywords = EXCLUDED.keywords, scryfall_id = EXCLUDED.scryfall_id, legalities = EXCLUDED.legalities, \
         updated_at = now()",
    );
    qb.build().execute(&mut *tx).await.context("upserting cards")?;

    // Faces: a card may lose faces between syncs (layout errata), so clear first.
    let ids: Vec<Uuid> = cards.iter().map(|c| c.oracle_id).collect();
    sqlx::query!("DELETE FROM card_faces WHERE oracle_id = ANY($1)", &ids)
        .execute(&mut *tx)
        .await
        .context("clearing card_faces")?;
    for chunk in faces.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("INSERT INTO card_faces (oracle_id, face_idx, name, oracle_text, mana_cost, type_line) ");
        qb.push_values(chunk, |mut b, f| {
            b.push_bind(f.oracle_id)
                .push_bind(f.face_idx)
                .push_bind(&f.name)
                .push_bind(&f.oracle_text)
                .push_bind(&f.mana_cost)
                .push_bind(&f.type_line);
        });
        qb.push(" ON CONFLICT (oracle_id, face_idx) DO UPDATE SET name = EXCLUDED.name, oracle_text = EXCLUDED.oracle_text, mana_cost = EXCLUDED.mana_cost, type_line = EXCLUDED.type_line");
        qb.build().execute(&mut *tx).await.context("upserting card_faces")?;
    }
    tx.commit().await?;
    Ok(())
}

async fn insert_printed_names(pool: &PgPool, rows: &[(String, Uuid)]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for chunk in rows.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("INSERT INTO printed_names (printed_name, oracle_id) ");
        qb.push_values(chunk, |mut b, (name, id)| {
            b.push_bind(name).push_bind(id);
        });
        qb.push(" ON CONFLICT (printed_name, oracle_id) DO NOTHING");
        qb.build().execute(&mut *tx).await.context("inserting printed_names")?;
    }
    tx.commit().await?;
    Ok(())
}

async fn insert_rulings(pool: &PgPool, rows: &[RulingRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    // A card's ruling list can shrink; replace the whole list for each touched card.
    let ids: Vec<Uuid> = rows.iter().map(|r| r.oracle_id).collect::<HashSet<_>>().into_iter().collect();
    sqlx::query!("DELETE FROM rulings WHERE oracle_id = ANY($1)", &ids)
        .execute(&mut *tx)
        .await
        .context("clearing rulings")?;
    for chunk in rows.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("INSERT INTO rulings (oracle_id, key, published_at, text) ");
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(r.oracle_id).push_bind(r.key.to_string()).push_bind(&r.published_at).push_unseparated("::date").push_bind(&r.text);
        });
        // The list was just cleared, so a conflict is only possible within this
        // batch and key_rulings has deduplicated it; DO NOTHING is a safety net.
        qb.push(" ON CONFLICT (oracle_id, key) DO NOTHING");
        qb.build().execute(&mut *tx).await.context("inserting rulings")?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Sync all Scryfall-derived tables.
///
/// # Errors
/// On download, parse or database failure.
pub async fn run(pool: &PgPool, cache_dir: &Path) -> Result<()> {
    let client = client()?;
    let index = bulk_index(&client).await?;
    let entry = |kind: &str| index.get(kind).ok_or_else(|| anyhow::anyhow!("bulk index has no `{kind}` entry"));
    let oracle_path = fetch_bulk(&client, cache_dir, entry("oracle_cards")?).await?;
    let default_path = fetch_bulk(&client, cache_dir, entry("default_cards")?).await?;
    let rulings_path = fetch_bulk(&client, cache_dir, entry("rulings")?).await?;

    // 0. Which oracle ids have at least one paper printing (from default_cards).
    // `oracle_cards` holds one representative printing per oracle id, and for
    // ~1000 old cards that printing is MTGO-only; without this pass they would
    // be dropped by the digital-only rule.
    let mut paper: HashSet<Uuid> = HashSet::new();
    for card in jsonl::<ScryfallCard>(&default_path)? {
        let card = card?;
        if let Some(oracle_id) = card.effective_oracle_id()
            && !card.is_skipped_shape()
            && !card.is_digital_only()
        {
            paper.insert(oracle_id);
        }
    }
    tracing::info!(oracle_ids = paper.len(), "oracle ids with a paper printing");

    // 1. cards + card_faces from oracle_cards.
    let mut known: HashSet<Uuid> = HashSet::new();
    let (mut seen, mut skipped) = (0usize, 0usize);
    let mut cards: Vec<CardRow> = Vec::with_capacity(BATCH);
    let mut faces: Vec<FaceRow> = Vec::new();
    for card in jsonl::<ScryfallCard>(&oracle_path)? {
        let card = card?;
        seen += 1;
        let has_paper = card.effective_oracle_id().is_some_and(|id| paper.contains(&id));
        let Some((row, fs)) = card.to_rows(has_paper) else {
            skipped += 1;
            continue;
        };
        if !known.insert(row.oracle_id) {
            continue; // duplicate oracle_id in the file; first wins
        }
        cards.push(row);
        faces.extend(fs);
        if cards.len() >= BATCH {
            insert_cards(pool, &cards, &faces).await?;
            cards.clear();
            faces.clear();
        }
    }
    insert_cards(pool, &cards, &faces).await?;
    tracing::info!(seen, skipped, upserted = known.len(), "cards + card_faces");

    // 2. printed_names from default_cards (every printing).
    let mut names: HashSet<(String, Uuid)> = HashSet::new();
    let mut batch: Vec<(String, Uuid)> = Vec::with_capacity(BATCH);
    let mut printings = 0usize;
    for card in jsonl::<ScryfallCard>(&default_path)? {
        let card = card?;
        printings += 1;
        let Some(oracle_id) = card.effective_oracle_id() else { continue };
        // Names from digital printings of a known paper card are still real names.
        if card.is_skipped_shape() || !known.contains(&oracle_id) {
            continue;
        }
        for name in card.printed_names() {
            if names.insert((name.clone(), oracle_id)) {
                batch.push((name, oracle_id));
            }
        }
        if batch.len() >= BATCH {
            insert_printed_names(pool, &batch).await?;
            batch.clear();
        }
    }
    insert_printed_names(pool, &batch).await?;
    tracing::info!(printings, names = names.len(), "printed_names");

    // 3. rulings (bulk, grouped by oracle_id).
    let mut raw: HashMap<Uuid, Vec<(String, String)>> = HashMap::new();
    let (mut total, mut orphaned) = (0usize, 0usize);
    let mut ruled: HashSet<Uuid> = HashSet::new();
    for r in jsonl::<ScryfallRuling>(&rulings_path)? {
        let r = r?;
        total += 1;
        ruled.insert(r.oracle_id);
        if !known.contains(&r.oracle_id) {
            orphaned += 1;
            continue;
        }
        raw.entry(r.oracle_id).or_default().push((r.published_at, r.comment));
    }
    let mut rows = key_rulings(raw);
    rows.sort_by(|a, b| (a.oracle_id, &a.published_at, &a.key).cmp(&(b.oracle_id, &b.published_at, &b.key)));
    // Group whole cards per batch so the DELETE+INSERT stays consistent per card.
    let mut start = 0;
    while start < rows.len() {
        let mut end = (start + BATCH).min(rows.len());
        if let Some(last) = rows.get(end - 1) {
            let id = last.oracle_id;
            while end < rows.len() && rows.get(end).is_some_and(|r| r.oracle_id == id) {
                end += 1;
            }
        }
        insert_rulings(pool, rows.get(start..end).unwrap_or_default()).await?;
        start = end;
    }
    tracing::info!(total, orphaned, stored = rows.len(), "rulings");

    remove_stale(pool, &known, &ruled).await
}

/// Step 4: drop retired oracle ids (Scryfall merges/retires them occasionally;
/// cascades to faces, printed names, rulings, aliases and notes) and the rulings
/// of cards whose ruling list was withdrawn entirely from the bulk file.
async fn remove_stale(pool: &PgPool, known: &HashSet<Uuid>, ruled: &HashSet<Uuid>) -> Result<()> {
    let known_ids: Vec<Uuid> = known.iter().copied().collect();
    let removed_cards = sqlx::query!("DELETE FROM cards WHERE oracle_id <> ALL($1)", &known_ids)
        .execute(pool)
        .await
        .context("deleting retired cards")?
        .rows_affected();
    let ruled_ids: Vec<Uuid> = ruled.iter().copied().collect();
    let removed_rulings = sqlx::query!("DELETE FROM rulings WHERE oracle_id <> ALL($1)", &ruled_ids)
        .execute(pool)
        .await
        .context("deleting withdrawn rulings")?
        .rows_affected();
    tracing::info!(removed_cards, removed_rulings, "removed rows no longer in the bulk files");
    Ok(())
}

// ---------------------------------------------------------------------------
// Aliases
// ---------------------------------------------------------------------------

/// Parse a flat YAML mapping (`alias: Card Name`). Returns `(alias_lowercased,
/// canonical_name)` pairs sorted by alias; entries with an empty alias or name
/// are reported and dropped.
///
/// # Errors
/// If the text is not a YAML mapping of string to string.
fn parse_alias_yaml(text: &str) -> Result<Vec<(String, String)>> {
    let map: BTreeMap<String, String> = serde_yaml_ng::from_str(text).context("aliases: expected a flat `alias: Card Name` mapping")?;
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

/// Load aliases from `path`, replacing the `card_aliases` table contents.
/// Names are resolved case-insensitively against `cards.name`, then `card_faces.name`.
/// Unresolved names are reported (warning) and skipped.
///
/// # Errors
/// On read, parse or database failure.
pub async fn load_aliases(pool: &PgPool, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let pairs = parse_alias_yaml(&text)?;
    let wanted: Vec<String> = pairs.iter().map(|(_, n)| n.to_lowercase()).collect::<HashSet<_>>().into_iter().collect();

    let mut by_name: BTreeMap<String, Uuid> = BTreeMap::new();
    let rows: Vec<(String, Uuid)> = sqlx::query_as("SELECT lower(name), oracle_id FROM cards WHERE lower(name) = ANY($1)")
        .bind(&wanted)
        .fetch_all(pool)
        .await
        .context("resolving alias names against cards")?;
    by_name.extend(rows);
    let face_rows: Vec<(String, Uuid)> =
        sqlx::query_as("SELECT lower(name), oracle_id FROM card_faces WHERE lower(name) = ANY($1) ORDER BY face_idx DESC")
            .bind(&wanted)
            .fetch_all(pool)
            .await
            .context("resolving alias names against card_faces")?;
    for (name, id) in face_rows {
        by_name.entry(name).or_insert(id);
    }

    let mut resolved: Vec<(String, Uuid)> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    let mut seen_alias: HashSet<String> = HashSet::new();
    for (alias, name) in pairs {
        if !seen_alias.insert(alias.clone()) {
            tracing::warn!(alias, "aliases: duplicate alias; first wins");
            continue;
        }
        if let Some(id) = by_name.get(&name.to_lowercase()) {
            resolved.push((alias, *id));
        } else {
            tracing::warn!(alias, name, "aliases: card name not found in cards/card_faces");
            unresolved.push(name);
        }
    }

    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM card_aliases").execute(&mut *tx).await.context("clearing card_aliases")?;
    for chunk in resolved.chunks(BATCH) {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("INSERT INTO card_aliases (alias, oracle_id) ");
        qb.push_values(chunk, |mut b, (alias, id)| {
            b.push_bind(alias).push_bind(id);
        });
        qb.build().execute(&mut *tx).await.context("inserting card_aliases")?;
    }
    tx.commit().await?;
    tracing::info!(loaded = resolved.len(), unresolved = unresolved.len(), "card_aliases replaced");
    if !unresolved.is_empty() {
        tracing::warn!(names = ?unresolved, "aliases: unresolved card names (run `ingest cards` first?)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ScryfallCard {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn normal_card_maps_to_one_face_from_top_level_fields() {
        let c = parse(
            r#"{"id":"11111111-1111-1111-1111-111111111111","oracle_id":"22222222-2222-2222-2222-222222222222",
            "name":"Lightning Bolt","layout":"normal","type_line":"Instant","cmc":1.0,"color_identity":["R"],
            "keywords":[],"legalities":{"modern":"legal"},"oracle_text":"Lightning Bolt deals 3 damage to any target.",
            "mana_cost":"{R}","set_type":"core","games":["paper","mtgo"]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        assert_eq!(card.name, "Lightning Bolt");
        assert_eq!(card.layout, "normal");
        assert_eq!(card.type_line, "Instant");
        assert_eq!(card.color_identity, vec!["R"]);
        assert_eq!(card.scryfall_id, Some(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()));
        assert_eq!(card.legalities["modern"], "legal");
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].face_idx, 0);
        assert_eq!(faces[0].name, "Lightning Bolt");
        assert_eq!(faces[0].mana_cost, "{R}");
        assert!(faces[0].oracle_text.starts_with("Lightning Bolt deals"));
        assert_eq!(c.printed_names(), vec!["lightning bolt"]);
    }

    #[test]
    fn transform_card_uses_faces() {
        let c = parse(
            r#"{"id":"11111111-1111-1111-1111-111111111111","oracle_id":"33333333-3333-3333-3333-333333333333",
            "name":"Delver of Secrets // Insectile Aberration","layout":"transform",
            "type_line":"Creature — Human Wizard // Creature — Human Insect","cmc":1.0,"color_identity":["U"],
            "keywords":[],"legalities":{},"set_type":"expansion","games":["paper"],
            "card_faces":[
              {"name":"Delver of Secrets","mana_cost":"{U}","type_line":"Creature — Human Wizard","oracle_text":"At the beginning of your upkeep, look at the top card of your library."},
              {"name":"Insectile Aberration","mana_cost":"","type_line":"Creature — Human Insect","oracle_text":"Flying"}
            ]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        assert_eq!(card.name, "Delver of Secrets // Insectile Aberration");
        assert_eq!(faces.len(), 2);
        assert_eq!((faces[0].face_idx, faces[0].name.as_str(), faces[0].mana_cost.as_str()), (0, "Delver of Secrets", "{U}"));
        assert_eq!((faces[1].face_idx, faces[1].name.as_str(), faces[1].oracle_text.as_str()), (1, "Insectile Aberration", "Flying"));
        assert_eq!(
            c.printed_names(),
            vec!["delver of secrets // insectile aberration", "delver of secrets", "insectile aberration"]
        );
    }

    #[test]
    fn mdfc_uses_faces_and_keeps_layout() {
        let c = parse(
            r#"{"oracle_id":"44444444-4444-4444-4444-444444444444","name":"Valakut Awakening // Valakut Stoneforge",
            "layout":"modal_dfc","type_line":"Instant // Land","cmc":3.0,"color_identity":["R"],"set_type":"expansion",
            "card_faces":[
              {"name":"Valakut Awakening","mana_cost":"{2}{R}","type_line":"Instant","oracle_text":"Put any number of cards from your hand on the bottom of your library."},
              {"name":"Valakut Stoneforge","mana_cost":"","type_line":"Land","oracle_text":"Valakut Stoneforge enters tapped."}
            ]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        assert_eq!(card.layout, "modal_dfc");
        assert_eq!(card.scryfall_id, None);
        assert_eq!(faces.iter().map(|f| f.type_line.as_str()).collect::<Vec<_>>(), vec!["Instant", "Land"]);
    }

    #[test]
    fn adventure_card_has_two_faces_sharing_oracle_id() {
        let c = parse(
            r#"{"oracle_id":"55555555-5555-5555-5555-555555555555","name":"Bonecrusher Giant // Stomp","layout":"adventure",
            "type_line":"Creature — Giant // Instant — Adventure","cmc":3.0,"color_identity":["R"],"set_type":"expansion",
            "card_faces":[
              {"name":"Bonecrusher Giant","mana_cost":"{2}{R}","type_line":"Creature — Giant","oracle_text":"Whenever Bonecrusher Giant becomes the target of a spell, Bonecrusher Giant deals 2 damage to that spell's controller."},
              {"name":"Stomp","mana_cost":"{1}{R}","type_line":"Instant — Adventure","oracle_text":"Damage can't be prevented this turn. Stomp deals 2 damage to any target."}
            ]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        let id = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
        assert_eq!(card.oracle_id, id);
        assert!(faces.iter().all(|f| f.oracle_id == id));
        assert_eq!(faces[1].name, "Stomp");
        assert_eq!(faces[1].mana_cost, "{1}{R}");
    }

    #[test]
    fn split_card_keeps_full_name_and_both_faces() {
        let c = parse(
            r#"{"oracle_id":"66666666-6666-6666-6666-666666666666","name":"Fire // Ice","layout":"split",
            "type_line":"Instant // Instant","cmc":4.0,"color_identity":["U","R"],"set_type":"expansion","games":["paper","arena","mtgo"],
            "card_faces":[
              {"name":"Fire","mana_cost":"{1}{R}","type_line":"Instant","oracle_text":"Fire deals 2 damage divided as you choose among one or two targets."},
              {"name":"Ice","mana_cost":"{1}{U}","type_line":"Instant","oracle_text":"Tap target permanent.\nDraw a card."}
            ]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        assert_eq!(card.name, "Fire // Ice");
        assert_eq!(card.color_identity, vec!["U", "R"]);
        assert_eq!(faces.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Fire", "Ice"]);
        assert_eq!(c.printed_names(), vec!["fire // ice", "fire", "ice"]);
    }

    #[test]
    fn reversible_card_takes_oracle_id_and_type_line_from_faces() {
        let c = parse(
            r#"{"name":"Zndrsplt, Eye of Wisdom // Zndrsplt, Eye of Wisdom","layout":"reversible_card","cmc":5.0,"set_type":"expansion",
            "card_faces":[
              {"oracle_id":"77777777-7777-7777-7777-777777777777","name":"Zndrsplt, Eye of Wisdom","type_line":"Legendary Creature — Homunculus","mana_cost":"{4}{U}"},
              {"oracle_id":"77777777-7777-7777-7777-777777777777","name":"Zndrsplt, Eye of Wisdom","type_line":"Legendary Creature — Homunculus","mana_cost":"{4}{U}"}
            ]}"#,
        );
        let (card, faces) = c.to_rows(true).expect("kept");
        assert_eq!(card.oracle_id, Uuid::parse_str("77777777-7777-7777-7777-777777777777").unwrap());
        assert_eq!(card.type_line, "Legendary Creature — Homunculus // Legendary Creature — Homunculus");
        assert_eq!(faces.len(), 2);
    }

    #[test]
    fn skip_rule() {
        let base = |extra: &str| {
            parse(&format!(
                r#"{{"oracle_id":"88888888-8888-8888-8888-888888888888","name":"X","layout":"normal"{extra}}}"#
            ))
        };
        assert!(base("").to_rows(false).is_some());
        assert!(base(r#","games":["paper"]"#).to_rows(false).is_some());
        assert!(base(r#","games":["arena"]"#).to_rows(false).is_none(), "digital-only");
        assert!(base(r#","games":["mtgo"]"#).to_rows(true).is_some(), "MTGO representative of a paper card");
        assert!(base(r#","set_type":"alchemy""#).to_rows(true).is_none());
        assert!(base(r#","set_type":"funny""#).to_rows(false).is_some(), "acorn cards kept");
        for layout in SKIPPED_LAYOUTS {
            assert!(parse(&format!(r#"{{"oracle_id":"88888888-8888-8888-8888-888888888888","name":"X","layout":"{layout}"}}"#)).to_rows(true).is_none());
        }
        assert!(parse(r#"{"name":"No Oracle","layout":"normal"}"#).to_rows(true).is_none());
    }

    #[test]
    fn printed_names_include_flavor_and_face_names_lowercased() {
        let c = parse(
            r#"{"oracle_id":"99999999-9999-9999-9999-999999999999","name":"Ugin's Conjurant","layout":"normal","flavor_name":"Godzilla, King of the Monsters"}"#,
        );
        assert_eq!(c.printed_names(), vec!["ugin's conjurant", "godzilla, king of the monsters"]);
    }

    #[test]
    fn rulings_are_numbered_by_date_then_text() {
        let id = Uuid::parse_str("99999999-9999-9999-9999-999999999999").unwrap();
        let mut raw = HashMap::new();
        raw.insert(
            id,
            vec![
                ("2020-01-01".to_owned(), "b".to_owned()),
                ("2019-05-05".to_owned(), "z".to_owned()),
                ("2020-01-01".to_owned(), "a".to_owned()),
            ],
        );
        let rows = key_rulings(raw);
        assert_eq!(rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["z", "a", "b"]);
        // The key is the ruling's identity: a function of date and text only, so
        // the same ruling gets the same key wherever it sits in the list.
        assert!(rows.iter().all(|r| r.key == ruling_key(&r.published_at, &r.text)));
        assert_eq!(rows.iter().map(|r| &r.key).collect::<std::collections::HashSet<_>>().len(), 3);
    }

    #[test]
    fn alias_yaml_parses_quotes_and_comments() {
        let text = "# nicknames\n---\nBob: Dark Confidant\n\"Jace TMS\": 'Jace, the Mind Sculptor'  # comment\nurborg: Urborg, Tomb of Yawgmoth # note\n\"\": Nothing\n";
        let pairs = parse_alias_yaml(text).expect("parses");
        assert_eq!(
            pairs,
            vec![
                ("bob".to_owned(), "Dark Confidant".to_owned()),
                ("jace tms".to_owned(), "Jace, the Mind Sculptor".to_owned()),
                ("urborg".to_owned(), "Urborg, Tomb of Yawgmoth".to_owned()),
            ]
        );
        assert!(parse_alias_yaml("- not\n- a mapping\n").is_err());
    }
}
