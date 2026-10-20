//! [`Admin`]: the capability every admin operation takes (D37).
//!
//! The admin operations read what the public never sees: private calls,
//! Discord user ids, failed questions, the spend. Each takes an `&Admin`, and
//! this crate hands one out in exactly two places, so an operation reached
//! any other way does not compile:
//!
//! * [`crate::Toolbox::from_env`], which builds a toolbox that is its own
//!   process over the database (`judge-cli`, stdio `judge-mcp`) and returns
//!   the capability beside it ([`crate::Local`]): whoever runs it already
//!   holds `DATABASE_URL`, which reads all of this anyway. A toolbox inside
//!   the instance is built with [`crate::Client::instance`] through
//!   [`crate::Toolbox::new`], which hands out no capability.
//! * [`crate::mcp::admin_http_service`], which builds the `/mcp` handler
//!   holding one only from a `judge_bot::admin::McpAdminToken` (checked to
//!   differ from `MCP_TOKEN`), and returns it paired with that token, the
//!   bearer `judgebot` gates it behind. The handler `MCP_TOKEN` reaches is
//!   another type ([`crate::mcp::Public`]), which has no admin tools at all.

/// Proof that the caller may read the operator's data. Not constructible
/// outside this crate.
#[derive(Clone, Debug)]
pub struct Admin {
    _sealed: (),
}

impl Admin {
    /// The capability; only this crate's two grant points call it.
    pub(crate) const fn grant() -> Self {
        Self { _sealed: () }
    }
}
