use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use pastebridge::config::Paths;
use pastebridge::identity::{Identity, PeerList};
use pastebridge::{daemon, doctor, pairing, service, Config};

#[derive(Parser)]
#[command(
    name = "pastebridge",
    version,
    about = "Copy on macOS, paste on Linux — and the other way around.",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the clipboard daemon (used by the login service)
    Start,
    /// Pair this computer with another one running `pastebridge pair`
    Pair {
        /// Connect to host:port instead of discovering on the LAN
        #[arg(long)]
        connect: Option<String>,
        /// Confirm pairing without prompting (the codes must still match)
        #[arg(long)]
        yes: bool,
    },
    /// Show whether the daemon is running and who it is paired with
    Status,
    /// List paired computers
    List,
    /// Forget a paired computer
    Unpair { device_id: String },
    /// Check clipboard, ports, and pairing
    Doctor,
    /// Start Pastebridge automatically when you log in
    InstallService,
    /// Remove the login service
    UninstallService,
}

#[tokio::main]
async fn main() {
    if let Err(err) = real_main().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    pastebridge::init_crypto();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("pastebridge=info,quinn=warn,rustls=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let (cfg, paths) = Config::load()?;
    let identity = Identity::load_or_create(&paths, &cfg.device_name())?;

    match cli.command {
        None => {
            print_intro(&cfg, &paths, &identity);
            Ok(())
        }
        Some(Command::Start) => daemon::run(cfg, paths, identity).await,
        Some(Command::Pair { connect, yes }) => {
            pairing::run(&cfg, &paths, &identity, connect, yes).await
        }
        Some(Command::Status) => print_status(&paths, &identity),
        Some(Command::List) => print_list(&paths),
        Some(Command::Unpair { device_id }) => unpair(&paths, &device_id),
        Some(Command::Doctor) => doctor::run(&cfg, &paths, &identity),
        Some(Command::InstallService) => service::install(),
        Some(Command::UninstallService) => service::uninstall(),
    }
}

fn print_intro(cfg: &Config, paths: &Paths, identity: &Identity) {
    println!("pastebridge {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!(
        "  This computer : {} ({})",
        identity.name, identity.device_id
    );
    println!("  Config        : {}", paths.config_dir.display());
    println!();

    let peers = PeerList::load(paths).unwrap_or_default();
    if peers.peers.is_empty() {
        println!("  Not paired yet. On both computers run:");
        println!();
        println!("      pastebridge pair");
        println!();
        println!("  Compare the codes, type y, then:");
        println!();
        println!("      pastebridge install-service");
    } else {
        println!("  Paired with:");
        for peer in &peers.peers {
            println!("      {} ({})", peer.name, peer.device_id);
        }
        println!();
        if daemon::running_pid(paths).is_some() {
            println!("  Daemon is running. Copy here, paste there.");
        } else {
            println!("  Daemon is not running. Start it with:");
            println!();
            println!("      pastebridge start");
            println!("      pastebridge install-service");
        }
    }
    println!();
    println!("  Commands: pair, start, status, doctor, install-service");
    println!("  Port {}  (UDP, QUIC)", cfg.port);
}

fn print_status(paths: &Paths, identity: &Identity) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(&paths.status_file) {
        if daemon::running_pid(paths).is_some() {
            println!("{text}");
            return Ok(());
        }
    }
    println!("daemon: stopped");
    println!("device: {} ({})", identity.name, identity.device_id);
    print_list(paths)
}

fn print_list(paths: &Paths) -> Result<()> {
    let peers = PeerList::load(paths)?;
    if peers.peers.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for peer in peers.peers {
        println!(
            "{}  {}  {}",
            peer.device_id,
            peer.name,
            peer.last_addr.unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

fn unpair(paths: &Paths, device_id: &str) -> Result<()> {
    let mut peers = PeerList::load(paths)?;
    if peers.remove(device_id) {
        peers.save(paths)?;
        println!("forgot {device_id}");
    } else {
        anyhow::bail!("no paired device named {device_id}");
    }
    Ok(())
}
