//! The curated lists ([`super::aliases`], [`super::notes`]): where the copy
//! in the database came from (`curated_lists`), and the refresh step that
//! keeps a built-in copy current ([`refresh`]).
//!
//! A loader records its list's [`Source`] and the [`Digest`] of the YAML it
//! loaded in the transaction that replaces the table ([`record`]). The
//! refresh then decides per list ([`plan`]):
//!
//! * built-in, same digest as this binary's copy: the table is compared with
//!   what that copy loads against today's cards, and reloaded when they
//!   differ. A load made before the cards were (or before a card a name
//!   needs) is filled in that way, and a card Scryfall dropped and restored
//!   comes back;
//! * built-in, another digest: an upgrade changed the list, so it is reloaded
//!   from this binary's copy;
//! * a file: the operator's own list, left alone. Loading a file opts the list
//!   out of built-in updates until it is loaded again with no file.
//!
//! A list with no record was loaded by a release before the record existed.
//! It is taken as built-in (and reloaded, which records it) only when its
//! table holds exactly the rows that this binary's copy, or an earlier
//! release's ([`List::legacy`]), loads against today's cards ([`holds`]).
//! Anything else, an operator's additions, removals or remappings, is left
//! alone with a warning naming the two commands that settle it. Equality
//! rather than "every row is in the built-in copy": a list an operator trimmed
//! is a subset of the built-in one, and is theirs.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::{PgConnection, PgPool};

use super::{RefreshLease, aliases, notes, runs::Outcome};

/// A curated list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    /// Card nicknames, `card_aliases`.
    Aliases,
    /// Notes on hard cards, `card_notes`.
    Notes,
}

impl List {
    /// Both, in the order they load.
    pub const ALL: [Self; 2] = [Self::Aliases, Self::Notes];

    /// The name stored in `curated_lists.list`, and the `judgebot ingest`
    /// command that loads it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Aliases => "aliases",
            Self::Notes => "notes",
        }
    }

    /// The copy of `data/*.yaml` this binary was built with.
    #[must_use]
    pub const fn builtin(self) -> &'static str {
        match self {
            Self::Aliases => aliases::BUILTIN,
            Self::Notes => notes::BUILTIN,
        }
    }

    /// Built-in copies of earlier releases (`data/legacy/`), whose rows a
    /// database loaded before the record existed may hold. A change to a
    /// list's rows appends the text it replaces here, so such a database is
    /// still recognised as built-in after the upgrade.
    #[must_use]
    pub const fn legacy(self) -> &'static [&'static str] {
        match self {
            Self::Aliases => aliases::LEGACY,
            Self::Notes => notes::LEGACY,
        }
    }
}

/// Where a loaded list came from, as `curated_lists.source` stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The copy compiled into the binary.
    Builtin,
    /// A file the operator named.
    File,
}

impl Source {
    /// The stored value (the table's check constraint lists both).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::File => "file",
        }
    }

    fn stored(s: &str) -> Result<Self> {
        match s {
            "builtin" => Ok(Self::Builtin),
            "file" => Ok(Self::File),
            other => anyhow::bail!("curated_lists.source {other:?} is neither builtin nor file"),
        }
    }
}

/// The YAML a loader reads: the built-in copy, or the text of an operator's
/// file. The source it records follows from which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListText {
    /// [`List::builtin`].
    Builtin,
    /// A file's text.
    File(String),
}

impl ListText {
    /// The source this text is recorded as.
    #[must_use]
    pub const fn source(&self) -> Source {
        match self {
            Self::Builtin => Source::Builtin,
            Self::File(_) => Source::File,
        }
    }

    /// The YAML to load as `list`.
    #[must_use]
    pub fn yaml(&self, list: List) -> &str {
        match self {
            Self::Builtin => list.builtin(),
            Self::File(text) => text,
        }
    }
}

/// The SHA-256 of a list's YAML text, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest(String);

impl Digest {
    /// The digest of `yaml`.
    #[must_use]
    pub fn of(yaml: &str) -> Self {
        use std::fmt::Write as _;
        Self(Sha256::digest(yaml.as_bytes()).iter().fold(
            String::with_capacity(64),
            |mut hex, b| {
                // Writing to a String cannot fail.
                let _ = write!(hex, "{b:02x}");
                hex
            },
        ))
    }

