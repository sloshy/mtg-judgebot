//! The toolbox over a real (temporary) database: the session flow through
//! the ops layer, the built-in pipeline's `unavailable` reply, and the
//! bounds the ops layer adds. `#[sqlx::test]` applies the bot's migrations
//! to a throwaway database off `DATABASE_URL`.

use std::sync::Arc;

use judge_core::{
    Category, CategoryGuess, Citation, Confidence, Extraction, Resolution, RuleId, Source, Verdict,
};
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::{
    Client, Options, Quota, Toolbox, Transport,
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

pub(crate) fn options(quota: Option<Quota>) -> Options {
    Options {
        transport: Transport::Mcp,
        client: Client::standalone(),
        models: None,
        deps_config: judge_bot::DepsConfig::default(),
        vectors: None,
        permits: Arc::new(Semaphore::new(1)),
        judge_quota: quota,
        history_len: 5,
        offer: judge_core::SourceOffer::upstream(judge_core::Commit::Unknown),
        operator: judge_core::Operator::default(),
    }
}

fn toolbox(pool: PgPool, quota: Option<Quota>) -> Toolbox {
    Toolbox::new(pool, options(quota))
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
            pins: vec![],
            private: false,
        })
        .await?,
        JudgeReply::Unavailable { .. }
    ));

    let begun = t
        .begin_session(BeginInput {
            question: "does bolt's damage gain me life?".into(),
            thread: None,
            private: false,
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

    let stored: (String, bool) = sqlx::query_as("SELECT interface, private FROM calls")
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, ("agent_session".to_owned(), false));

    // The follow-up in the same thread sees the answer as history.
    let next = t
        .begin_session(BeginInput {
            question: "and with lifelink?".into(),
            thread: Some(begun.thread.clone()),
            private: true,
        })
        .await?;
    assert!(
        next.extraction
            .user
            .contains("does bolt's damage gain me life?")
    );
    // A private session's call is stored private ...
    let ExtractionReply::Ready { .. } = t
        .submit_extraction(crate::ops::ExtractionInput {
            session: next.session,
            extraction: extraction(),
        })
        .await?
    else {
        anyhow::bail!("expected ready");
    };
    let VerdictReply::Accepted { call: Some(_), .. } = t
        .submit_verdict(VerdictInput {
            session: next.session,
            verdict: Verdict::new(
                "Still no: lifelink would have to be on the source of the damage.".into(),
                Confidence::High,
                vec![Citation::Rule {
                    id: rid("702.15")?,
                    quote: judge_core::Quote::try_new(BODY)?,
                }],
                Category::Combat,
            ),
            persist: true,
        })
        .await?
    else {
        anyhow::bail!("expected a stored call");
    };
    let private: Vec<(String, bool)> =
        sqlx::query_as("SELECT question, private FROM calls ORDER BY private")
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        private,
        vec![
            ("does bolt's damage gain me life?".to_owned(), false),
            ("and with lifelink?".to_owned(), true),
        ]
    );
    // ... so a public follow-up does not read it, and a private one does.
    let public = t
        .begin_session(BeginInput {
            question: "and with deathtouch?".into(),
            thread: Some(begun.thread.clone()),
            private: false,
        })
        .await?;
    assert!(!public.extraction.user.contains("and with lifelink?"));
    let own = t
        .begin_session(BeginInput {
            question: "and with deathtouch?".into(),
            thread: Some(begun.thread),
            private: true,
        })
        .await?;
    assert!(own.extraction.user.contains("and with lifelink?"));
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
        private: false,
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

