//! The dashboard queries against a database: what the public report counts
//! (never a private call or run, never past the window), what the filters
//! narrow, when cost and models appear, and the operator's figures.

use super::*;
use crate::budget::Period;

const BLOOD_MOON: &str = "b4f1e8a7-0000-4000-8000-000000000001";
const OKO: &str = "b4f1e8a7-0000-4000-8000-000000000002";

#[test]
fn a_date_reads_only_a_real_day_and_writes_it_back() {
    for day in [
        "2000-01-01",
        "2024-02-29",
        "2026-10-20",
        "2026-12-31",
        "9999-12-31",
    ] {
        assert_eq!(
            Date::parse(day).map(|d| d.to_string()).as_deref(),
            Some(day)
        );
    }
    for bad in [
        "2026-02-29",
        "2026-13-01",
        "2026-04-31",
        "2026-00-10",
        "1999-12-31",
        "2026-1-01",
        "2026/10/01",
        "+026-10-01",
        "2026-10-01T00",
        "",
    ] {
        assert_eq!(Date::parse(bad), None, "{bad}");
    }
    let (a, b) = (Date::parse("2026-02-27"), Date::parse("2026-03-01"));
    assert_eq!(a.map(|a| a.plus(2)), b);
    assert_eq!(a.zip(b).map(|(a, b)| b.since(a)), Some(2));
}

#[test]
fn a_span_resolves_to_at_most_a_year_ending_today_by_default() -> anyhow::Result<()> {
    let today = Date::parse("2026-10-20").ok_or_else(|| anyhow::anyhow!("today"))?;
    let day = |s: &str| Date::parse(s);
    let r = Span::default().resolve(today)?;
    assert_eq!(
        (r.from().to_string(), r.to().to_string(), r.days()),
        ("2026-09-21".into(), "2026-10-20".into(), 30)
    );
    let r = Span {
        days: Some(1),
        ..Span::default()
    }
    .resolve(today)?;
    assert_eq!((r.from(), r.to()), (today, today));
    let r = Span {
        from: day("2025-10-20"),
        to: day("2026-10-20"),
        days: None,
    }
    .resolve(today)?;
    assert_eq!(r.days(), MAX_DAYS);
    assert_eq!(
        Span {
            from: day("2025-10-19"),
            ..Span::default()
        }
        .resolve(today),
        Err(BadRange::TooLong)
    );
    assert_eq!(
        Span {
            from: day("2026-10-21"),
            ..Span::default()
        }
        .resolve(today),
        Err(BadRange::Reversed)
    );
    for (days, err) in [(0, BadRange::NoDays), (MAX_DAYS + 1, BadRange::TooLong)] {
        assert_eq!(
            Span {
                days: Some(days),
                ..Span::default()
            }
            .resolve(today),
            Err(err)
        );
    }
    Ok(())
}

#[test]
fn cost_and_models_are_off_unless_set_and_a_typo_is_refused() {
    let off = Disclosure::parse(None, Some(" ")).unwrap_or(Disclosure {
        cost: true,
        models: true,
    });
    assert_eq!((off.cost(), off.models()), (None, None));
    let on = Disclosure::parse(Some("TRUE"), Some("1")).unwrap_or_default();
    assert!(on.cost().is_some() && on.models().is_some());
    assert_eq!(
        Disclosure::parse(Some("yes please"), None),
        Err(BadDisclosure {
            var: PUBLIC_COST_ENV,
            value: "yes please".into()
        })
    );
    assert_eq!(
        Disclosure::parse(None, Some("2")).map_err(|e| e.var),
        Err(PUBLIC_MODELS_ENV)
    );
}