    fn stored(s: String) -> Result<Self> {
        anyhow::ensure!(
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "curated_lists.digest {s:?} is not a SHA-256 in lowercase hex"
        );
        Ok(Self(s))
    }
}

/// A list's row in `curated_lists`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Where the loaded copy came from.
    pub source: Source,
    /// The digest of the YAML loaded.
    pub digest: Digest,
}

/// Whether a resolver logs the entries it cannot resolve. A load does; a
/// comparison the refresh makes every run does not, or every run would
/// repeat the same warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Warnings {
    /// Log each one.
    Log,
    /// Count them only.
    Quiet,
}

/// What a loader resolved from a list's YAML against today's cards.
#[derive(Debug)]
pub(super) struct Resolved<R> {
    /// The table rows, in the list's order.
    pub rows: Vec<R>,
    /// Card names that resolved to no card, or to several.
    pub unresolved: Vec<String>,
}

impl<R> Default for Resolved<R> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            unresolved: Vec::new(),
        }
    }
}

impl<R> Resolved<R> {
    pub(super) const fn loaded(&self) -> Loaded {
        Loaded {
            rows: self.rows.len(),
            unresolved: self.unresolved.len(),
        }
    }
}

/// What a load wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loaded {
    /// Rows in the table after it.
    pub rows: usize,
    /// Entries whose card name resolved to no card, or to several.
    pub unresolved: usize,
}

/// Why a list was left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// The operator loaded it from a file.
    File,
    /// No record says where it came from, and the table does not hold what
    /// a built-in copy loads: it may be the operator's own.
    Unrecorded,
}

impl std::fmt::Display for Keep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::File => "loaded from the operator's file",
            Self::Unrecorded => {
                "no record of its source, and it is not a built-in copy: it may be the \
                 operator's own"
            }
        })
    }
}

/// What the refresh does with one list, from its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// It is this binary's built-in copy: compare the table with what that
    /// copy loads today, and reload when they differ.
    Verify,
    /// An earlier built-in copy: reload it from this binary's.
    Reload,
    /// No record: adopt it as built-in when the table holds what a built-in
    /// copy loads, else keep it ([`Keep::Unrecorded`]).
    Compare,
    /// Leave it alone.
    Keep(Keep),
}

/// The plan for a list from its record (`None`: it has none).
#[must_use]
pub fn plan(record: Option<&Record>, builtin: &Digest) -> Plan {
    match record {
        None => Plan::Compare,
        Some(Record {
            source: Source::File,
            ..
        }) => Plan::Keep(Keep::File),
        Some(Record {
            source: Source::Builtin,
            digest,
        }) if digest == builtin => Plan::Verify,
        Some(Record {
            source: Source::Builtin,
            ..
        }) => Plan::Reload,
    }
}

/// Whether the rows `loaded` are exactly one of `copies` (each the rows a
/// built-in copy loads).
#[must_use]
pub fn holds<R: Ord>(loaded: &BTreeSet<R>, copies: &[BTreeSet<R>]) -> bool {
    copies.iter().any(|c| c == loaded)
}

/// What became of one list, as the run record stores it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ListOutcome {
    /// It was this binary's built-in copy already.
    Current,
    /// Loaded from this binary's built-in copy: by `init`, after an upgrade
    /// changed it, or because the table no longer held what it loads.
    Reloaded(Loaded),
    /// It had no record and held what a built-in copy loads; reloaded and
    /// recorded as built-in.
    Adopted(Loaded),
    /// Left alone.
    Kept {
        /// Why.
        reason: Keep,
    },
    /// The list could not be read, compared or loaded (or was not reached:
    /// its lease was lost).
    Failed {
        /// The error and its causes (`{:#}`).
        error: String,
    },
}

/// What the `lists` step did, per list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// `card_aliases`.
    pub aliases: ListOutcome,
    /// `card_notes`.
    pub notes: ListOutcome,
}

