//! MTG Judge Bot — domain core.
//!
//! This crate holds the language-neutral domain model from
//! `docs/ARCHITECTURE.md` §5, the port traits, citation validation and the
//! `judge()` pipeline. It deliberately has **no** tokio/reqwest/sqlx
//! dependency: any I/O here is a build failure by dependency graph.

pub mod category;
pub mod domain;
pub mod error;
pub mod freshness;
pub mod judge;
pub mod operator;
pub mod origin;
pub mod ports;
pub mod quote;
pub mod release;
pub mod run;
pub mod source;
pub mod symbol;
pub mod usage;
pub mod verdict;

pub use category::{Category, UnknownCategory};
pub use domain::*;
pub use error::JudgeError;
pub use freshness::Freshness;
pub use judge::{Deps, Traced, judge, judge_traced};
pub use operator::{
    DiscordOperator, DiscordUsername, MissingContact, NetworkOperator, Operator, SupportEmail,
};
pub use origin::{Asker, HistoryScope, Interface, Origin, Visibility};
pub use ports::{
    CallStore, Embedder, Extractor, InputKind, Rated, Resolver, Retriever, Synthesizer,
};
pub use run::{FailedCallId, Left, Outcome, RunRecord};
pub use source::{About, Commit, CommitHash, RepositoryUrl, SourceOffer};
pub use usage::{Bill, Charge, Cost, Stage, Tally, Tokens};
pub use verdict::{
    LabelPart, MAX_ANSWER_CHARS, MIN_ANSWER_CHARS, State, Unvalidated, Validated, Verdict,
    citation_supported, covers, misfiled_oracle_text, quote_from_another_rule, quotes_face_label,
    quotes_nothing,
};