/// A call asked `days_ago` days ago at noon UTC.
async fn call(
    pool: &PgPool,
    days_ago: i32,
    interface: &str,
    asker: Option<(&str, &str)>,
    private: bool,
    category: &str,
    cards: &[&str],
) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO calls (thread_id, interface, asker_id, asker_name, private, question, \
         answer, category, source, confidence, citations, context_ids, cr_version, created_at) \
         VALUES ('t', $1, $2, $3, $4, 'q?', 'a.', $5, 'cr', 'high', '[]', \
                 jsonb_build_object('cards', $6::jsonb), '20260819', \
                 ((now() AT TIME ZONE 'utc')::date - $7 + time '12:00') AT TIME ZONE 'utc') \
         RETURNING id",
    )
    .bind(interface)
    .bind(asker.map(|a| a.0))
    .bind(asker.map(|a| a.1))
    .bind(private)
    .bind(category)
    .bind(serde_json::json!(cards))
    .bind(days_ago)
    .fetch_one(pool)
    .await?)
}

/// A run that finished `days_ago` days ago at noon UTC, taking `secs`,
/// billed `micro` to `model` for its answer and a tenth of it for its
/// extraction (to `extract`).
#[expect(clippy::too_many_arguments, reason = "a test fixture's columns")]
async fn run(
    pool: &PgPool,
    days_ago: i32,
    interface: &str,
    private: bool,
    outcome: &str,
    call: Option<Uuid>,
    models: (&str, &str),
    micro: i64,
) -> anyhow::Result<Uuid> {
    let run: Uuid = sqlx::query_scalar(
        "INSERT INTO judge_runs (started_at, finished_at, interface, private, outcome, retried, \
         tool_round, call_id) VALUES ( \
           ((now() AT TIME ZONE 'utc')::date - $1 + time '12:00') AT TIME ZONE 'utc' - interval '4 seconds', \
           ((now() AT TIME ZONE 'utc')::date - $1 + time '12:00') AT TIME ZONE 'utc', \
           $2, $3, $4, false, false, $5) RETURNING id",
    )
    .bind(days_ago)
    .bind(interface)
    .bind(private)
    .bind(outcome)
    .bind(call)
    .fetch_one(pool)
    .await?;
    for (seq, stage, model, cost) in [
        (0_i16, "extraction", models.0, micro / 10),
        (1, "synthesis", models.1, micro),
        (2, "embedding", "voyage-3.5", 1),
    ] {
        sqlx::query(
            "INSERT INTO judge_run_charges (run_id, seq, stage, provider, model, input_tokens, \
             output_tokens, cache_read_tokens, cache_write_tokens, micro_usd, cost_estimated) \
             VALUES ($1, $2, $3, 'anthropic', $4, 10, 5, 0, 0, $5, false)",
        )
        .bind(run)
        .bind(seq)
        .bind(stage)
        .bind(model)
        .bind(cost)
        .execute(pool)
        .await?;
    }
    Ok(run)
}

async fn rate(pool: &PgPool, call: Uuid, user: &str, kind: &str, score: i16) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO ratings (call_id, user_id, score, is_judge, rater_kind) VALUES ($1, $2, $3, false, $4)",
    )
    .bind(call)
    .bind(user)
    .bind(score)
    .bind(kind)
    .execute(pool)
    .await?;
    Ok(())
}

const OPUS: &str = "claude-opus-5-5";
const HAIKU: &str = "claude-haiku-5-5";