impl Summary {
    /// The step's outcome from each list's: `Ok` when both are, else failed
    /// with both lists' outcomes kept beside the errors.
    #[must_use]
    pub fn outcome(aliases: Result<ListOutcome>, notes: Result<ListOutcome>) -> Outcome<Self> {
        let mut errors = Vec::new();
        let mut each = |list: List, r: Result<ListOutcome>| {
            r.unwrap_or_else(|e| {
                let error = format!("{e:#}");
                errors.push(format!("{}: {error}", list.name()));
                ListOutcome::Failed { error }
            })
        };
        let summary = Self {
            aliases: each(List::Aliases, aliases),
            notes: each(List::Notes, notes),
        };
        if errors.is_empty() {
            Outcome::Ok { summary }
        } else {
            Outcome::Failed {
                error: errors.join("; "),
                summary: Some(summary),
            }
        }
    }
}

/// Record that `list` was just loaded from `text`, inside the loader's
/// transaction.
///
/// # Errors
/// On a database failure.
pub(super) async fn record(tx: &mut PgConnection, list: List, text: &ListText) -> Result<()> {
    sqlx::query!(
        "INSERT INTO curated_lists (list, source, digest) VALUES ($1, $2, $3)
         ON CONFLICT (list) DO UPDATE
         SET source = excluded.source, digest = excluded.digest, loaded_at = now()",
        list.name(),
        text.source().as_str(),
        Digest::of(text.yaml(list)).0,
    )
    .execute(tx)
    .await
    .with_context(|| format!("recording the {} source", list.name()))?;
    Ok(())
}

/// `list`'s record, if it has one.
///
/// # Errors
/// On a database failure, or a stored value outside the check constraints.
pub async fn recorded(pool: &PgPool, list: List) -> Result<Option<Record>> {
    let row = sqlx::query!(
        "SELECT source, digest FROM curated_lists WHERE list = $1",
        list.name()
    )
    .fetch_optional(pool)
    .await
    .with_context(|| format!("reading the {} record", list.name()))?;
    row.map(|r| {
        Ok(Record {
            source: Source::stored(&r.source)?,
            digest: Digest::stored(r.digest)?,
        })
    })
    .transpose()
}

/// Whether `list`'s table holds exactly what one of `texts` loads against
/// today's cards ([`holds`]). Quiet: the loader logs what does not resolve.
async fn table_holds(pool: &PgPool, list: List, texts: &[&str]) -> Result<bool> {
    // Equal texts load equal rows; resolve each once.
    let mut distinct: Vec<&str> = Vec::with_capacity(texts.len());
    for t in texts {
        if !distinct.contains(t) {
            distinct.push(t);
        }
    }
    match list {
        List::Aliases => {
            let mut copies = Vec::with_capacity(distinct.len());
            for t in distinct {
                let r = aliases::resolve(pool, t, Warnings::Quiet).await?;
                copies.push(r.rows.into_iter().collect());
            }
            Ok(holds(&aliases::loaded(pool).await?, &copies))
        }
        List::Notes => {
            let mut copies = Vec::with_capacity(distinct.len());
            for t in distinct {
                let r = notes::resolve(pool, t, Warnings::Quiet).await?;
                copies.push(r.rows.into_iter().collect());
            }
            Ok(holds(&notes::loaded(pool).await?, &copies))
        }
    }
}

/// Load `list` from `text` ([`aliases::run`], [`notes::run`]).
///
/// # Errors
/// The loader's.
pub async fn load(lease: &mut RefreshLease, list: List, text: &ListText) -> Result<Loaded> {
    match list {
        List::Aliases => aliases::run(lease, text).await,
        List::Notes => notes::run(lease, text).await,
    }
}

/// The `lists` step of the refresh: each list brought up to this binary's
/// built-in copy when that is where it came from ([`plan`]). Both lists are
/// tried; a failure in either fails the step, naming the list, with the
/// other's outcome kept ([`Summary::outcome`]).
pub async fn refresh(lease: &mut RefreshLease) -> Outcome<Summary> {
    let aliases = refresh_list(lease, List::Aliases).await;
    let notes = refresh_list(lease, List::Notes).await;
    Summary::outcome(aliases, notes)
}

