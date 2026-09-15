//! `rules` table access shared by the retriever's three legs and `lookup_rules`.
//!
//! "Rule-level" rows are those whose id has the shape `NNN.M` (e.g. `702.19`):
//! that is the chunk granularity of ARCHITECTURE.md §2. Section rows (`702`)
//! and leaf rows (`702.19b`) are only returned when asked for by exact id.

use judge_core::{CrVersion, JudgeError, RuleChunk, RuleId};
use pgvector::Vector;
use sqlx::PgPool;

use super::{bad_row_from, upstream};

/// One `rules` row without `embedding` / `tsv`.
pub(super) struct RuleRow {
    id: String,
    parent_id: Option<String>,
    subsection: String,
    heading: String,
    body: String,
    examples: Vec<String>,
    cr_version: String,
}

impl TryFrom<RuleRow> for RuleChunk {
    type Error = JudgeError;

    fn try_from(r: RuleRow) -> Result<Self, JudgeError> {
        let rule_id = |what: &str, s: String| {
            RuleId::try_new(s.clone())
                .map_err(|e| bad_row_from(e, format!("rules.{what} {s:?} of {}", r.id)))
        };
        Ok(RuleChunk {
            parent_id: r
                .parent_id
                .clone()
                .map(|p| rule_id("parent_id", p))
                .transpose()?,
            subsection: rule_id("subsection", r.subsection.clone())?,
            id: rule_id("id", r.id.clone())?,
            cr_version: CrVersion::try_new(r.cr_version.clone()).map_err(|e| {
                bad_row_from(
                    e,
                    format!("rules.cr_version {:?} of {}", r.cr_version, r.id),
                )
            })?,
            heading: r.heading,
            body: r.body,
            examples: r.examples,
        })
    }
}

fn chunks(rows: Vec<RuleRow>) -> Result<Vec<RuleChunk>, JudgeError> {
    rows.into_iter().map(RuleChunk::try_from).collect()
}

/// Natural ordering key for a rule id: `(section, rule number, letters)`, so
/// `613.2` sorts before `613.10`, `613.10` before `613.10a`, and the two-letter
/// `704.5aa` after `704.5z` (shorter suffix first, then lexicographic).
pub(super) fn sort_key(id: &str) -> (u32, u32, usize, String) {
    let (section, rest) = id.split_once('.').unwrap_or((id, ""));
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let letters: String = rest
        .chars()
        .skip_while(char::is_ascii_digit)
        .take_while(char::is_ascii_alphabetic)
        .collect();
    (
        section.parse().unwrap_or(0),
        digits.parse().unwrap_or(0),
        letters.len(),
        letters,
    )
}

/// Split requested ids into three-digit subsections (to be expanded to their
/// rule-level rows) and exact ids. A leaf id (`704.5q`) also requests its
/// enclosing rule (`704.5`), whose body folds the leaf text in.
pub(super) fn partition_ids<S: AsRef<str>>(
    ids: impl IntoIterator<Item = S>,
) -> (Vec<String>, Vec<String>) {
    let mut subsections = Vec::new();
    let mut exact = Vec::new();
    for id in ids {
        let id = id.as_ref().trim();
        if id.is_empty() {
            continue;
        }
        if id.contains('.') {
            exact.push(id.to_owned());
            let stem = id.trim_end_matches(|c: char| c.is_ascii_alphabetic());
            if stem != id {
                exact.push(stem.to_owned());
            }
        } else {
            subsections.push(id.to_owned());
        }
    }
    (subsections, exact)
}

/// Rule-level rows of `subsections` plus the rows with exactly these `ids`
/// (rule or leaf), in natural id order.
pub(super) async fn by_ids(
    pool: impl sqlx::PgExecutor<'_>,
    subsections: &[String],
    ids: &[String],
) -> Result<Vec<RuleChunk>, JudgeError> {
    if subsections.is_empty() && ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        RuleRow,
        r#"
        SELECT id, parent_id, subsection, heading, body, examples, cr_version
        FROM rules
        WHERE (subsection = ANY($1) AND id ~ '^[0-9]{3}\.[0-9]+$')
           OR id = ANY($2)
        "#,
        subsections,
        ids
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("rules by id"))?;
    let mut out = chunks(rows)?;
    out.sort_by_key(|c| sort_key(c.id.as_ref()));
    Ok(out)
}

/// One category's rules split by whether they share a word with the question.
#[derive(Debug, Default)]
pub(super) struct CategoryRules {
    /// Rows matching a lexeme of the concepts or the question, most relevant
    /// first (the [`bm25`] score), ties in natural id order.
    pub matching: Vec<RuleChunk>,
    /// Rows matching nothing, in natural id order.
    pub rest: Vec<RuleChunk>,
}