/// The instance every test below reads: public and private calls and runs
/// on several interfaces, users, categories, cards and models, one public
/// call and run older than a 30-day window.
async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    for (id, name) in [(BLOOD_MOON, "Blood Moon"), (OKO, "Oko, Thief of Crowns")] {
        sqlx::query("INSERT INTO cards (oracle_id, name, layout) VALUES ($1::uuid, $2, 'normal')")
            .bind(id)
            .bind(name)
            .execute(pool)
            .await?;
    }
    let alice = Some(("42", "alice"));
    let bob = Some(("43", "bob"));
    // Public: alice twice on Discord (one about Blood Moon and Oko), the
    // web once about Blood Moon, MCP once.
    let a1 = call(
        pool,
        1,
        "discord",
        alice,
        false,
        "combat",
        &[BLOOD_MOON, OKO],
    )
    .await?;
    let a2 = call(pool, 2, "discord", alice, false, "layers", &[]).await?;
    let w = call(pool, 2, "web", None, false, "combat", &[BLOOD_MOON]).await?;
    let m = call(pool, 3, "mcp", None, false, "stack_and_priority", &[]).await?;
    // Private: bob's Discord calls (all of his), an MCP one.
    let p1 = call(pool, 1, "discord", bob, true, "copying", &[OKO]).await?;
    let p2 = call(pool, 1, "mcp", None, true, "copying", &[OKO]).await?;
    // Public but older than a 30-day window.
    let old = call(pool, 40, "discord", alice, false, "zones", &[OKO]).await?;

    run(
        pool,
        1,
        "discord",
        false,
        "answered",
        Some(a1),
        (HAIKU, OPUS),
        50_000,
    )
    .await?;
    run(
        pool,
        2,
        "discord",
        false,
        "answered",
        Some(a2),
        (HAIKU, OPUS),
        30_000,
    )
    .await?;
    run(
        pool,
        2,
        "web",
        false,
        "answered",
        Some(w),
        (HAIKU, HAIKU),
        10_000,
    )
    .await?;
    run(
        pool,
        2,
        "web",
        false,
        "ambiguous",
        None,
        (HAIKU, HAIKU),
        1_000,
    )
    .await?;
    run(
        pool,
        3,
        "mcp",
        false,
        "upstream",
        None,
        (HAIKU, OPUS),
        2_000,
    )
    .await?;
    run(
        pool,
        3,
        "mcp",
        false,
        "abandoned",
        None,
        (HAIKU, OPUS),
        2_000,
    )
    .await?;
    // Private runs: what a public count must never see.
    run(
        pool,
        1,
        "discord",
        true,
        "answered",
        Some(p1),
        (OPUS, OPUS),
        900_000,
    )
    .await?;
    run(
        pool,
        1,
        "mcp",
        true,
        "answered",
        Some(p2),
        (OPUS, OPUS),
        800_000,
    )
    .await?;
    run(
        pool,
        1,
        "discord",
        true,
        "refused",
        None,
        (OPUS, OPUS),
        700_000,
    )
    .await?;
    run(pool, 1, "discord", true, "at_cap", None, (OPUS, OPUS), 0).await?;
    run(
        pool,
        40,
        "discord",
        false,
        "answered",
        Some(old),
        (HAIKU, OPUS),
        5_000,
    )
    .await?;

    rate(pool, a1, "1", "discord", 3).await?;
    rate(pool, a1, "2", "discord", 1).await?;
    rate(pool, a1, "web:x", "web", 2).await?;
    rate(pool, w, "web:y", "web", 3).await?;
    rate(pool, p1, "3", "discord", 1).await?;
    rate(pool, p1, "web:z", "web", 1).await?;
    rate(pool, old, "4", "discord", 2).await?;
    let _ = m;
    Ok((a1, p1))
}

fn query(days: u32) -> PublicQuery {
    PublicQuery {
        span: Span {
            days: Some(days),
            ..Span::default()
        },
        ..PublicQuery::default()
    }
}

fn names(d: &Dashboard) -> (Vec<String>, Vec<String>, Vec<(String, i64)>) {
    (
        d.categories.iter().map(|c| c.category.clone()).collect(),
        d.cards.iter().map(|c| c.name.clone()).collect(),
        d.top_askers
            .iter()
            .map(|a| (a.name.clone(), a.questions))
            .collect(),
    )
}

