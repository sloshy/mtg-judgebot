//! MTG Judge Bot — domain core.
//!
//! This crate holds the language-neutral domain model from
//! `docs/ARCHITECTURE.md` §5, the port traits, citation validation and the
//! `judge()` pipeline. It deliberately has **no** tokio/reqwest/sqlx
//! dependency: any I/O here is a build failure by dependency graph.

pub mod category;
pub mod domain;
pub mod error;
pub mod judge;
pub mod operator;
pub mod ports;
pub mod quote;
pub mod source;
pub mod symbol;
pub mod verdict;

pub use category::{Category, UnknownCategory};
pub use domain::*;
pub use error::JudgeError;
pub use judge::{Deps, judge};
pub use operator::{
    DiscordOperator, DiscordUsername, MissingContact, NetworkOperator, Operator, SupportEmail,
};
pub use ports::{CallStore, Embedder, Extractor, InputKind, Resolver, Retriever, Synthesizer};
pub use source::{About, Commit, CommitHash, RepositoryUrl, SourceOffer};
pub use verdict::{
    MAX_ANSWER_CHARS, MIN_ANSWER_CHARS, State, Unvalidated, Validated, Verdict, citation_supported,
};
