//! `judge-api` — the anonymous HTTP adapter (`judgebot`'s network roles,
//! [`network`]): axum routes over the same
//! `judge()` composition the Discord bot uses, plus static serving of the
//! `SolidJS` web client (`web/dist`).
//!
//! Differences from the Discord adapter, by design:
//! * callers are anonymous, so there are **no rating endpoints** — ratings
//!   need an accountable identity (Discord user + judge role);
//! * the "did you mean…?" flow is stateless: ambiguity is returned as data
//!   and the client re-asks with `pins` (span → full card name), which the
//!   server rewrites into `[[Full Name]]` via the same `pin_card` the Discord
//!   buttons use — no server-side pending store;
//! * follow-up context comes from a client-generated `session_id` (kept in
//!   `sessionStorage`), namespaced as thread id `web:<uuid>`;
//! * anonymous traffic is cost: a per-IP fixed-window limiter ([`limit`])
//!   sits in front of the concurrency semaphore, and the spend-capped
//!   Anthropic client remains the backstop;
//! * with `--mcp` and `MCP_TOKEN` set, the MCP transport of `judge-agent` is
//!   mounted at `/mcp` behind that bearer token ([`mcp`]), sharing the judge
//!   slots and the spend cap with the web route.
//!
//! Which of those interfaces a process opens is a launch option, not a
//! consequence of being started: see [`interfaces`] and `judgebot`'s roles.
//! The web page is served only where an operator asked for it.

pub mod config;
pub mod http;
pub mod interfaces;
pub mod limit;
pub mod mcp;
pub mod network;
pub mod shape;

pub use config::{ApiConfig, Refused, Unmet};
pub use http::{App, DataStatus, bind, router, serve_on};
pub use interfaces::{Interface, Interfaces};
pub use network::{Listening, Network};