/// The public report counts public calls and runs only, whatever it is
/// asked, and carries no cost or model figures unless they are shown.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_public_report_never_counts_a_private_call_or_run(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let d = public(&pool, None, Disclosure::default(), &query(90)).await?;
    assert_eq!(d.questions, 5, "four public calls and the old one");
    assert_eq!(d.askers, 1, "bob asked only in private");
    assert_eq!(d.by_day.len(), 90);
    assert!(d.by_day.iter().all(|day| day.private.is_none()));
    let total: i64 = d
        .by_day
        .iter()
        .map(|x| x.discord + x.web + x.mcp + x.cli + x.agent_session + x.agent)
        .sum();
    assert_eq!(total, 5);
    assert_eq!(
        d.interfaces
            .iter()
            .map(|i| (i.interface, i.questions))
            .collect::<Vec<_>>(),
        [(Asked::Discord, 3), (Asked::Mcp, 1), (Asked::Web, 1)]
    );
    assert_eq!(
        d.outcomes,
        Some(Outcomes {
            answered: 4,
            ambiguous: 1,
            failed: 1,
            ..Outcomes::default()
        }),
        "private runs (a refusal, a capped run) and the abandoned one are not counted"
    );
    assert_eq!(
        d.ratings,
        RatingMix {
            discord: RatingCounts {
                correct: 1,
                partial: 1,
                incorrect: 1
            },
            web: RatingCounts {
                correct: 1,
                partial: 1,
                incorrect: 0
            },
        },
        "the private call's ratings are not counted"
    );
    let (categories, cards, askers) = names(&d);
    assert!(
        !categories.contains(&"copying".to_owned()),
        "{categories:?}"
    );
    assert_eq!(categories.first().map(String::as_str), Some("combat"));
    assert_eq!(
        d.categories.first().map(|c| c.label.as_str()),
        Some(Category::Combat.label())
    );
    assert_eq!(cards, ["Blood Moon", "Oko, Thief of Crowns"]);
    assert_eq!(
        d.cards.iter().map(|c| c.questions).collect::<Vec<_>>(),
        [2, 2],
        "Oko's private calls are not counted"
    );
    assert_eq!(askers, [("alice".to_owned(), 3)]);
    assert_eq!((d.cost, d.models.clone()), (None, None));
    let sent = serde_json::to_value(&d)?;
    for absent in ["cost", "models", "window_days", "private"] {
        assert!(sent.get(absent).is_none(), "{absent} in {sent}");
    }

    // A user filter: bob, who asked only in private, has nothing; alice's
    // public calls; and no run figures either way.
    for (user, questions) in [("bob", 0), ("alice", 3)] {
        let d = public(
            &pool,
            None,
            Disclosure::default(),
            &PublicQuery {
                user: DiscordUsername::try_new(user.to_owned()).ok(),
                ..query(90)
            },
        )
        .await?;
        assert_eq!(d.questions, questions, "{user}");
        assert_eq!(d.outcomes, None, "{user}");
        if user == "bob" {
            assert_eq!(d.ratings, RatingMix::default());
            assert!(d.cards.is_empty() && d.categories.is_empty());
        }
    }

    // An interface: its public calls and runs only.
    let mcp = public(
        &pool,
        None,
        Disclosure::default(),
        &PublicQuery {
            asked: Some(Asked::Mcp),
            ..query(90)
        },
    )
    .await?;
    assert_eq!(mcp.questions, 1);
    assert_eq!(
        mcp.outcomes,
        Some(Outcomes {
            failed: 1,
            ..Outcomes::default()
        })
    );
    Ok(())
}

/// The window hides older calls and runs, and the report starts at it.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_public_report_keeps_to_the_feed_window(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let d = public(&pool, Days::new(30), Disclosure::default(), &query(90)).await?;
    assert_eq!(d.window_days, Some(30));
    assert_eq!(d.questions, 4, "the 40-day-old call is past the window");
    assert_eq!(d.by_day.len(), 31, "from the window's first day to today");
    assert_eq!(d.outcomes.map(|o| o.answered), Some(3), "nor its run");
    assert_eq!(d.ratings.discord.partial, 0, "nor its rating");
    assert!(!names(&d).0.contains(&"zones".to_owned()));
    assert_eq!(
        serde_json::to_value(&d)?.get("window_days"),
        Some(&serde_json::json!(30))
    );
    Ok(())
}