async fn refresh_list(lease: &mut RefreshLease, list: List) -> Result<ListOutcome> {
    let pool = lease.pool().clone();
    let record = recorded(&pool, list).await?;
    let name = list.name();
    match plan(record.as_ref(), &Digest::of(list.builtin())) {
        Plan::Verify => {
            if table_holds(&pool, list, &[list.builtin()]).await? {
                return Ok(ListOutcome::Current);
            }
            tracing::info!(
                list = name,
                "the {name} table no longer holds what the built-in copy loads (cards added \
                 or removed since it was loaded): reloading it"
            );
            Ok(ListOutcome::Reloaded(
                load(lease, list, &ListText::Builtin).await?,
            ))
        }
        Plan::Reload => {
            tracing::info!(
                list = name,
                "the built-in {name} list changed: reloading it"
            );
            Ok(ListOutcome::Reloaded(
                load(lease, list, &ListText::Builtin).await?,
            ))
        }
        Plan::Compare => {
            let mut copies = vec![list.builtin()];
            copies.extend_from_slice(list.legacy());
            if !table_holds(&pool, list, &copies).await? {
                tracing::warn!(
                    list = name,
                    "{name}: no record of where the loaded list came from, and it is not a \
                     built-in copy, so the refresh leaves it alone. `judgebot ingest {name} \
                     <file>` records your own list as yours; `judgebot ingest {name}` loads the \
                     built-in copy, which the refresh then keeps current"
                );
                return Ok(ListOutcome::Kept {
                    reason: Keep::Unrecorded,
                });
            }
            tracing::info!(
                list = name,
                "the {name} list holds a built-in copy: reloading it and recording it as built-in"
            );
            Ok(ListOutcome::Adopted(
                load(lease, list, &ListText::Builtin).await?,
            ))
        }
        Plan::Keep(reason) => {
            tracing::info!(
                list = name,
                "{name}: loaded from your own file, so the refresh leaves it alone and it gets \
                 no built-in updates; `judgebot ingest {name}` with no file returns it to the \
                 built-in copy"
            );
            Ok(ListOutcome::Kept { reason })
        }
    }
}

/// `init`'s load of `list`: the built-in copy, unless the operator loaded
/// their own file, which is kept (with a warning) so that `init` stays safe
/// to run again.
///
/// # Errors
/// When the record cannot be read or the load fails.
pub async fn init_list(lease: &mut RefreshLease, list: List) -> Result<ListOutcome> {
    let name = list.name();
    match recorded(lease.pool(), list).await?.map(|r| r.source) {
        Some(Source::File) => {
            tracing::warn!(
                list = name,
                "{name}: loaded from your own file; init keeps it. `judgebot ingest {name}` with \
                 no file loads the built-in copy"
            );
            Ok(ListOutcome::Kept { reason: Keep::File })
        }
        Some(Source::Builtin) | None => Ok(ListOutcome::Reloaded(
            load(lease, list, &ListText::Builtin).await?,
        )),
    }
}