/// Category-map leg, ranked: the rule-level rows of `subsections` plus the
/// rows named exactly by `ids`, scored against `concepts`/`question` with the
/// same `ts_rank_cd` expression as [`bm25`].
///
/// A curated category is a *set*, not a ranking: `multi_faced_cards` alone is
/// ~50 chunks, about twice what the synthesis prompt shows. In id order that
/// set shows whichever subsection has the lowest number (split cards before
/// adventurers) whatever was asked.
pub(super) async fn in_subsections_ranked(
    pool: &PgPool,
    subsections: &[String],
    ids: &[String],
    concepts: &str,
    question: &str,
) -> Result<CategoryRules, JudgeError> {
    if subsections.is_empty() && ids.is_empty() {
        return Ok(CategoryRules::default());
    }
    let rows = sqlx::query!(
        r#"
        WITH q AS (
            SELECT (SELECT string_agg(DISTINCT lexeme, ' | ')
                      FROM unnest(to_tsvector('english', $3)) AS t
                     WHERE lexeme ~ '^[[:alpha:]][[:alnum:]]+$')::tsquery AS cq,
                   (SELECT string_agg(DISTINCT lexeme, ' | ')
                      FROM unnest(to_tsvector('english', $4)) AS t
                     WHERE lexeme ~ '^[[:alpha:]][[:alnum:]]+$')::tsquery AS qq
        )
        SELECT r.id, r.parent_id, r.subsection, r.heading, r.body, r.examples, r.cr_version,
               (coalesce(ts_rank_cd(r.tsv, q.cq, 1), 0) * 2 + coalesce(ts_rank_cd(r.tsv, q.qq, 1), 0))::float8 AS "score!"
        FROM rules r, q
        WHERE (r.subsection = ANY($1) AND r.id ~ '^[0-9]{3}\.[0-9]+$')
           OR r.id = ANY($2)
        "#,
        subsections,
        ids,
        concepts,
        question
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("rules by category"))?;
    let mut scored = rows
        .into_iter()
        .map(|r| {
            let row = RuleRow {
                id: r.id,
                parent_id: r.parent_id,
                subsection: r.subsection,
                heading: r.heading,
                body: r.body,
                examples: r.examples,
                cr_version: r.cr_version,
            };
            RuleChunk::try_from(row).map(|c| (r.score, c))
        })
        .collect::<Result<Vec<_>, _>>()?;
    scored.sort_by(|(a, x), (b, y)| {
        b.total_cmp(a)
            .then_with(|| sort_key(x.id.as_ref()).cmp(&sort_key(y.id.as_ref())))
    });
    let (matching, rest): (Vec<_>, Vec<_>) =
        scored.into_iter().partition(|(score, _)| *score > 0.0);
    Ok(CategoryRules {
        matching: matching.into_iter().map(|(_, c)| c).collect(),
        rest: rest.into_iter().map(|(_, c)| c).collect(),
    })
}

/// Full-text leg: rule-level rows matching any lexeme of `concepts` or
/// `question`, ranked by `ts_rank_cd` (concept matches weigh double).
/// Lexemes are OR-ed so that a long question still matches; punctuation-only
/// tokens, numeric-dotted tokens (rule ids) and the stray single letters they
/// shed (`702.19b` → `702.19` + `b`) are dropped.
pub(super) async fn bm25(
    pool: &PgPool,
    concepts: &str,
    question: &str,
    limit: i64,
) -> Result<Vec<RuleChunk>, JudgeError> {
    if concepts.trim().is_empty() && question.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        RuleRow,
        r#"
        WITH q AS (
            SELECT (SELECT string_agg(DISTINCT lexeme, ' | ')
                      FROM unnest(to_tsvector('english', $1)) AS t
                     WHERE lexeme ~ '^[[:alpha:]][[:alnum:]]+$')::tsquery AS cq,
                   (SELECT string_agg(DISTINCT lexeme, ' | ')
                      FROM unnest(to_tsvector('english', $2)) AS t
                     WHERE lexeme ~ '^[[:alpha:]][[:alnum:]]+$')::tsquery AS qq
        )
        SELECT r.id, r.parent_id, r.subsection, r.heading, r.body, r.examples, r.cr_version
        FROM rules r, q
        WHERE r.id ~ '^[0-9]{3}\.[0-9]+$'
          AND (r.tsv @@ q.cq OR r.tsv @@ q.qq)
        ORDER BY coalesce(ts_rank_cd(r.tsv, q.cq, 1), 0) * 2 + coalesce(ts_rank_cd(r.tsv, q.qq, 1), 0) DESC,
                 r.id
        LIMIT $3
        "#,
        concepts,
        question,
        limit
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("rules full-text"))?;
    chunks(rows)
}

/// Vector leg: the `limit` rule-level rows nearest to `embedding` by cosine distance.
/// `parent_id IS NULL` matches the partial HNSW index (only rule-level rows are
/// embedded), so the index scan is not post-filtered down below `limit`.
pub(super) async fn nearest(
    pool: &PgPool,
    embedding: &Vector,
    limit: i64,
) -> Result<Vec<RuleChunk>, JudgeError> {
    let rows = sqlx::query_as!(
        RuleRow,
        r#"
        SELECT id, parent_id, subsection, heading, body, examples, cr_version
        FROM rules
        WHERE parent_id IS NULL AND embedding IS NOT NULL AND id ~ '^[0-9]{3}\.[0-9]+$'
        ORDER BY embedding <=> $1
        LIMIT $2
        "#,
        embedding as _,
        limit
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("rules vector"))?;
    chunks(rows)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn natural_order() {
        let mut ids = vec![
            "613.10a", "613.2", "613", "613.10", "614.1", "613.1d", "704.5aa", "704.5z", "704.5b",
        ];
        ids.sort_by_key(|id| sort_key(id));
        assert_eq!(
            ids,
            [
                "613", "613.1d", "613.2", "613.10", "613.10a", "614.1", "704.5b", "704.5z",
                "704.5aa"
            ]
        );
    }

    #[test]
    fn partitions_and_adds_leaf_parent() {
        let (subs, exact) = partition_ids(["613", "903.4", "704.5q", " ", "702"]);
        assert_eq!(subs, ["613", "702"]);
        assert_eq!(exact, ["903.4", "704.5q", "704.5"]);
    }
}
