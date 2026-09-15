//! Integration tests against a throwaway database created from `DATABASE_URL`
//! (`#[sqlx::test]` applies `./migrations` to it). `DATABASE_URL` is read via
//! dotenvy, so the workspace `.env` is enough.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use judge_core::{
    AnswerableSource, CallStore, Category, CategoryGuess, Confidence, Embedder, Extraction,
    InputKind, JudgeError, MatchedVia, Qa, Question, Resolution, Resolver, Retriever, RuleId,
    Score, Source, Verdict,
};
use judge_embed::{Provider, Space, WithSpace};
use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    PgCallStore, PgResolver, PgRetriever, Vectors, retire_unsupported,
    space::{VECTOR_TABLES, column_width, record_space, stored_counts, stored_space, switch_space},
};

const BOB: Uuid = Uuid::from_u128(1);
const URZA_MINE: Uuid = Uuid::from_u128(2);
const URZA_TOWER: Uuid = Uuid::from_u128(3);
const URZA_PLANT: Uuid = Uuid::from_u128(4);
const BONECRUSHER: Uuid = Uuid::from_u128(5);
const BALLISTA: Uuid = Uuid::from_u128(6);
const BOLT: Uuid = Uuid::from_u128(7);
const EMERITUS: Uuid = Uuid::from_u128(8);
const LEOTAU: Uuid = Uuid::from_u128(9);
const RAGAVAN: Uuid = Uuid::from_u128(10);
const RASHMI: Uuid = Uuid::from_u128(11);
const BRUNA_FADING: Uuid = Uuid::from_u128(12);
const BRUNA_ALABASTER: Uuid = Uuid::from_u128(13);

/// `(face_idx, face name, oracle text)`.
type FaceSeed = (i16, &'static str, &'static str);

/// One fixture for every sqlx test; its length is the data, not logic.
#[expect(
    clippy::too_many_lines,
    reason = "test fixture: one row per seeded card"
)]
pub(crate) async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    let cards: [(Uuid, &str, &str, &[FaceSeed]); 13] = [
        (
            BOB,
            "Dark Confidant",
            "normal",
            &[(
                0,
                "Dark Confidant",
                "At the beginning of your upkeep, reveal the top card of your library and put that card into your hand. You lose life equal to its mana value.",
            )],
        ),
        (
            URZA_MINE,
            "Urza's Mine",
            "normal",
            &[(0, "Urza's Mine", "{T}: Add {C}.")],
        ),
        (
            URZA_TOWER,
            "Urza's Tower",
            "normal",
            &[(0, "Urza's Tower", "{T}: Add {C}.")],
        ),
        (
            URZA_PLANT,
            "Urza's Power Plant",
            "normal",
            &[(0, "Urza's Power Plant", "{T}: Add {C}.")],
        ),
        (
            BONECRUSHER,
            "Bonecrusher Giant // Stomp",
            "adventure",
            &[
                (
                    0,
                    "Bonecrusher Giant",
                    "Whenever this creature becomes the target of a spell, it deals 2 damage to that spell's controller.",
                ),
                (
                    1,
                    "Stomp",
                    "Damage can't be prevented this turn. Stomp deals 2 damage to any target.",
                ),
            ],
        ),
        (
            BALLISTA,
            "Walking Ballista",
            "normal",
            &[(
                0,
                "Walking Ballista",
                "This creature enters with X +1/+1 counters on it.",
            )],
        ),
        (
            BOLT,
            "Lightning Bolt",
            "normal",
            &[(
                0,
                "Lightning Bolt",
                "Lightning Bolt deals 3 damage to any target.",
            )],
        ),
        (
            EMERITUS,
            "Emeritus of Conflict // Lightning Bolt",
            "prepare",
            &[
                (0, "Emeritus of Conflict", "Prepare {1}{R}"),
                (
                    1,
                    "Lightning Bolt",
                    "Lightning Bolt deals 3 damage to any target.",
                ),
            ],
        ),
        (
            LEOTAU,
            "Grizzled Leotau",
            "normal",
            &[(0, "Grizzled Leotau", "")],
        ),
        (
            RAGAVAN,
            "Ragavan, Nimble Pilferer",
            "normal",
            &[(0, "Ragavan, Nimble Pilferer", "Dash {R}")],
        ),
        (
            RASHMI,
            "Rashmi and Ragavan",
            "normal",
            &[(0, "Rashmi and Ragavan", "")],
        ),
        (
            BRUNA_FADING,
            "Bruna, the Fading Light",
            "normal",
            &[(0, "Bruna, the Fading Light", "")],
        ),
        (
            BRUNA_ALABASTER,
            "Bruna, Light of Alabaster",
            "normal",
            &[(0, "Bruna, Light of Alabaster", "")],
        ),
    ];
    for (id, name, layout, faces) in cards {
        sqlx::query("INSERT INTO cards (oracle_id, name, layout) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(name)
            .bind(layout)
            .execute(pool)
            .await?;
        for &(idx, face, text) in faces {
            sqlx::query("INSERT INTO card_faces (oracle_id, face_idx, name, oracle_text) VALUES ($1, $2, $3, $4)")
                .bind(id).bind(idx).bind(face).bind(text).execute(pool).await?;
        }
        sqlx::query("INSERT INTO printed_names (printed_name, oracle_id) VALUES ($1, $2)")
            .bind(name)
            .bind(id)
            .execute(pool)
            .await?;
        for &(_, face, _) in faces {
            sqlx::query("INSERT INTO printed_names (printed_name, oracle_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                .bind(face).bind(id).execute(pool).await?;
        }
    }
    // One old printed name shared by two cards: an exact match with no full-name winner.
    for id in [URZA_MINE, URZA_TOWER] {
        sqlx::query("INSERT INTO printed_names (printed_name, oracle_id) VALUES ('Urza Land', $1)")
            .bind(id)
            .execute(pool)
            .await?;
    }
    sqlx::query("INSERT INTO card_aliases (alias, oracle_id) VALUES ('bob', $1)")
        .bind(BOB)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO printed_names (printed_name, oracle_id) VALUES ('Urzas Mine (old)', $1)",
    )
    .bind(URZA_MINE)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO card_notes (oracle_id, note) VALUES ($1, 'Stomp is an Adventure: see 715.')",
    )
    .bind(BONECRUSHER)
    .execute(pool)
    .await?;
    for text in [
        "Stomp can target a player.",
        "The creature ability triggers on any spell.",
    ] {
        sqlx::query("INSERT INTO rulings (oracle_id, key, published_at, text) VALUES ($1, $2, '2019-10-04', $3)")
            .bind(BONECRUSHER)
            .bind(judge_core::ruling_key("2019-10-04", text).to_string())
            .bind(text)
            .execute(pool)
            .await?;
    }

    let rules: [(&str, Option<&str>, &str, &str, &str); 8] = [
        (
            "613",
            None,
            "613",
            "Interaction of Continuous Effects",
            "Section header.",
        ),
        (
            "613.1",
            None,
            "613",
            "",
            "The values of an object's characteristics are determined by starting with the actual object. 613.1d Layer 4: Type-changing effects are applied.",
        ),
        (
            "613.1d",
            Some("613.1"),
            "613",
            "",
            "Layer 4: Type-changing effects are applied. This includes effects that change an object's card type, subtype, and/or supertype.",
        ),
        (
            "613.2",
            None,
            "613",
            "",
            "Within layers 1-6, apply effects from characteristic-defining abilities first.",
        ),
        (
            "614.1",
            None,
            "614",
            "",
            "Some continuous effects are replacement effects.",
        ),
        (
            "702.19",
            None,
            "702",
            "Trample",
            "702.19a Trample is a static ability. 702.19b The controller of an attacking creature with trample first assigns the combat damage to the creature(s) blocking it. Once all those blocking creatures are assigned lethal damage, any remaining damage is assigned as its controller chooses.",
        ),
        (
            "702.2",
            None,
            "702",
            "Deathtouch",
            "702.2b Any nonzero amount of combat damage assigned by a source with deathtouch to a creature is considered to be lethal damage.",
        ),
        (
            "903.4",
            None,
            "903",
            "",
            "The Commander variant uses color identity to determine what cards can be in a deck.",
        ),
    ];
    for (id, parent, subsection, heading, body) in rules {
        sqlx::query("INSERT INTO rules (id, parent_id, subsection, heading, body, examples, cr_version) VALUES ($1, $2, $3, $4, $5, '{}', '20260819')")
            .bind(id).bind(parent).bind(subsection).bind(heading).bind(body).execute(pool).await?;
    }
    sqlx::query("INSERT INTO glossary (term, text, cr_version) VALUES ('Damage', 'Damage is dealt to objects and players.', '20260819'), ('Target', 'A chosen object or player.', '20260819'), ('Deathtouch', 'A keyword ability.', '20260819')")
        .execute(pool).await?;
    sqlx::query("INSERT INTO categories (id, label, subsections) VALUES ('layers', 'Layers', '{613}'), ('replacement_effects', 'Replacement effects', '{614,615,616}'), ('damage_and_life', 'Damage', '{119,120,702.15,702.2}')")
        .execute(pool).await?;
    Ok(())
}