/// Fails when no cards are loaded: the lists resolve against them, so a
/// built-in load now would be recorded as current with none of its rows.
///
/// # Errors
/// When no cards are loaded, or they cannot be counted.
pub(super) async fn ensure_cards(pool: &PgPool, list: List) -> Result<()> {
    let any: bool = sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM cards) AS "any!""#)
        .fetch_one(pool)
        .await
        .context("counting cards")?;
    anyhow::ensure!(
        any,
        "no cards are loaded, and the {} list resolves against them (`judgebot ingest cards`)",
        list.name()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::ingest::lease;

    fn builtin(digest: &Digest) -> Record {
        Record {
            source: Source::Builtin,
            digest: digest.clone(),
        }
    }

    /// The step's summary, or the error naming what failed.
    fn ok(outcome: Outcome<Summary>) -> Result<Summary> {
        match outcome {
            Outcome::Ok { summary } => Ok(summary),
            Outcome::Failed { error, .. } => anyhow::bail!(error),
            Outcome::Skipped { reason } => anyhow::bail!("skipped: {reason}"),
        }
    }

    #[test]
    fn a_recorded_list_is_planned_by_its_source_and_digest() {
        let now = Digest::of("bob: Dark Confidant\n");
        let before = Digest::of("bob: Dark Confidant\n# a comment\n");
        assert_ne!(now, before);
        assert_eq!(plan(Some(&builtin(&now)), &now), Plan::Verify);
        assert_eq!(plan(Some(&builtin(&before)), &now), Plan::Reload);
        for digest in [&now, &before] {
            let file = Record {
                source: Source::File,
                digest: digest.clone(),
            };
            assert_eq!(
                plan(Some(&file), &now),
                Plan::Keep(Keep::File),
                "a file is the operator's, even one equal to the built-in copy"
            );
        }
        assert_eq!(
            plan(None, &now),
            Plan::Compare,
            "no record: the rows decide"
        );
    }

    #[test]
    fn a_table_holds_a_copy_only_when_it_equals_one() {
        let set = |rows: &[(&str, u8)]| -> BTreeSet<(String, u8)> {
            rows.iter().map(|(a, c)| ((*a).to_owned(), *c)).collect()
        };
        let now = set(&[("bob", 1), ("snap", 2)]);
        let older = set(&[("bob", 1)]);
        assert!(holds(&now.clone(), std::slice::from_ref(&now)));
        assert!(
            holds(&older.clone(), &[now.clone(), older.clone()]),
            "an earlier release's copy"
        );
        // The operator added one, trimmed one, or pointed one elsewhere.
        for theirs in [
            set(&[("bob", 1), ("snap", 2), ("goyf", 3)]),
            set(&[("bob", 1)]),
            set(&[("bob", 1), ("snap", 3)]),
            set(&[]),
        ] {
            assert!(!holds(&theirs, std::slice::from_ref(&now)), "{theirs:?}");
        }
    }

    #[test]
    fn a_digest_is_lowercase_hex_sha256() -> Result<()> {
        let d = Digest::of("");
        assert_eq!(
            d.0,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(Digest::stored(d.0.clone())?, d);
        assert!(Digest::stored("E3B0".into()).is_err());
        assert_eq!(Source::stored(Source::File.as_str())?, Source::File);
        assert_eq!(Source::stored(Source::Builtin.as_str())?, Source::Builtin);
        Ok(())
    }

    /// Every earlier copy still parses: adoption resolves it.
    #[test]
    fn the_legacy_copies_parse() -> Result<()> {
        for text in List::Aliases.legacy() {
            assert!(!aliases::parse_alias_yaml(text)?.is_empty());
        }
        for text in List::Notes.legacy() {
            assert!(!notes::parse_notes_yaml(text)?.is_empty());
        }
        Ok(())
    }

    /// The stored shape of the step's summary (see [`super::super::runs`]).
    #[test]
    fn the_summary_shape_is_pinned() -> Result<()> {
        let summary = Summary {
            aliases: ListOutcome::Reloaded(Loaded {
                rows: 54,
                unresolved: 1,
            }),
            notes: ListOutcome::Kept { reason: Keep::File },
        };
        let value = serde_json::to_value(&summary)?;
        assert_eq!(
            value,
            json!({"aliases": {"action": "reloaded", "rows": 54, "unresolved": 1},
                   "notes": {"action": "kept", "reason": "file"}})
        );
        assert_eq!(serde_json::from_value::<Summary>(value)?, summary);
        let more = json!({"aliases": {"action": "current"},
                          "notes": {"action": "adopted", "rows": 7, "unresolved": 0}});
        let back: Summary = serde_json::from_value(more.clone())?;
        assert_eq!(serde_json::to_value(&back)?, more);
        let kept = json!({"action": "kept", "reason": "unrecorded"});
        assert_eq!(
            serde_json::from_value::<ListOutcome>(kept)?,
            ListOutcome::Kept {
                reason: Keep::Unrecorded
            }
        );
        Ok(())
    }

    /// One list failing fails the step, with the other's outcome kept.
    #[test]
    fn a_failed_list_fails_the_step_and_keeps_the_other() -> Result<()> {
        let outcome = Summary::outcome(
            Ok(ListOutcome::Current),
            Err(anyhow::anyhow!("boom").context("counting cards")),
        );
        assert_eq!(
            serde_json::to_value(&outcome)?,
            json!({"outcome": "failed", "error": "notes: counting cards: boom",
                   "summary": {"aliases": {"action": "current"},
                               "notes": {"action": "failed", "error": "counting cards: boom"}}})
        );
        assert!(matches!(
            Summary::outcome(Ok(ListOutcome::Current), Ok(ListOutcome::Current)),
            Outcome::Ok { .. }
        ));
        Ok(())
    }

    async fn card(pool: &PgPool, name: &str) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO cards (oracle_id, name, layout) VALUES ($1, $2, 'normal')")
            .bind(id)
            .bind(name)
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO printed_names (printed_name, oracle_id) VALUES ($1, $2)")
            .bind(name.to_lowercase())
            .bind(id)
            .execute(pool)
            .await?;
        Ok(id)
    }

    /// Two cards: enough for both lists.
    async fn cards(pool: &PgPool) -> Result<(Uuid, Uuid)> {
        Ok((
            card(pool, "Dark Confidant").await?,
            card(pool, "Snapcaster Mage").await?,
        ))
    }

    async fn aliases_now(pool: &PgPool) -> Result<Vec<(String, Uuid)>> {
        Ok(aliases::loaded(pool).await?.into_iter().collect())
    }

    /// A load writes the table and its record together, and an operator's
    /// file is recorded as theirs; a failed load writes neither; a built-in
    /// load with no cards is refused.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_load_records_its_source_with_the_table(pool: PgPool) -> Result<()> {
        let mut held = lease(&pool, "lists-test").await?;
        let err = aliases::run(&mut held, &ListText::Builtin)
            .await
            .err()
            .map(|e| format!("{e:#}"));
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("no cards are loaded")),
            "{err:?}"
        );
        assert_eq!(recorded(&pool, List::Aliases).await?, None);

        let (bob, _) = cards(&pool).await?;
        let file = ListText::File("bob: Dark Confidant\n".into());
        let loaded = aliases::run(&mut held, &file).await?;
        assert_eq!(
            loaded,
            Loaded {
                rows: 1,
                unresolved: 0
            }
        );
        assert_eq!(aliases_now(&pool).await?, vec![("bob".to_owned(), bob)]);
        assert_eq!(
            recorded(&pool, List::Aliases).await?,
            Some(Record {
                source: Source::File,
                digest: Digest::of("bob: Dark Confidant\n")
            })
        );
        assert_eq!(recorded(&pool, List::Notes).await?, None);

        // A record that cannot be written rolls the table back with it: the
        // built-in copy's rows are not left in place under the file's record.
        sqlx::query(
            "ALTER TABLE curated_lists ADD CONSTRAINT no_builtin CHECK (source <> 'builtin')",
        )
        .execute(&pool)
        .await?;
        assert!(aliases::run(&mut held, &ListText::Builtin).await.is_err());
        assert_eq!(
            aliases_now(&pool).await?,
            vec![("bob".to_owned(), bob)],
            "rolled back with the record"
        );
        assert_eq!(
            recorded(&pool, List::Aliases).await?.map(|r| r.source),
            Some(Source::File)
        );
        held.release().await;
        Ok(())
    }

    /// The step reloads a built-in list whose digest is not this binary's,
    /// leaves an operator's file alone, and does nothing to a current one
    /// until the cards change what it loads.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_step_reloads_a_changed_built_in_list_and_keeps_a_file(pool: PgPool) -> Result<()> {
        cards(&pool).await?;
        let mut held = lease(&pool, "lists-test").await?;
        // An older binary's built-in copy: one alias, a stale digest.
        aliases::run(&mut held, &ListText::File("bob: Dark Confidant\n".into())).await?;
        sqlx::query("UPDATE curated_lists SET source = 'builtin' WHERE list = 'aliases'")
            .execute(&pool)
            .await?;
        // The operator's own notes.
        notes::run(
            &mut held,
            &ListText::File("Dark Confidant: their note\n".into()),
        )
        .await?;

        let summary = ok(refresh(&mut held).await)?;
        assert!(
            matches!(summary.aliases, ListOutcome::Reloaded(_)),
            "{summary:?}"
        );
        assert_eq!(summary.notes, ListOutcome::Kept { reason: Keep::File });
        assert_eq!(
            recorded(&pool, List::Aliases).await?,
            Some(builtin(&Digest::of(aliases::BUILTIN)))
        );
        assert!(
            aliases_now(&pool).await?.iter().any(|(a, _)| a == "snap"),
            "the built-in copy's rows"
        );
        let note: String = sqlx::query_scalar("SELECT note FROM card_notes")
            .fetch_one(&pool)
            .await?;
        assert_eq!(note, "their note", "the operator's list is untouched");

        let again = ok(refresh(&mut held).await)?;
        assert_eq!(again.aliases, ListOutcome::Current);

        // A card a built-in alias names arrives: the current list is filled
        // in, once.
        let goyf = card(&pool, "Tarmogoyf").await?;
        let filled = ok(refresh(&mut held).await)?;
        assert!(
            matches!(filled.aliases, ListOutcome::Reloaded(_)),
            "{filled:?}"
        );
        assert!(
            aliases_now(&pool)
                .await?
                .contains(&("goyf".to_owned(), goyf))
        );
        assert_eq!(ok(refresh(&mut held).await)?.aliases, ListOutcome::Current);
        held.release().await;
        Ok(())
    }

    /// With no record, a table holding what a built-in copy loads is
    /// adopted; one holding anything else, more or less, is left alone.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn an_unrecorded_list_is_adopted_or_kept_by_its_rows(pool: PgPool) -> Result<()> {
        let (bob, snap) = cards(&pool).await?;
        let mut held = lease(&pool, "lists-test").await?;
        let forget = async || {
            sqlx::query("DELETE FROM curated_lists")
                .execute(&pool)
                .await
        };

        // Loaded by a release before the record: the built-in copy.
        aliases::run(&mut held, &ListText::Builtin).await?;
        notes::run(&mut held, &ListText::Builtin).await?;
        forget().await?;
        let summary = ok(refresh(&mut held).await)?;
        assert!(
            matches!(summary.aliases, ListOutcome::Adopted(_)),
            "{summary:?}"
        );
        assert_eq!(
            recorded(&pool, List::Aliases).await?.map(|r| r.source),
            Some(Source::Builtin)
        );

        // A superset (the operator added one) and a subset (they trimmed
        // one) are both theirs.
        let built = aliases_now(&pool).await?;
        let superset = format!("{}\nbobby: Dark Confidant\n", aliases::BUILTIN);
        for theirs in [superset.as_str(), "bob: Dark Confidant\n"] {
            aliases::run(&mut held, &ListText::File(theirs.to_owned())).await?;
            forget().await?;
            let before = aliases_now(&pool).await?;
            let summary = ok(refresh(&mut held).await)?;
            assert_eq!(
                summary.aliases,
                ListOutcome::Kept {
                    reason: Keep::Unrecorded
                },
                "{theirs}"
            );
            assert_eq!(aliases_now(&pool).await?, before, "untouched: {theirs}");
            assert_eq!(recorded(&pool, List::Aliases).await?, None);
        }
        assert!(
            built.contains(&("bob".to_owned(), bob)) && built.contains(&("snap".to_owned(), snap))
        );

        // No cards at all: the step fails rather than record an empty load,
        // and keeps the other list's outcome.
        sqlx::query("DELETE FROM cards").execute(&pool).await?;
        let failed = refresh(&mut held).await;
        let Outcome::Failed {
            error,
            summary: Some(summary),
        } = failed
        else {
            anyhow::bail!("expected a failed step with a summary: {failed:?}");
        };
        assert!(error.contains("aliases: no cards are loaded"), "{error}");
        assert_eq!(summary.notes, ListOutcome::Current);
        held.release().await;
        Ok(())
    }

    /// `init` keeps a list the operator loaded from a file, and loads the
    /// built-in copy otherwise.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn init_keeps_a_file_list(pool: PgPool) -> Result<()> {
        cards(&pool).await?;
        let mut held = lease(&pool, "lists-test").await?;
        notes::run(
            &mut held,
            &ListText::File("Dark Confidant: their note\n".into()),
        )
        .await?;
        assert_eq!(
            init_list(&mut held, List::Notes).await?,
            ListOutcome::Kept { reason: Keep::File }
        );
        let note: String = sqlx::query_scalar("SELECT note FROM card_notes")
            .fetch_one(&pool)
            .await?;
        assert_eq!(note, "their note");
        assert!(matches!(
            init_list(&mut held, List::Aliases).await?,
            ListOutcome::Reloaded(_)
        ));
        assert_eq!(
            recorded(&pool, List::Aliases).await?.map(|r| r.source),
            Some(Source::Builtin)
        );
        held.release().await;
        Ok(())
    }
}
