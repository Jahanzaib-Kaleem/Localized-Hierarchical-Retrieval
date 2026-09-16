use clap::Parser;
use lhr::{ServiceConfig, ServiceRole};
use std::{io, net::SocketAddr, path::PathBuf};

#[path = "lhr_appliance/mod.rs"]
mod lhr_appliance;

#[derive(Debug, Parser)]
#[command(name = "lhr-appliance", about = "Run LHR Studio/API and the MCP operator control plane")]
struct Args {
    #[arg(long, default_value = "/data")]
    root: PathBuf,
    #[arg(long, default_value = "/tmp/lhr-service.json")]
    config: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8788")]
    mcp_bind: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    let config = ServiceConfig::from_json_file(&args.config)?;
    let mcp_bind: SocketAddr = args
        .mcp_bind
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid MCP bind: {e}")))?;

    if !mcp_bind.ip().is_loopback() && (!config.behind_tls_proxy || config.api_keys.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote MCP listeners require protected transport and at least one API key",
        ));
    }

    // Keep one OS process and one Tokio runtime. The existing LHR HTTP/Studio server retains its
    // production service contract on 8787 while MCP is an isolated stateless HTTP listener.
    let mcp = lhr_appliance::McpServer::new(args.root.clone(), config.clone(), mcp_bind)?;
    tokio::try_join!(lhr::serve(args.root, config), mcp.serve())?;
    Ok(())
}

#[allow(dead_code)]
fn _role_order_is_part_of_the_appliance_contract() {
    debug_assert!(ServiceRole::Admin > ServiceRole::Write);
    debug_assert!(ServiceRole::Write > ServiceRole::Read);
}