fn resolved_name(r: &Resolution) -> Option<(&str, MatchedVia)> {
    match r {
        Resolution::Resolved { card, via } => Some((card.name.as_str(), *via)),
        _ => None,
    }
}

/// The migration that introduced `rulings.key` backfilled it with a SQL
/// expression; every later row is written by the ingest loader from
/// `judge_core::ruling_key`. The two must agree or a citation stored before the
/// migration points at nothing. Non-ASCII text exercises the UTF-8 encoding step.
#[sqlx::test(migrations = "./migrations")]
async fn ruling_key_sql_backfill_matches_rust(pool: PgPool) -> anyhow::Result<()> {
    for (date, text) in [
        ("2019-10-04", "Stomp can target a player."),
        (
            "2004-10-04",
            "If Humility’s effect is applied — “all creatures lose all abilities” — layer 6 governs.",
        ),
        ("2020-01-01", ""),
    ] {
        let sql: String = sqlx::query_scalar(
            "SELECT left(encode(sha256(convert_to(to_char($1::date, 'YYYY-MM-DD') || E'\\n' || $2, 'UTF8')), 'hex'), 16)",
        )
        .bind(date)
        .bind(text)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            sql,
            judge_core::ruling_key(date, text).to_string(),
            "{date} {text:?}"
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn alias_hit(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let r = PgResolver::new(pool).resolve("Bob").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Dark Confidant", MatchedVia::Alias))
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn bracket_hit_on_card_and_face_names(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool);
    let r = resolver.resolve("[[dark confidant]]").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Dark Confidant", MatchedVia::Bracket))
    );
    let r = resolver.resolve("[[Stomp]]").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Bonecrusher Giant // Stomp", MatchedVia::Bracket))
    );
    if let Resolution::Resolved { card, .. } = &r {
        assert_eq!(card.faces.len(), 2);
        assert_eq!(card.faces.last().name, "Stomp");
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn printed_and_exact_rungs(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool.clone());
    let r = resolver.resolve("urzas mine (OLD)").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Urza's Mine", MatchedVia::PrintedName))
    );
    // The current name reports `Exact` even though it is also a printed name.
    let r = resolver.resolve("walking ballista").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Walking Ballista", MatchedVia::Exact))
    );
    sqlx::query("DELETE FROM printed_names")
        .execute(&pool)
        .await?;
    let r = resolver.resolve("walking ballista").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Walking Ballista", MatchedVia::Exact))
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn full_name_beats_face_name_and_shared_names_stay_ambiguous(
    pool: PgPool,
) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool);
    // "Lightning Bolt" is a card and a face of "Emeritus of Conflict // Lightning Bolt".
    let r = resolver.resolve("lightning bolt").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Lightning Bolt", MatchedVia::Exact))
    );
    let r = resolver.resolve("[[Lightning Bolt]]").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Lightning Bolt", MatchedVia::Bracket))
    );
    let r = resolver.resolve("Emeritus of Conflict").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Emeritus of Conflict // Lightning Bolt", MatchedVia::Exact))
    );
    // Two cards share the old printed name and neither's full name is the span.
    match resolver.resolve("urza land").await? {
        Resolution::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 2),
        other => anyhow::bail!("expected Ambiguous, got {other:?}"),
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn fuzzy_ignores_a_span_buried_inside_a_word(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool);
    // strict_word_similarity: "led" inside "Grizzled" is not a word match.
    assert!(matches!(
        resolver.resolve("led").await?,
        Resolution::NotFound { .. }
    ));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn short_name_before_the_comma_beats_fuzzy(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool);
    // strict_word_similarity ties "Ragavan, Nimble Pilferer" with "Rashmi and Ragavan";
    // only the former is *named* Ragavan.
    let r = resolver.resolve("ragavan").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Ragavan, Nimble Pilferer", MatchedVia::ShortName))
    );
    // Two cards share the short name: still a "did you mean…?".
    match resolver.resolve("Bruna").await? {
        Resolution::Ambiguous {
            candidates, via, ..
        } => {
            assert_eq!(candidates.len(), 2);
            assert_eq!(via, MatchedVia::ShortName);
            assert!(candidates.iter().all(|c| c.name.starts_with("Bruna, ")));
        }
        other => anyhow::bail!("expected Ambiguous, got {other:?}"),
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn fuzzy_ambiguous_for_urza(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let r = PgResolver::new(pool).resolve("urza").await?;
    match r {
        Resolution::Ambiguous {
            query,
            candidates,
            via,
        } => {
            assert_eq!(query, "urza");
            assert_eq!(via, MatchedVia::Fuzzy);
            assert!((2..=5).contains(&candidates.len()), "{candidates:?}");
            assert!(candidates.iter().all(|c| c.name.starts_with("Urza's")));
        }
        other => anyhow::bail!("expected Ambiguous, got {other:?}"),
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn fuzzy_resolves_a_typo(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let r = PgResolver::new(pool).resolve("walking balista").await?;
    assert_eq!(
        resolved_name(&r),
        Some(("Walking Ballista", MatchedVia::Fuzzy))
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn not_found_for_gibberish(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool);
    assert!(
        matches!(resolver.resolve("xqzvv plorth").await?, Resolution::NotFound { query } if query == "xqzvv plorth")
    );
    assert!(matches!(
        resolver.resolve("  ").await?,
        Resolution::NotFound { .. }
    ));
    Ok(())
}

fn extraction(categories: &[Category], concepts: &[&str]) -> Extraction {
    let guess = |c: &Category| CategoryGuess {
        category: *c,
        confidence: Confidence::High,
    };
    let (primary, secondary) = categories.split_first().map_or_else(
        || (guess(&Category::Other), vec![]),
        |(p, rest)| (guess(p), rest.iter().map(guess).collect()),
    );
    Extraction {
        card_spans: vec![],
        concepts: concepts.iter().map(|s| (*s).to_owned()).collect(),
        primary,
        secondary,
        source: Source::Cr,
    }
}

/// A validatable answer: long enough, citing the first line of the first rule chunk in `ctx`.
fn cite_first_rule(ctx: &judge_core::Context) -> anyhow::Result<Vec<judge_core::Citation>> {
    let rule = ctx
        .rules
        .first()
        .ok_or_else(|| anyhow::anyhow!("context has no rules to cite"))?;
    let quote = rule
        .body
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    anyhow::ensure!(!quote.is_empty(), "first rule has an empty first line");
    Ok(vec![judge_core::Citation::Rule {
        id: rule.id.clone(),
        quote: judge_core::Quote::try_new(quote)?,
    }])
}

fn question(text: &str) -> Question {
    Question {
        thread_id: "t1".into(),
        text: text.into(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn retrieve_unions_category_map_and_full_text(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let resolver = PgResolver::new(pool.clone());
    let Resolution::Resolved { card, .. } = resolver.resolve("[[Stomp]]").await? else {
        anyhow::bail!("no card")
    };
    let retriever = PgRetriever::new(pool);
    let ctx = retriever
        .retrieve(
            &question("does trample need lethal damage?"),
            &[card],
            &extraction(&[Category::Layers], &["trample lethal damage"]),
        )
        .await?;
    let ids: Vec<&str> = ctx
        .rules
        .iter()
        .map(|r| -> &str { r.id.as_ref() })
        .collect();
    // The layers rules share no word with a trample question, so they follow the full-text hits;
    // rule-level rows only (no "613" section row, no "613.1d" leaf), in id order.
    assert_eq!(ids, ["702.19", "702.2", "613.1", "613.2"]);
    assert!(!ids.contains(&"613") && !ids.contains(&"613.1d"), "{ids:?}");
    // Full-text leg finds trample by phrase and ranks it above deathtouch.
    let trample = ids.iter().position(|id| *id == "702.19");
    let deathtouch = ids.iter().position(|id| *id == "702.2");
    assert!(trample.is_some() && trample < deathtouch, "{ids:?}");
    assert!(!ids.contains(&"903.4"), "{ids:?}");
    assert_eq!(
        ctx.cr_version().map(|v| -> &str { v.as_ref() }),
        Some("20260819")
    );

    assert_eq!(ctx.rulings.len(), 2);
    assert_eq!(
        ctx.rulings.first().map(|r| r.published_at.as_str()),
        Some("2019-10-04")
    );
    let terms: Vec<&str> = ctx.glossary.iter().map(|g| g.term.as_str()).collect();
    assert_eq!(terms, ["Damage", "Target"]);
    assert_eq!(ctx.notes.len(), 1);
    assert!(ctx.prior.is_empty());
    Ok(())
}

/// The synthesis prompt shows a prefix of `Context.rules`, so the order is
/// what the model reads: the primary category ranked by relevance to the
/// question (not by id), then the full-text hits, then the secondary
/// categories.
#[sqlx::test(migrations = "./migrations")]
async fn retrieve_orders_primary_by_relevance_then_full_text_then_secondary(
    pool: PgPool,
) -> anyhow::Result<()> {
    seed(&pool).await?;
    let ctx = PgRetriever::new(pool)
        .retrieve(
            &question("with trample, which abilities apply first within layers?"),
            &[],
            &extraction(
                &[Category::Layers, Category::ReplacementEffects],
                &["apply first"],
            ),
        )
        .await?;
    let ids: Vec<&str> = ctx
        .rules
        .iter()
        .map(|r| -> &str { r.id.as_ref() })
        .collect();
    let at = |id: &str| {
        ids.iter()
            .position(|x| *x == id)
            .ok_or_else(|| anyhow::anyhow!("{id} missing from {ids:?}"))
    };
    // 613.2 matches more of the question ("layers", "apply", "abilities", "first") than 613.1
    // does, so it ranks first despite the higher id.
    assert!(at("613.2")? < at("613.1")?, "{ids:?}");
    // A full-text hit outside every category ranks above the secondary category.
    assert!(at("613.1")? < at("702.19")?, "{ids:?}");
    assert!(at("702.19")? < at("614.1")?, "{ids:?}");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn lookup_rules_expands_subsections(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let retriever = PgRetriever::new(pool);
    let ids = |s: &[&str]| -> Result<Vec<RuleId>, JudgeError> {
        s.iter()
            .map(|id| {
                RuleId::try_new((*id).to_owned())
                    .map_err(anyhow::Error::from)
                    .map_err(JudgeError::from)
            })
            .collect()
    };
    let chunks = retriever.lookup_rules(&ids(&["613", "702.19"])?).await?;
    let got: Vec<&str> = chunks.iter().map(|c| -> &str { c.id.as_ref() }).collect();
    assert_eq!(got, ["613.1", "613.2", "702.19"]);
    // A leaf id returns the leaf and its enclosing rule.
    let chunks = retriever.lookup_rules(&ids(&["613.1d"])?).await?;
    let got: Vec<&str> = chunks.iter().map(|c| -> &str { c.id.as_ref() }).collect();
    assert_eq!(got, ["613.1", "613.1d"]);
    assert_eq!(
        chunks
            .last()
            .and_then(|c| c.parent_id.as_ref())
            .map(|p| -> &str { p.as_ref() }),
        Some("613.1")
    );
    assert!(retriever.lookup_rules(&[]).await?.is_empty());
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn persist_then_prior_calls_and_rate_upserts(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let retriever = PgRetriever::new(pool.clone());
    let store = PgCallStore::new(pool.clone());
    let q = question("how do layers work?");
    let ctx = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    let verdict = Verdict::new(
        "Layer 4 is where type-changing effects apply, before colour and abilities.".into(),
        Confidence::High,
        cite_first_rule(&ctx)?,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let call = store.persist(&q, &verdict, &ctx).await?;

    let ctx2 = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    assert_eq!(ctx2.prior.len(), 1);
    let prior = ctx2
        .prior
        .first()
        .ok_or_else(|| anyhow::anyhow!("no prior"))?;
    assert_eq!(prior.id, call);
    assert_eq!(prior.rating_count, 0);
    assert!((prior.rating - 2.0).abs() < 1e-6);
    // Other categories do not see it.
    let other = retriever
        .retrieve(&q, &[], &extraction(&[Category::Combat], &[]))
        .await?;
    assert!(other.prior.is_empty());
    // A question about a card the call did not involve does not see it either.
    let Resolution::Resolved { card: bob, .. } =
        PgResolver::new(pool.clone()).resolve("bob").await?
    else {
        anyhow::bail!("no bob")
    };
    let unrelated = retriever
        .retrieve(
            &q,
            std::slice::from_ref(&bob),
            &extraction(&[Category::Layers], &[]),
        )
        .await?;
    assert!(unrelated.prior.is_empty());
    let ctx_bob = retriever
        .retrieve(
            &q,
            std::slice::from_ref(&bob),
            &extraction(&[Category::Layers], &[]),
        )
        .await?;
    let v2 = Verdict::new(
        "Bob's trigger resolves and you lose life equal to the card's mana value.".into(),
        Confidence::High,
        cite_first_rule(&ctx_bob)?,
        Category::Layers,
    )
    .validate(&ctx_bob, AnswerableSource::Cr)?;
    let call_bob = store.persist(&q, &v2, &ctx_bob).await?;
    let with_bob = retriever
        .retrieve(
            &q,
            std::slice::from_ref(&bob),
            &extraction(&[Category::Layers], &[]),
        )
        .await?;
    assert_eq!(
        with_bob.prior.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![call_bob]
    );
    sqlx::query("DELETE FROM calls WHERE id = $1")
        .bind(call_bob.into_inner())
        .execute(&pool)
        .await?;

    // rate() upserts on (call_id, user_id).
    store.rate(call, "u1", Score::Correct, false).await?;
    store.rate(call, "u1", Score::Incorrect, true).await?;
    let (n, score, judge): (i64, i16, bool) = sqlx::query_as(
        "SELECT count(*), min(score), bool_or(is_judge) FROM ratings WHERE call_id = $1",
    )
    .bind(call.into_inner())
    .fetch_one(&pool)
    .await?;
    assert_eq!((n, score, judge), (1, 1, true));
    let ctx3 = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    let prior = ctx3
        .prior
        .first()
        .ok_or_else(|| anyhow::anyhow!("no prior"))?;
    assert_eq!(prior.rating_count, 1);
    assert!(
        (prior.rating - 1.0).abs() < 1e-6,
        "judge score overrides: {}",
        prior.rating
    );

    // Five crowd votes of 1 push smoothed_mean below 1.5 -> excluded.
    for u in ["u2", "u3", "u4", "u5"] {
        store.rate(call, u, Score::Incorrect, false).await?;
    }
    let ctx4 = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    assert!(ctx4.prior.is_empty());
    // ... unless a judge says otherwise: the judge score dominates the exclusion too.
    store.rate(call, "u1", Score::Correct, true).await?;
    let ctx5 = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    assert_eq!(ctx5.prior.len(), 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn history_returns_the_last_n_oldest_first(pool: PgPool) -> anyhow::Result<()> {
    let store = PgCallStore::new(pool.clone());
    assert!(store.history("t1", 5).await?.is_empty());
    // Explicit timestamps: `now()` is per statement here, but ordering must not
    // depend on how fast the inserts run.
    for (q, a, secs) in [("q1", "a1", 0.0_f64), ("q2", "a2", 1.0), ("q3", "a3", 2.0)] {
        sqlx::query(
            "INSERT INTO calls (thread_id, question, answer, category, source, cr_version, created_at) \
             VALUES ('t1', $1, $2, 'layers', 'cr', '20260819', timestamptz '2026-08-29 12:00:00+00' + make_interval(secs => $3))",
        )
        .bind(q)
        .bind(a)
        .bind(secs)
        .execute(&pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO calls (thread_id, question, answer, category, source, cr_version) \
         VALUES ('t2', 'elsewhere', 'x', 'layers', 'cr', '20260819')",
    )
    .execute(&pool)
    .await?;
    let qa = |q: &str, a: &str| Qa {
        question: q.into(),
        answer: a.into(),
    };
    assert_eq!(
        store.history("t1", 2).await?,
        vec![qa("q2", "a2"), qa("q3", "a3")]
    );
    assert_eq!(
        store.history("t1", 10).await?,
        vec![qa("q1", "a1"), qa("q2", "a2"), qa("q3", "a3")]
    );
    assert!(store.history("t1", 0).await?.is_empty());
    assert_eq!(store.history("t2", 5).await?, vec![qa("elsewhere", "x")]);
    assert!(store.history("nope", 5).await?.is_empty());
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn possessive_nickname_hits_the_alias_rung(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let r = PgResolver::new(pool).resolve("bob's").await?;
    match r {
        Resolution::Resolved { card, via } => {
            assert_eq!(card.name, "Dark Confidant");
            assert_eq!(via, MatchedVia::Alias);
        }
        other => anyhow::bail!("expected Resolved, got {other:?}"),
    }
    Ok(())
}

/// `(retired?, retired_reason)` of one call.
async fn retirement_state(
    pool: &PgPool,
    id: judge_core::CallId,
) -> anyhow::Result<(bool, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT retired_at IS NOT NULL, retired_reason FROM calls WHERE id = $1")
            .bind(id.into_inner())
            .fetch_one(pool)
            .await?,
    )
}

/// Retirement is a function of the data: a call is live exactly while every
/// citation it was admitted with would be admitted today, and comes back when
/// the cited text does.
#[sqlx::test(migrations = "./migrations")]
async fn retirement_follows_citation_validity_both_ways(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let retriever = PgRetriever::new(pool.clone());
    let store = PgCallStore::new(pool.clone());
    let q = question("how do layers work?");
    let ctx = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    let cites = cite_first_rule(&ctx)?;
    let rule_id = match cites.first() {
        Some(judge_core::Citation::Rule { id, .. }) => id.as_ref().to_owned(),
        other => anyhow::bail!("expected a rule citation, got {other:?}"),
    };
    let verdict = Verdict::new(
        "Layer 4 is where type-changing effects apply, before colour and abilities.".into(),
        Confidence::High,
        cites,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let call = store.persist(&q, &verdict, &ctx).await?;
    let prior_visible = || async {
        let c = retriever
            .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
            .await?;
        anyhow::Ok(c.prior.iter().any(|p| p.id == call))
    };

    // Nothing changed: nothing retired.
    let s = retire_unsupported(&pool).await?;
    assert_eq!(
        (s.checked, s.retired, s.restored, s.still_retired),
        (1, 0, 0, 0)
    );
    assert!(prior_visible().await?);

    // The cited rule is reworded so the quote no longer appears: retired, with
    // the offending citation named, and gone from retrieval.
    let (original,): (String,) = sqlx::query_as("SELECT body FROM rules WHERE id = $1")
        .bind(&rule_id)
        .fetch_one(&pool)
        .await?;
    sqlx::query("UPDATE rules SET body = 'Rewritten in a later release.' WHERE id = $1")
        .bind(&rule_id)
        .execute(&pool)
        .await?;
    let s = retire_unsupported(&pool).await?;
    assert_eq!((s.retired, s.restored, s.still_retired), (1, 0, 0));
    let (retired, reason) = retirement_state(&pool, call).await?;
    assert!(retired);
    assert!(
        reason
            .as_deref()
            .is_some_and(|r| r.starts_with(&format!("unsupported citation: rule {rule_id}:"))),
        "{reason:?}"
    );
    assert!(!prior_visible().await?);

    // A second pass changes nothing but confirms the state.
    let s = retire_unsupported(&pool).await?;
    assert_eq!((s.retired, s.restored, s.still_retired), (0, 0, 1));

    // The text is restored: the call comes back.
    sqlx::query("UPDATE rules SET body = $2 WHERE id = $1")
        .bind(&rule_id)
        .bind(&original)
        .execute(&pool)
        .await?;
    let s = retire_unsupported(&pool).await?;
    assert_eq!((s.retired, s.restored, s.still_retired), (0, 1, 0));
    assert_eq!(retirement_state(&pool, call).await?, (false, None));
    assert!(prior_visible().await?);

    // A citation that no longer decodes (a pre-migration ruling citation that
    // could not be mapped) retires the call rather than counting as nothing cited.
    sqlx::query(r#"UPDATE calls SET citations = '[{"kind":"scryfall_ruling","card":"00000000-0000-0000-0000-000000000001","idx":0,"quote":"x"}]' WHERE id = $1"#)
        .bind(call.into_inner())
        .execute(&pool)
        .await?;
    let s = retire_unsupported(&pool).await?;
    assert_eq!(s.retired, 1);
    let (retired, reason) = retirement_state(&pool, call).await?;
    assert!(
        retired
            && reason
                .as_deref()
                .is_some_and(|r| r.starts_with("stored citations do not decode")),
        "{reason:?}"
    );

    // The CR version no longer gates retrieval on its own: a live call from an
    // older release is still an example.
    sqlx::query("UPDATE calls SET citations = '[]', retired_at = NULL, retired_reason = NULL, cr_version = '20250101' WHERE id = $1")
        .bind(call.into_inner())
        .execute(&pool)
        .await?;
    assert!(
        prior_visible().await?,
        "retrieval filters on retired_at, not cr_version"
    );
    Ok(())
}

/// The two cases the citation check alone would miss or mishandle: a ruling
/// that disappears and comes back keeps its identity (content key), and a
/// card whose Oracle text changes retires a call that only cited the CR.
#[sqlx::test(migrations = "./migrations")]
async fn retirement_sees_rulings_and_context_card_text(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let retriever = PgRetriever::new(pool.clone());
    let store = PgCallStore::new(pool.clone());
    let bonecrusher = judge_core::CardId::new(BONECRUSHER);
    let q = question("can Stomp target a player?");
    let card = super::cards::load_cards(&pool, &[BONECRUSHER]).await?;
    let ctx = retriever
        .retrieve(&q, &card, &extraction(&[Category::Layers], &[]))
        .await?;
    let ruling = ctx
        .rulings
        .iter()
        .find(|r| r.card == bonecrusher && r.text.starts_with("Stomp can target"))
        .ok_or_else(|| anyhow::anyhow!("seeded ruling not retrieved"))?
        .clone();
    let mut cites = cite_first_rule(&ctx)?;
    cites.push(judge_core::Citation::ScryfallRuling {
        card: bonecrusher,
        ruling: ruling.key,
        quote: judge_core::Quote::try_new("target a player")?,
    });
    let verdict = Verdict::new(
        "Yes: Stomp can target a player, and the damage cannot be prevented this turn.".into(),
        Confidence::High,
        cites,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let call = store.persist(&q, &verdict, &ctx).await?;
    assert_eq!(retire_unsupported(&pool).await?.retired, 0);

    // The ruling is removed (as a refresh does before reinserting): retired, naming it.
    sqlx::query("DELETE FROM rulings WHERE oracle_id = $1 AND key = $2")
        .bind(BONECRUSHER)
        .bind(ruling.key.to_string())
        .execute(&pool)
        .await?;
    assert_eq!(retire_unsupported(&pool).await?.retired, 1);
    let (retired, reason) = retirement_state(&pool, call).await?;
    assert!(
        retired
            && reason
                .as_deref()
                .is_some_and(|r| r.starts_with("unsupported citation: ruling")),
        "{reason:?}"
    );

    // Reinserted with the same date and text — a different position would have
    // broken a positional citation; the content key does not care.
    sqlx::query(
        "INSERT INTO rulings (oracle_id, key, published_at, text) VALUES ($1, $2, $3::date, $4)",
    )
    .bind(BONECRUSHER)
    .bind(ruling.key.to_string())
    .bind(&ruling.published_at)
    .bind(&ruling.text)
    .execute(&pool)
    .await?;
    assert_eq!(retire_unsupported(&pool).await?.restored, 1);

    // An erratum to a context card retires the call even though no citation
    // quotes the card: the answer was about that card as it then read.
    sqlx::query("UPDATE card_faces SET oracle_text = oracle_text || ' Stomp can’t target players.' WHERE oracle_id = $1 AND face_idx = 1")
        .bind(BONECRUSHER)
        .execute(&pool)
        .await?;
    assert_eq!(retire_unsupported(&pool).await?.retired, 1);
    let (retired, reason) = retirement_state(&pool, call).await?;
    assert!(
        retired
            && reason
                .as_deref()
                .is_some_and(|r| r.contains("Oracle text of Bonecrusher Giant")),
        "{reason:?}"
    );

    // A call persisted before fingerprints existed declares no card dependency.
    sqlx::query("UPDATE calls SET context_ids = context_ids - 'card_text' WHERE id = $1")
        .bind(call.into_inner())
        .execute(&pool)
        .await?;
    assert_eq!(retire_unsupported(&pool).await?.restored, 1);
    Ok(())
}

// ---------- the vector space ----------

/// A fixed-vector embedder of a chosen space that counts its calls.
struct FakeEmbedder {
    space: Space,
    calls: AtomicUsize,
}

impl FakeEmbedder {
    fn new(provider: Provider, model: &str, dimensions: usize) -> Arc<Self> {
        Arc::new(Self {
            space: Space {
                provider,
                model: model.to_owned(),
                dimensions,
            },
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(&self, texts: &[&str], _kind: InputKind) -> Result<Vec<Vec<f32>>, JudgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|_| vec![0.25; self.space.dimensions])
            .collect())
    }
    fn dimensions(&self) -> usize {
        self.space.dimensions
    }
}

impl WithSpace for FakeEmbedder {
    fn space(&self) -> &Space {
        &self.space
    }
}

fn voyage() -> Space {
    Space {
        provider: Provider::Voyage,
        model: "voyage-3.5".into(),
        dimensions: 1024,
    }
}

/// The HNSW index definitions the catalogue holds, by index name.
async fn index_definitions(pool: &PgPool) -> anyhow::Result<Vec<(String, String)>> {
    let names: Vec<&str> = VECTOR_TABLES.iter().map(|t| t.index).collect();
    Ok(sqlx::query_as::<_, (String, String)>(
        "SELECT indexname, indexdef FROM pg_indexes WHERE indexname = ANY($1) ORDER BY indexname",
    )
    .bind(&names)
    .fetch_all(pool)
    .await?)
}

#[sqlx::test(migrations = "./migrations")]
async fn vectors_embed_only_into_the_stored_space(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    // No row yet: nothing is embedded, so the legs stay off and the embedder is never called.
    let fake = FakeEmbedder::new(Provider::Voyage, "voyage-3.5", 1024);
    let vectors = Vectors::new(pool.clone(), Arc::clone(&fake) as Arc<dyn WithSpace>);
    assert!(!vectors.enabled().await);
    assert!(vectors.embed("lifelink", InputKind::Query).await.is_none());
    assert_eq!(fake.calls(), 0);
    // The row appears (the first `ingest embed`): the same `Vectors` picks it up without a restart.
    record_space(&pool, &voyage()).await?;
    assert!(vectors.enabled().await);
    assert!(
        vectors
            .embed("lifelink", InputKind::Query)
            .await
            .is_some_and(|v| v.as_slice().len() == 1024)
    );
    assert_eq!(fake.calls(), 1);

    // Another model at the same width: a mismatch, never mixed.
    let other = FakeEmbedder::new(Provider::OpenAi, "nomic-embed-text", 1024);
    let mismatched = Arc::new(Vectors::new(
        pool.clone(),
        Arc::clone(&other) as Arc<dyn WithSpace>,
    ));
    assert!(!mismatched.enabled().await);
    assert!(
        mismatched
            .embed("lifelink", InputKind::Query)
            .await
            .is_none()
    );
    assert!(
        format!("{mismatched:?}").contains("Mismatch"),
        "{mismatched:?}"
    );

    // Through the adapters: retrieval still succeeds (the other legs run), a persisted call
    // carries no vector, and the embedder behind the mismatch is never called.
    let q = question("Does lifelink work on Dark Confidant's trigger?");
    let ctx = PgRetriever::new(pool.clone())
        .with_vectors(Arc::clone(&mismatched))
        .retrieve(&q, &[], &extraction(&[Category::Layers], &["lifelink"]))
        .await?;
    assert!(!ctx.rules.is_empty());
    let store = PgCallStore::new(pool.clone()).with_vectors(Arc::clone(&mismatched));
    let v = Verdict::new(
        "Lifelink applies to any damage the creature deals, including from its trigger.".into(),
        Confidence::High,
        cite_first_rule(&ctx)?,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let id = store.persist(&q, &v, &ctx).await?;
    let embedded: bool =
        sqlx::query_scalar("SELECT embedding IS NOT NULL FROM calls WHERE id = $1")
            .bind(id.into_inner())
            .fetch_one(&pool)
            .await?;
    assert!(
        !embedded,
        "a call is stored without a vector rather than with one of another space"
    );
    assert_eq!(other.calls(), 0);
    assert_eq!(fake.calls(), 1);

    // A `reembed` under a running process: the same `Vectors` that was on goes dark on its
    // next use (the row is re-read every time), instead of erroring on the new width.
    let nomic = Space {
        provider: Provider::OpenAi,
        model: "nomic-embed-text".into(),
        dimensions: 768,
    };
    switch_space(&pool, &nomic).await?;
    assert!(!vectors.enabled().await);
    assert!(vectors.embed("lifelink", InputKind::Query).await.is_none());
    assert!(format!("{vectors:?}").contains("Mismatch"), "{vectors:?}");
    assert_eq!(fake.calls(), 1);
    // And the writer's check: under the shared lock the same answer, with a matching
    // embedder allowed to write in that transaction.
    let mut tx = pool.begin().await?;
    assert!(!vectors.hold(&mut tx).await?);
    let matching = FakeEmbedder::new(Provider::OpenAi, "nomic-embed-text", 768);
    let on = Vectors::new(pool.clone(), Arc::clone(&matching) as Arc<dyn WithSpace>);
    assert!(on.hold(&mut tx).await?);
    tx.commit().await?;
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn a_persisted_call_carries_a_vector_only_of_the_stored_space(
    pool: PgPool,
) -> anyhow::Result<()> {
    seed(&pool).await?;
    record_space(&pool, &voyage()).await?;
    let fake = FakeEmbedder::new(Provider::Voyage, "voyage-3.5", 1024);
    let vectors = Arc::new(Vectors::new(
        pool.clone(),
        Arc::clone(&fake) as Arc<dyn WithSpace>,
    ));
    let store = PgCallStore::new(pool.clone()).with_vectors(Arc::clone(&vectors));
    let q = question("Does lifelink work on Dark Confidant's trigger?");
    let ctx = PgRetriever::new(pool.clone())
        .retrieve(&q, &[], &extraction(&[Category::Layers], &["lifelink"]))
        .await?;
    let v = Verdict::new(
        "Lifelink applies to any damage the creature deals.".into(),
        Confidence::High,
        cite_first_rule(&ctx)?,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let embedded = |id: judge_core::CallId| {
        let pool = pool.clone();
        async move {
            anyhow::Ok(
                sqlx::query_scalar::<_, bool>(
                    "SELECT embedding IS NOT NULL FROM calls WHERE id = $1",
                )
                .bind(id.into_inner())
                .fetch_one(&pool)
                .await?,
            )
        }
    };
    // The stored space is the embedder's: the vector is written.
    let id = store.persist(&q, &v, &ctx).await?;
    assert!(embedded(id).await?);
    // The database moves to another model of the same width: the next persist, whose
    // embedder still matched a moment ago, stores no vector rather than a Voyage one.
    let nomic = Space {
        provider: Provider::OpenAi,
        model: "nomic-embed-text".into(),
        dimensions: 1024,
    };
    switch_space(&pool, &nomic).await?;
    let id = store
        .persist(&question("Another question about lifelink?"), &v, &ctx)
        .await?;
    assert!(!embedded(id).await?);
    assert_eq!(
        fake.calls(),
        1,
        "the mismatch is seen before the request goes out"
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn switch_space_retypes_columns_clears_vectors_and_rebuilds_indexes_atomically(
    pool: PgPool,
) -> anyhow::Result<()> {
    seed(&pool).await?;
    record_space(&pool, &voyage()).await?;
    sqlx::query("INSERT INTO glossary (term, text, cr_version, embedding) VALUES ('Lifelink', 'A keyword.', '20260819', $1)")
        .bind(Vector::from(vec![0.1; 1024]))
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE rules SET embedding = $1 WHERE parent_id IS NULL")
        .bind(Vector::from(vec![0.1; 1024]))
        .execute(&pool)
        .await?;
    let before = index_definitions(&pool).await?;
    assert_eq!(before.len(), 3, "{before:?}");
    assert!(stored_counts(&pool).await?.iter().any(|(_, n)| *n > 0));

    let nomic = Space {
        provider: Provider::OpenAi,
        model: "nomic-embed-text".into(),
        dimensions: 768,
    };
    switch_space(&pool, &nomic).await?;
    for t in VECTOR_TABLES {
        assert_eq!(column_width(&pool, t.table).await?, 768, "{}", t.table);
    }
    assert!(
        stored_counts(&pool).await?.iter().all(|(_, n)| *n == 0),
        "{:?}",
        stored_counts(&pool).await?
    );
    assert_eq!(stored_space(&pool).await?, Some(nomic.clone()));
    // The indexes are back exactly as the migrations define them: HNSW, cosine, partial on rules.
    let after = index_definitions(&pool).await?;
    assert_eq!(after, before, "index definitions survive the switch");
    let rules_idx = after
        .iter()
        .find(|(n, _)| n == "rules_embedding_idx")
        .map(|(_, d)| d.clone())
        .unwrap_or_default();
    assert!(
        rules_idx.contains("USING hnsw")
            && rules_idx.contains("vector_cosine_ops")
            && rules_idx.contains("WHERE (parent_id IS NULL)"),
        "{rules_idx}"
    );
    // The new width is what the columns accept now.
    sqlx::query("UPDATE glossary SET embedding = $1")
        .bind(Vector::from(vec![0.2; 768]))
        .execute(&pool)
        .await?;
    assert!(
        sqlx::query("UPDATE glossary SET embedding = $1")
            .bind(Vector::from(vec![0.2; 1024]))
            .execute(&pool)
            .await
            .is_err()
    );
    let held = stored_counts(&pool).await?;
    assert!(
        held.iter().any(|(t, n)| *t == "glossary" && *n > 0),
        "{held:?}"
    );

    // A width pgvector refuses (`vector(N)` allows at most 16000) fails the switch, and
    // nothing of it survives: the width, the vectors and the row are as before.
    let huge = Space {
        provider: Provider::OpenAi,
        model: "huge".into(),
        dimensions: 20_000,
    };
    let err = switch_space(&pool, &huge)
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.contains("ALTER TABLE rules ALTER COLUMN embedding TYPE vector(20000)"),
        "{err}"
    );
    for t in VECTOR_TABLES {
        assert_eq!(column_width(&pool, t.table).await?, 768, "{}", t.table);
    }
    assert_eq!(
        stored_counts(&pool).await?,
        held,
        "the vectors cleared inside the failed transaction are back"
    );
    assert_eq!(stored_space(&pool).await?, Some(nomic.clone()));
    assert_eq!(index_definitions(&pool).await?, before);
    // A width the column accepts but HNSW does not (2000 is its limit): the failing step
    // is `CREATE INDEX`, after the columns were retyped, and still nothing survives. This
    // is the bound `config::Dimensions` enforces at load.
    let wide = Space {
        provider: Provider::OpenAi,
        model: "text-embedding-3-large".into(),
        dimensions: 2048,
    };
    let err = switch_space(&pool, &wide)
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(err.contains("CREATE INDEX rules_embedding_idx"), "{err}");
    assert_eq!(column_width(&pool, "rules").await?, 768);
    assert_eq!(stored_counts(&pool).await?, held);
    assert_eq!(stored_space(&pool).await?, Some(nomic));
    Ok(())
}

/// `/forget` deletes exactly the caller's ratings: another user's stay, the
/// call itself stays, and a second call reports nothing left to delete.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn forget_user_deletes_only_that_users_ratings(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let retriever = PgRetriever::new(pool.clone());
    let store = PgCallStore::new(pool.clone());
    let q = question("how do layers work?");
    let ctx = retriever
        .retrieve(&q, &[], &extraction(&[Category::Layers], &[]))
        .await?;
    let verdict = Verdict::new(
        "Layer 4 is where type-changing effects apply.".into(),
        Confidence::High,
        cite_first_rule(&ctx)?,
        Category::Layers,
    )
    .validate(&ctx, AnswerableSource::Cr)?;
    let call = store.persist(&q, &verdict, &ctx).await?;
    store.rate(call, "user-a", Score::Correct, false).await?;
    store.rate(call, "user-b", Score::Incorrect, true).await?;

    assert_eq!(store.forget_user("user-a").await?, 1);
    assert_eq!(store.forget_user("user-a").await?, 0, "idempotent");
    let left: Vec<String> = sqlx::query_scalar("SELECT user_id FROM ratings ORDER BY user_id")
        .fetch_all(&pool)
        .await?;
    assert_eq!(left, vec!["user-b".to_owned()]);
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM calls")
        .fetch_one(&pool)
        .await?;
    assert_eq!(calls, 1, "the call is not the user's data");
    Ok(())
}
