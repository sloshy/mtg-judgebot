//! sqlx adapters for the DB-backed ports. Bodies are `todo!()` until a
//! Postgres instance and `.sqlx` offline data exist (then use `query!`).

use async_trait::async_trait;
use judge_core::{
    CallId, CallStore, Card, Context, Extraction, JudgeError, Question, Resolution, Resolver, Retriever,
    RuleChunk, RuleId, Score, Validated, Verdict,
};
use sqlx::PgPool;

#[derive(Clone, Debug)]
pub struct PgCallStore {
    pool: PgPool,
}

impl PgCallStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CallStore for PgCallStore {
    async fn persist(&self, _q: &Question, _v: &Verdict<Validated>, _ctx: &Context) -> Result<CallId, JudgeError> {
        let _ = &self.pool;
        todo!("INSERT INTO calls (...) RETURNING id")
    }

    async fn rate(&self, _call: CallId, _user_id: &str, _score: Score, _is_judge: bool) -> Result<(), JudgeError> {
        todo!("INSERT INTO ratings ... ON CONFLICT (call_id, user_id) DO UPDATE")
    }
}

/// alias table → `[[bracket]]` syntax → printed-name table → `pg_trgm` fuzzy.
#[derive(Clone, Debug)]
pub struct PgResolver {
    pool: PgPool,
}

impl PgResolver {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Resolver for PgResolver {
    async fn resolve(&self, _span: &str) -> Result<Resolution, JudgeError> {
        let _ = &self.pool;
        todo!("resolution ladder")
    }
}

/// Category map + tsvector BM25 + pgvector cosine, unioned and expanded to full chunks.
#[derive(Clone, Debug)]
pub struct PgRetriever {
    pool: PgPool,
}

impl PgRetriever {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Retriever for PgRetriever {
    async fn retrieve(&self, _q: &Question, _cards: &[Card], _e: &Extraction) -> Result<Context, JudgeError> {
        let _ = &self.pool;
        todo!("three retrieval legs + rulings + glossary + prior calls + notes")
    }

    async fn lookup_rules(&self, _ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
        todo!("SELECT ... FROM rules WHERE id = ANY($1) OR subsection = ANY($1)")
    }
}
