use std::path::PathBuf;

use clap::Parser;
use shell_daemon::ShellDaemonServer;
use tokio::net::UnixListener;

#[derive(Debug, Parser)]
#[command(name = "shell-daemon", about = "Oxydra shell daemon sidecar")]
struct Args {
    #[arg(long = "socket")]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Remove stale socket file so bind succeeds.
    if args.socket.exists() {
        std::fs::remove_file(&args.socket)?;
    }

    let listener = UnixListener::bind(&args.socket)?;
    eprintln!(
        "shell-daemon listening on {}",
        args.socket.to_string_lossy()
    );

    ShellDaemonServer::default()
        .serve_unix_listener(listener)
        .await?;

    Ok(())
}
