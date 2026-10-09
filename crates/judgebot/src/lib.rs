//! The `judgebot` binary's launch model, as a library: the [`roles`] a
//! process can run, what each requires, and the role list
//! `docker-compose.yml` passes. The binary is `src/main.rs`; the library
//! exists so `judge-config` checks a draft `.env` with the same parser the
//! binary runs rather than a copy of it.

pub mod roles;