/// Cost and models appear only with their access, and then count public
/// calls only; a model filter needs the models' access to exist at all.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn cost_and_models_appear_only_when_shown(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    let only_cost = Disclosure::parse(Some("true"), None).unwrap_or_default();
    let d = public(&pool, None, only_cost, &query(90)).await?;
    assert_eq!(d.models, None);
    let cost = d.cost.ok_or_else(|| anyhow::anyhow!("cost"))?;
    // a1 50_000 + 5_000 + 1, a2 30_000 + 3_000 + 1, w 10_000 + 1_000 + 1,
    // old 5_000 + 500 + 1; no private run's charges.
    assert_eq!(cost.questions, 4);
    assert!((cost.usd - 0.104_504).abs() < 1e-9, "{}", cost.usd);
    assert!(
        cost.per_question_usd
            .is_some_and(|p| (p - 0.026_126).abs() < 1e-9)
    );
    assert!(!cost.estimated);
    assert!(serde_json::to_value(&d)?.get("cost").is_some());

    let both = Disclosure::parse(Some("true"), Some("true")).unwrap_or_default();
    let d = public(&pool, None, both, &query(90)).await?;
    let models = d.models.ok_or_else(|| anyhow::anyhow!("models"))?;
    assert_eq!(
        models
            .iter()
            .map(|m| (m.model.as_str(), m.answered, m.extracted))
            .collect::<Vec<_>>(),
        [(OPUS, 3, 0), (HAIKU, 1, 4)],
        "public calls only: no private Opus extraction"
    );
    let access = both.models().ok_or_else(|| anyhow::anyhow!("access"))?;
    let by_haiku = public(
        &pool,
        None,
        both,
        &PublicQuery {
            model: FilterText::parse(HAIKU).map(|n| ModelFilter::new(access, n)),
            ..query(90)
        },
    )
    .await?;
    assert_eq!(by_haiku.questions, 4, "every public call read with Haiku");
    let by_opus = public(
        &pool,
        None,
        both,
        &PublicQuery {
            model: FilterText::parse(OPUS).map(|n| ModelFilter::new(access, n)),
            ..query(90)
        },
    )
    .await?;
    assert_eq!(
        by_opus.questions, 3,
        "answered by Opus; the private ones left out"
    );
    assert_eq!(
        by_opus.outcomes.map(|o| (o.answered, o.failed)),
        Some((3, 1)),
        "runs billed to Opus: no private one"
    );
    Ok(())
}

