//! The toolbox over a real (temporary) database: the session flow through
//! the ops layer, the built-in pipeline's `unavailable` reply, and the
//! bounds the ops layer adds. `#[sqlx::test]` applies the bot's migrations
//! to a throwaway database off `DATABASE_URL`.

use std::sync::Arc;

use judge_bot::synth::Harness;
use judge_core::{
    Category, CategoryGuess, Citation, Confidence, Extraction, RuleId, Source, Verdict,
};
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::{
    Options, Quota, Toolbox,
    ops::{
        BeginInput, ExtractionReply, IdsInput, JudgeInput, JudgeReply, LookupInput, NameInput,
        SearchInput, SessionInput, VerdictInput, VerdictReply,
    },
};

const BOLT: uuid::Uuid = uuid::Uuid::from_u128(7);
const BODY: &str = "Damage dealt by a source with lifelink causes that source's controller to gain that much life.";

async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO cards (oracle_id, name, layout) VALUES ($1, 'Lightning Bolt', 'normal')",
    )
    .bind(BOLT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO card_faces (oracle_id, face_idx, name, oracle_text, mana_cost, type_line) \
         VALUES ($1, 0, 'Lightning Bolt', 'Lightning Bolt deals 3 damage to any target.', '{R}', 'Instant')",
    )
    .bind(BOLT)
    .execute(pool)
    .await?;
    for (id, parent, sub, heading, body) in [
        (
            "702",
            None,
            "702",
            "Keyword Abilities",
            "Keyword abilities.",
        ),
        ("702.15", Some("702"), "702", "Lifelink", BODY),
        ("120", None, "120", "Damage", "Damage is dealt."),
    ] {
        sqlx::query(
            "INSERT INTO rules (id, parent_id, subsection, heading, body, examples, cr_version) \
             VALUES ($1, $2, $3, $4, $5, '{}', '20260819')",
        )
        .bind(id)
        .bind(parent)
        .bind(sub)
        .bind(heading)
        .bind(body)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO categories (id, label, subsections) VALUES ('combat', 'Combat', '{702.15}')",
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn toolbox(pool: PgPool, quota: Option<Quota>) -> Toolbox {
    Toolbox::new(
        pool,
        Options {
            harness: Harness::Mcp,
            models: None,
            deps_config: judge_bot::DepsConfig::default(),
            vectors: None,
            permits: Arc::new(Semaphore::new(1)),
            judge_quota: quota,
            history_len: 5,
            offer: judge_core::SourceOffer::upstream(judge_core::Commit::Unknown),
        },
    )
}

fn extraction() -> Extraction {
    Extraction {
        card_spans: vec!["[[Lightning Bolt]]".into()],
        concepts: vec!["lifelink".into()],
        primary: CategoryGuess {
            category: Category::Combat,
            confidence: Confidence::High,
        },
        secondary: vec![],
        source: Source::Cr,
    }
}

fn rid(s: &str) -> anyhow::Result<RuleId> {
    Ok(RuleId::try_new(s.to_owned())?)
}

#[sqlx::test(migrations = "../bot/migrations")]
async fn a_session_through_the_ops_layer(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let t = toolbox(pool.clone(), None);
    assert!(!t.has_pipeline());
    assert!(matches!(
        t.judge(JudgeInput {
            question: "q?".into(),
            thread: None,
            pins: vec![]
        })
        .await?,
        JudgeReply::Unavailable { .. }
    ));

    let begun = t
        .begin_session(BeginInput {
            question: "does bolt's damage gain me life?".into(),
            thread: None,
        })
        .await?;
    let ExtractionReply::Ready { prompt } = t
        .submit_extraction(crate::ops::ExtractionInput {
            session: begun.session,
            extraction: extraction(),
        })
        .await?
    else {
        anyhow::bail!("expected ready");
    };
    assert!(prompt.material.contains("Lightning Bolt"));
    assert!(prompt.lookup_available);

    // Wrong quote: rejected, retry prompt returned, session still open.
    let bad = Verdict::new(
        "No: Lightning Bolt has no lifelink, so its damage does not cause you to gain life.".into(),
        Confidence::High,
        vec![Citation::Rule {
            id: rid("702.15")?,
            quote: judge_core::Quote::try_new("not in there")?,
        }],
        Category::Combat,
    );
    let VerdictReply::Rejected { retry, .. } = t
        .submit_verdict(VerdictInput {
            session: begun.session,
            verdict: bad,
            persist: true,
        })
        .await?
    else {
        anyhow::bail!("expected rejected");
    };
    assert!(retry.question.contains("Previous attempt rejected"));
    let status = t
        .session_status(SessionInput {
            session: begun.session,
        })
        .await?;
    assert_eq!(
        (
            status.stage.as_str(),
            status.attempts,
            status.lookup_available
        ),
        ("awaiting_verdict", Some(1), false)
    );
    assert!(
        t.lookup_rules(LookupInput {
            session: begun.session,
            ids: vec![rid("120")?]
        })
        .await
        .is_err(),
        "forfeited"
    );

    let good = Verdict::new(
        "No: Lightning Bolt has no lifelink, so its damage does not cause you to gain life.".into(),
        Confidence::High,
        vec![Citation::Rule {
            id: rid("702.15")?,
            quote: judge_core::Quote::try_new(BODY)?,
        }],
        Category::Combat,
    );
    let VerdictReply::Accepted {
        answer,
        call,
        persist_error,
    } = t
        .submit_verdict(VerdictInput {
            session: begun.session,
            verdict: good,
            persist: true,
        })
        .await?
    else {
        anyhow::bail!("expected accepted");
    };
    assert_eq!(persist_error, None);
    assert_eq!(
        answer.citations.first().map(|c| c.label.as_str()),
        Some("702.15")
    );
    let again = t
        .persist_session(SessionInput {
            session: begun.session,
        })
        .await?;
    assert_eq!(Some(again.call), call, "persist is idempotent");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM calls")
        .fetch_one(&pool)
        .await?;
    assert_eq!(n, 1);
    let status = t
        .session_status(SessionInput {
            session: begun.session,
        })
        .await?;
    assert_eq!(
        (
            status.stage.as_str(),
            status.attempts,
            status.outcome.as_deref()
        ),
        ("closed", None, Some("answered"))
    );

    // The follow-up in the same thread sees the answer as history.
    let next = t
        .begin_session(BeginInput {
            question: "and with lifelink?".into(),
            thread: Some(begun.thread),
        })
        .await?;
    assert!(
        next.extraction
            .user
            .contains("does bolt's damage gain me life?")
    );
    Ok(())
}

#[sqlx::test(migrations = "../bot/migrations")]
async fn lookups_are_bounded_and_the_judge_quota_counts(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let t = toolbox(
        pool,
        Some(Quota {
            limit: 1,
            window: std::time::Duration::from_hours(1),
        }),
    );
    assert!(
        t.resolve_card(NameInput {
            name: "x".repeat(300)
        })
        .await
        .is_err()
    );
    assert!(
        t.resolve_card(NameInput { name: " ".into() })
            .await
            .is_err()
    );
    let many: Vec<RuleId> = (0..11)
        .map(|i| rid(&format!("{}", 100 + i)))
        .collect::<anyhow::Result<_>>()?;
    assert!(t.get_rules(IdsInput { ids: many }).await.is_err());
    assert!(t.get_rules(IdsInput { ids: vec![] }).await.is_err());
    assert_eq!(
        t.get_rules(IdsInput {
            ids: vec![rid("702")?]
        })
        .await?
        .rules
        .len(),
        1,
        "a subsection expands to its rules"
    );
    assert!(
        t.search_rules(SearchInput {
            query: "  ".into(),
            limit: None
        })
        .await
        .is_err()
    );
    let hits = t
        .search_rules(SearchInput {
            query: "lifelink".into(),
            limit: Some(1000),
        })
        .await?;
    assert!(hits.rules.len() <= crate::ops::MAX_SEARCH);
    assert!(hits.rules.iter().any(|r| r.id.as_ref() == "702.15"));
    // Without a pipeline, `unavailable` comes before the quota and does not spend it.
    let q = || JudgeInput {
        question: "q?".into(),
        thread: None,
        pins: vec![],
    };
    assert!(matches!(
        t.judge(q()).await?,
        JudgeReply::Unavailable { .. }
    ));
    assert!(matches!(
        t.judge(q()).await?,
        JudgeReply::Unavailable { .. }
    ));
    assert!(t.allow_judge(), "the quota is untouched");
    assert!(!t.allow_judge(), "one run per window");
    Ok(())
}
