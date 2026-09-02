//! `Embedder` adapters shared by `bot` and `ingest` (a bin crate cannot be a
//! dependency): Voyage AI and any OpenAI-compatible `/v1/embeddings` server,
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

pub mod openai;
pub mod space;
pub mod voyage;

pub use openai::{Auth, OpenAiEmbedder};
pub use space::{Provider, Space, SpaceMismatch, WithSpace};
pub use voyage::VoyageEmbedder;
