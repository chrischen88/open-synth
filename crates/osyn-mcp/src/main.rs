//! `osyn-mcp`: an MCP server (stdio) for designing Serum 2 presets.
//!
//! Register with Claude Code:
//! `claude mcp add open-synth -- /path/to/target/release/osyn-mcp`

mod content;
mod host;
mod server;

use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = server::SynthServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
