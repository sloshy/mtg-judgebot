//! `Embedder` adapters shared by `bot` and `ingest` (a bin crate cannot be a dependency).

pub mod voyage;

pub use voyage::VoyageEmbedder;
