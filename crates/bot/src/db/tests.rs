//! Integration tests against a throwaway database created from `DATABASE_URL`
//! (`#[sqlx::test]` applies `./migrations` to it). `DATABASE_URL` is read via
//! dotenvy, so the workspace `.env` is enough.

use judge_core::{
    AnswerableSource, CallStore, Category, CategoryGuess, Confidence, Extraction, JudgeError, MatchedVia, Question,
    Resolution, Resolver, Retriever, RuleId, Score, Source, Verdict,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{PgCallStore, PgResolver, PgRetriever};

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

async fn seed(pool: &PgPool) -> anyhow::Result<()> {
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
    sqlx::query("INSERT INTO rulings (oracle_id, idx, published_at, text) VALUES ($1, 0, '2019-10-04', 'Stomp can target a player.'), ($1, 1, '2019-10-04', 'The creature ability triggers on any spell.')")
        .bind(BONECRUSHER).execute(pool).await?;

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
        Resolution::Ambiguous { candidates, via, .. } => {
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
        Resolution::Ambiguous { query, candidates, via } => {
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
    let guess = |c: &Category| CategoryGuess { category: *c, confidence: Confidence::High };
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
    let rule = ctx.rules.first().ok_or_else(|| anyhow::anyhow!("context has no rules to cite"))?;
    let quote = rule.body.lines().next().unwrap_or_default().trim().to_owned();
    anyhow::ensure!(!quote.is_empty(), "first rule has an empty first line");
    Ok(vec![judge_core::Citation::Rule { id: rule.id.clone(), quote }])
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
    // Category map first, rule-level rows only (no "613" section row, no "613.1d" leaf).
    assert_eq!(ids.first().copied(), Some("613.1"), "{ids:?}");
    assert!(ids.contains(&"613.2"), "{ids:?}");
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