/// The operator's report counts everything, and adds the ledger, the
/// budget, the runs' cost and latency and the failed calls.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_operator_report_counts_everything(pool: PgPool) -> anyhow::Result<()> {
    seed(&pool).await?;
    sqlx::query(
        "INSERT INTO spend_days (day, micro_usd, calls) VALUES \
         ((now() AT TIME ZONE 'utc')::date, 2000000, 4), \
         ((now() AT TIME ZONE 'utc')::date - 1, 500000, 2), \
         ((now() AT TIME ZONE 'utc')::date - 400, 9000000, 9)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO failed_calls (thread_id, private, interface, asker_id, asker_name, question, error) \
         VALUES ('t', true, 'discord', '43', 'bob', 'failed?', 'boom')",
    )
    .execute(&pool)
    .await?;
    // The serving process records its settings beside the ledger.
    crate::budget::record_setting(&pool, Period::Month, 5_000_000).await?;
    let q = OperatorQuery {
        span: Span {
            days: Some(90),
            ..Span::default()
        },
        ..OperatorQuery::default()
    };
    let u = usage(&pool, &q).await?;
    assert_eq!(u.dashboard.questions, 7);
    assert_eq!(u.private, 2);
    assert_eq!(u.failed_calls, 1);
    assert!(
        (u.usd - 2.5).abs() < 1e-9,
        "the 400-day-old ledger row is outside"
    );
    assert_eq!(u.spend_by_day.len(), 2);
    let month_total: f64 = u.spend_by_month.iter().map(|m| m.usd).sum();
    assert!((month_total - 2.5).abs() < 1e-9);
    assert_eq!(u.budget.period.as_deref(), Some("month"));
    assert_eq!(u.budget.cap_usd, Some(5.0));
    assert!(u.budget.recorded_at.is_some());
    assert!(u.budget.period_start.is_some());
    let spent = u.budget.spent_usd.ok_or_else(|| anyhow::anyhow!("spent"))?;
    let left = u.budget.left_usd.ok_or_else(|| anyhow::anyhow!("left"))?;
    assert!((spent + left - 5.0).abs() < 1e-9 || left == 0.0);
    assert_eq!(
        u.dashboard.outcomes,
        Some(Outcomes {
            answered: 6,
            ambiguous: 1,
            refused: 1,
            failed: 2,
            ..Outcomes::default()
        }),
        "private runs counted; the capped run is a failure; the abandoned one in none"
    );
    assert_eq!(u.dashboard.ratings.discord.incorrect, 2);
    assert!(u.dashboard.cost.is_some() && u.dashboard.models.is_some());
    assert!(u.dashboard.by_day.iter().any(|d| d.private == Some(2)));
    let runs = u.run_stats.clone().ok_or_else(|| anyhow::anyhow!("runs"))?;
    assert_eq!(runs.runs, 11);
    assert_eq!(runs.refused_at_cap, 1);
    assert_eq!(
        runs.outcomes.iter().map(|o| o.runs).sum::<i64>(),
        11,
        "every outcome, abandoned included"
    );
    let first = runs
        .most_expensive
        .first()
        .ok_or_else(|| anyhow::anyhow!("dearest"))?;
    assert_eq!(first.interface, "discord");
    assert!((first.usd - 0.990_001).abs() < 1e-9, "{}", first.usd);
    assert_eq!(first.question.as_deref(), Some("q?"));
    assert_eq!(
        runs.by_stage
            .iter()
            .map(|s| s.stage.as_str())
            .collect::<Vec<_>>(),
        ["synthesis", "extraction", "embedding"]
    );
    assert_eq!(
        runs.by_interface.first().map(|i| i.interface.as_str()),
        Some("discord")
    );
    let latency = runs.latency.ok_or_else(|| anyhow::anyhow!("latency"))?;
    assert_eq!(latency.runs, 6);
    assert!((latency.p50_ms - 4000.0).abs() < 1.0, "{latency:?}");
    assert!(runs.usd_per_answer.is_some());
    assert!(
        runs.by_model.iter().any(|m| m.model == "voyage-3.5"),
        "embedding charges are billed by model too"
    );

    // An asker by name or by id narrows the calls and the failed calls, and
    // leaves the runs out.
    for asker in ["bob", "43"] {
        let u = usage(
            &pool,
            &OperatorQuery {
                asker: FilterText::parse(asker),
                ..q.clone()
            },
        )
        .await?;
        assert_eq!(
            (u.dashboard.questions, u.private, u.failed_calls),
            (1, 1, 1),
            "{asker}"
        );
        assert!(u.run_stats.is_none() && u.dashboard.outcomes.is_none());
    }

    // A process cap has no shared period; a changed setting is rewritten.
    crate::budget::record_setting(&pool, Period::Process, 2_000_000).await?;
    let u = usage(&pool, &q).await?;
    assert_eq!(
        (u.budget.period.as_deref(), u.budget.cap_usd),
        (Some("process"), Some(2.0))
    );
    assert_eq!(
        (u.budget.period_start, u.budget.spent_usd, u.budget.left_usd),
        (None, None, None)
    );

    // A range the report cannot cover is refused, not clamped.
    let reversed = OperatorQuery {
        span: Span {
            from: Date::parse("2026-10-02"),
            to: Date::parse("2026-10-01"),
            days: None,
        },
        ..OperatorQuery::default()
    };
    assert!(matches!(
        usage(&pool, &reversed).await,
        Err(StatsError::Range(BadRange::Reversed))
    ));
    Ok(())
}

