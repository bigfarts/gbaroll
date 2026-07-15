use std::net::SocketAddr;

use clap::Parser;
use gbaroll_signaling::IceServer;

#[derive(Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:1984")]
    listen: SocketAddr,
    /// STUN server URL handed to clients (repeatable). Defaults to a
    /// public STUN list when neither --stun nor --turn is given.
    #[arg(long = "stun")]
    stun: Vec<String>,
    /// TURN server handed to clients, as `url,username,credential`
    /// (repeatable).
    #[arg(long = "turn")]
    turn: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let mut ice_servers = Vec::new();
    if !args.stun.is_empty() {
        ice_servers.push(IceServer {
            urls: args.stun.clone(),
            username: None,
            credential: None,
        });
    }
    for turn in &args.turn {
        let mut parts = turn.splitn(3, ',');
        let (Some(url), Some(username), Some(credential)) = (parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("--turn takes url,username,credential");
        };
        ice_servers.push(IceServer {
            urls: vec![url.to_string()],
            username: Some(username.to_string()),
            credential: Some(credential.to_string()),
        });
    }
    if ice_servers.is_empty() {
        ice_servers = gbaroll_signaling_server::default_ice_servers();
    }

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    gbaroll_signaling_server::serve(listener, ice_servers).await
}
