//! `Embedder` adapters for `judge-bot`'s retrieval and its ingest steps
//! (`judge_bot::ingest`): Voyage AI and any OpenAI-compatible `/v1/embeddings` server,
//! plus [`Space`], the identity of the vectors an embedder produces.
//!
//! Vectors from two models cannot share a column: cosine distance between a
//! `voyage-3.5` vector and a `nomic-embed-text` vector is noise, and pgvector's
//! HNSW index needs one fixed width. So every embedder here also says which
//! space it writes into ([`WithSpace`]), the database records the space its
//! stored vectors belong to (`embedding_space`, one row), and the readers and
//! writers compare the two before they touch a vector column
//! (`judge_bot::db::Vectors`, `ingest embed`). The comparison itself is
//! [`Space::check`], a pure function, so it is the same test everywhere.
//!
//! Every request is also behind the spend cap ([`metered`]): the adapters
//! implement [`EmbedBackend`], and only [`MeteredEmbedder`] — which reserves
//! a worst case on the process's `judge_llm::SpendMeter` before sending and
//! settles to the reported usage after — implements the sealed [`WithSpace`]
//! the database adapters take.

pub mod metered;
pub mod openai;
pub mod space;
pub mod voyage;

pub use metered::{
    EmbedBackend, EmbedError, EmbedPrice, EmbedUsage, Embedded, MeteredEmbedder, VOYAGE_PRICES,
    is_spend_cap, table_price, worst_case_tokens,
};
pub use openai::{Auth, OpenAiEmbedder};
pub use space::{Provider, Space, SpaceMismatch, WithSpace};
pub use voyage::VoyageEmbedder;
