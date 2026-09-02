//! `judge-mcp` — the MCP server over stdio, for a local client
//! (`claude mcp add judge -- judge-mcp`, or the repo's `.mcp.json`).
//!
//! Reads the same environment as the other binaries (`DATABASE_URL`
//! required; `ANTHROPIC_API_KEY` optional, enabling the built-in `judge`
//! tool; `VOYAGE_API_KEY` optional). Logs go to stderr because stdout is the
//! protocol stream. For the HTTP transport, see `judge-api` (`MCP_TOKEN`).

use std::sync::Arc;

use anyhow::Result;
use judge_agent::{Toolbox, mcp::serve_stdio};
use judge_bot::synth::Harness;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let toolbox = Toolbox::from_env(Harness::Mcp).await?;
    tracing::info!(pipeline = toolbox.has_pipeline(), "judge-mcp serving on stdio");
    serve_stdio(Arc::new(toolbox)).await
}