/// A standalone tool works against its own release and stops, saying what to
/// update, against another major; inside the instance nothing is checked.
#[sqlx::test(migrations = "../bot/migrations")]
async fn a_tool_of_another_major_stops_with_what_to_update(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let record = |v: String| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO instance_release (id, version) VALUES (true, $1) \
                 ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version",
            )
            .bind(v)
            .execute(&pool)
            .await
        }
    };
    record(judge_core::release::Release::this_build().to_string()).await?;
    let t = toolbox(pool.clone(), None);
    assert_eq!(t.release_notice().await, None);
    lookup(&t).await?;

    record("99.0.0".to_owned()).await?;
    let t = toolbox(pool.clone(), None);
    let err = lookup(&t).await.err().map(|e| e.to_string());
    assert!(
        err.as_deref()
            .is_some_and(|e| e.contains("99.0.0") && e.contains("update judge-mcp")),
        "{err:?}"
    );
    assert!(matches!(
        t.release_notice_cached(),
        Some(crate::release::Notice::Stop(_))
    ));

    let availability = judge_bot::maintenance::Availability::upgrading();
    let inside = Toolbox::new(
        pool,
        Options {
            client: Client::instance(availability.clone()),
            ..options(None)
        },
    );
    // Inside the instance a major upgrade's migration is the maintenance
    // message, and an open instance answers.
    let err = lookup(&inside).await.err().map(|e| e.to_string());
    assert_eq!(
        err.as_deref(),
        Some(judge_bot::maintenance::MESSAGE),
        "{err:?}"
    );
    availability.set_open();
    lookup(&inside).await?;
    Ok(())
}

async fn lookup(t: &Toolbox) -> anyhow::Result<Resolution> {
    t.resolve_card(NameInput {
        name: "[[Lightning Bolt]]".into(),
    })
    .await
}

/// The admin operations over a standalone toolbox: the reads see private
/// calls and failed ones.
#[sqlx::test(migrations = "../bot/migrations")]
async fn the_admin_operations_read_everything(pool: PgPool) -> anyhow::Result<()> {
    use crate::ops::{CallInput, CallsInput, FailuresInput, StatsInput};
    let private: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO calls (thread_id, interface, asker_id, asker_name, private, question, answer, \
         category, source, confidence, cr_version) VALUES ('t', 'discord', '42', 'alice', true, \
         'a private question', 'No.', 'combat', 'cr', 'high', '20260819') RETURNING id",
    )
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO failed_calls (thread_id, private, interface, question, error) \
         VALUES ('t', true, 'mcp', 'failed?', 'boom')",
    )
    .execute(&pool)
    .await?;
    // A standalone toolbox's capability, as `Toolbox::from_env` hands it out.
    let t = toolbox(pool, None);
    let admin = crate::admin::Admin::grant();
    let page = t.admin_calls(&admin, CallsInput::default()).await?;
    assert_eq!(
        page.calls.iter().map(|c| c.id).collect::<Vec<_>>(),
        [private]
    );
    let only_public = t
        .admin_calls(
            &admin,
            CallsInput {
                private: Some(false),
                ..CallsInput::default()
            },
        )
        .await?;
    assert!(only_public.calls.is_empty());
    let one = t
        .admin_call(
            &admin,
            CallInput {
                call: judge_core::CallId::new(private),
            },
        )
        .await?;
    assert_eq!(
        one.call.map(|c| (c.asker_id, c.question)),
        Some((Some("42".to_owned()), "a private question".to_owned()))
    );
    let failures = t.failures(&admin, FailuresInput { limit: None }).await?;
    assert_eq!(failures.failures.len(), 1);
    let usage = t
        .stats(
            &admin,
            StatsInput {
                days: Some(7),
                ..StatsInput::default()
            },
        )
        .await?;
    assert_eq!(usage.dashboard.questions, 1);
    assert_eq!(
        usage.private, 1,
        "the operator's report counts private calls"
    );
    assert_eq!(usage.failed_calls, 1);
    assert!(
        t.stats(
            &admin,
            StatsInput {
                from: Some("2026-10-02".into()),
                to: Some("2026-10-01".into()),
                ..StatsInput::default()
            }
        )
        .await
        .is_err(),
        "a reversed range"
    );
    assert!(
        t.admin_calls(
            &admin,
            CallsInput {
                cursor: Some("nope".into()),
                ..CallsInput::default()
            }
        )
        .await
        .is_err()
    );
    Ok(())
}