/// The SQL of the query in `stats.rs` that follows `after` (the function
/// it is in), as the binary sends it.
fn query_text(after: &str) -> anyhow::Result<&'static str> {
    const SOURCE: &str = include_str!("../stats.rs");
    let from = SOURCE
        .find(after)
        .ok_or_else(|| anyhow::anyhow!("{after} not in stats.rs"))?;
    let rest = SOURCE.get(from..).unwrap_or_default();
    let start = rest
        .find("r#\"")
        .ok_or_else(|| anyhow::anyhow!("no query"))?
        + 3;
    let body = rest.get(start..).unwrap_or_default();
    let end = body.find("\"#").ok_or_else(|| anyhow::anyhow!("no end"))?;
    Ok(body.get(..end).unwrap_or_default())
}

/// The public scope reads the feed's partial indexes even in a generic
/// plan, where every parameter is unknown: the public branch says
/// `publicly_visible` unconditionally, and the planner inlines it.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn generic_plans_of_the_public_reports_use_the_partial_indexes(
    pool: PgPool,
) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    for set in [
        "SET plan_cache_mode = force_generic_plan",
        "SET enable_seqscan = off",
    ] {
        sqlx::query(set).execute(&mut *conn).await?;
    }
    for (name, after, args, index) in [
        (
            "dash",
            "async fn dashboard(",
            "(true, 30, '2026-09-01', '2026-09-30', NULL, NULL, false, NULL, true, true, 10)",
            "calls_feed",
        ),
        (
            "outcomes",
            "async fn run_outcomes(",
            "(true, 30, '2026-09-01', '2026-09-30', NULL, NULL)",
            "judge_runs_public_finished_at",
        ),
    ] {
        // The SQL is this crate's own source, and the names constants.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "PREPARE {name} AS {}",
            query_text(after)?
        )))
        .execute(&mut *conn)
        .await?;
        let plan: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN EXECUTE {name}{args}")))
                .fetch_all(&mut *conn)
                .await?;
        let plan = plan.join("\n");
        assert!(plan.contains(index), "{name}: {plan}");
        // Inlined: the predicate is the function's body, not a call to it.
        assert!(!plan.contains("publicly_visible"), "{name}: {plan}");
        // Generic: the parameters are still parameters.
        assert!(plan.contains("$1"), "{name}: {plan}");
    }
    Ok(())
}

/// A statement past the report's limit is a timeout, not a store failure.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_slow_report_is_cut_off_and_says_so(pool: PgPool) -> anyhow::Result<()> {
    let mut tx = bounded(&pool, std::time::Duration::from_millis(20)).await?;
    let slow = sqlx::query("SELECT pg_sleep(1)")
        .execute(&mut *tx)
        .await
        .map_err(read("sleep"));
    assert!(matches!(slow, Err(StatsError::TimedOut)), "{slow:?}");
    Ok(())
}

/// A range that ends before the public window is refused rather than
/// moved; one that starts before it starts at it.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_range_before_the_window_is_refused(pool: PgPool) -> anyhow::Result<()> {
    let ended = PublicQuery {
        span: Span {
            from: Date::parse("2026-01-01"),
            to: Date::parse("2026-01-12"),
            days: None,
        },
        ..PublicQuery::default()
    };
    assert!(matches!(
        public(&pool, Days::new(7), Disclosure::default(), &ended).await,
        Err(StatsError::Range(BadRange::BeforeWindow { days: 7 }))
    ));
    let d = public(&pool, Days::new(7), Disclosure::default(), &query(30)).await?;
    assert_eq!(d.by_day.len(), 8, "the window's first day to today");
    Ok(())
}
