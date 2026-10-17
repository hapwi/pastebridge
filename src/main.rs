use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;

use pastebridge::config::Paths;
use pastebridge::identity::{Identity, PeerList};
use pastebridge::{daemon, doctor, pairing, service, ui, update, Config};

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
        /// Connect to host:port instead of auto-discovery
        #[arg(long)]
        connect: Option<String>,
    },
    /// Show whether the daemon is running and who it is paired with
    Status,
    /// Show saved computers and their connection status
    #[command(name = "devices", visible_alias = "list")]
    List {
        /// Print device details as JSON
        #[arg(long, conflicts_with = "quiet")]
        json: bool,
        /// Print only device IDs, one per line
        #[arg(long)]
        quiet: bool,
    },
    /// Choose a computer to forget, or specify its name or device ID
    Unpair {
        device: Option<String>,
        /// Forget the specified computer without asking
        #[arg(short, long)]
        yes: bool,
    },
    /// Check clipboard, ports, and pairing
    Doctor,
    /// Download a newer Pastebridge if one is out
    Update {
        /// Update without asking
        #[arg(short, long)]
        yes: bool,
    },
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
        None => dashboard(&cfg, &paths, &identity).await,
        Some(Command::Start) => daemon::run(cfg, paths, identity).await,
        Some(Command::Pair { connect }) => pairing::run(&cfg, &paths, &identity, connect).await,
        Some(Command::Status) => print_status(&paths, &identity),
        Some(Command::List { json, quiet }) => print_list(&paths, json, quiet),
        Some(Command::Unpair { device, yes }) => unpair(&paths, device.as_deref(), yes),
        Some(Command::Doctor) => doctor::run(&cfg, &paths, &identity),
        Some(Command::Update { yes }) => update::run(yes, &paths),
        Some(Command::InstallService) => service::install(),
        Some(Command::UninstallService) => service::uninstall(),
    }
}

async fn dashboard(cfg: &Config, paths: &Paths, identity: &Identity) -> Result<()> {
    loop {
        print_status(paths, identity)?;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            ui::hint("Pair: pastebridge pair   Devices: pastebridge devices   Forget: pastebridge unpair");
            println!();
            return Ok(());
        }
        let options = [
            "Pair another computer",
            "Forget a computer",
            "Check for updates",
            "Diagnose a connection",
        ]
        .map(String::from);
        match ui::select("Choose an action ›", &options)? {
            Some(0) => pairing::run(cfg, paths, identity, None).await?,
            Some(1) => unpair(paths, None, false)?,
            Some(2) => return update::run(false, paths),
            Some(3) => doctor::run(cfg, paths, identity)?,
            _ => return Ok(()),
        }
    }
}

fn print_status(paths: &Paths, identity: &Identity) -> Result<()> {
    ui::heading("Overview");
    println!(
        "  {}  {}",
        ui::accent(&identity.name),
        ui::muted("this computer")
    );
    if daemon::running_pid(paths).is_none() {
        ui::hint("Service stopped · run pastebridge install-service");
    } else if let Some(status) = service::ready_status(paths, env!("CARGO_PKG_VERSION")) {
        ui::success("Clipboard service ready");
        if let Some(err) = status.last_error {
            ui::hint(format!("Last connection error: {err}"));
        }
    } else {
        ui::hint("Service starting or unhealthy · run pastebridge doctor");
    }
    println!();
    print_devices(paths)?;
    Ok(())
}

fn print_devices(paths: &Paths) -> Result<()> {
    let peers = PeerList::load(paths)?;
    ui::hint(format!("COMPUTERS · {} saved", peers.peers.len()));
    if peers.peers.is_empty() {
        println!("  No paired computers yet.");
        ui::hint("Run pastebridge pair on both computers to connect.");
        println!();
        return Ok(());
    }
    let status = service::ready_status(paths, env!("CARGO_PKG_VERSION"));
    let mut waiting = false;
    for peer in peers.peers {
        let connected = status.as_ref().is_some_and(|status| {
            status
                .peers
                .iter()
                .any(|p| p.device_id == peer.device_id && p.connected)
        });
        waiting |= !connected;
        println!("  {}  {}", ui::state(connected), peer.name);
        ui::hint(format!("    ID {}", &peer.device_id[..8]));
    }
    println!();
    if waiting {
        ui::hint("Waiting computers reconnect automatically when available.");
        ui::hint("Reinstalled a computer? Run pastebridge pair again on both.");
    }
    Ok(())
}

fn print_list(paths: &Paths, json: bool, quiet: bool) -> Result<()> {
    let peers = PeerList::load(paths)?;
    if quiet {
        for peer in peers.peers {
            println!("{}", peer.device_id);
        }
        return Ok(());
    }
    if json {
        let status = service::ready_status(paths, env!("CARGO_PKG_VERSION"));
        let devices: Vec<_> = peers
            .peers
            .iter()
            .map(|peer| {
                let connected = status.as_ref().is_some_and(|status| {
                    status
                        .peers
                        .iter()
                        .any(|p| p.device_id == peer.device_id && p.connected)
                });
                serde_json::json!({
                    "device_id": peer.device_id,
                    "name": peer.name,
                    "connected": connected,
                    "last_addr": peer.last_addr,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }
    ui::heading("Computers");
    print_devices(paths)?;
    ui::hint("Add: pastebridge pair   Forget: pastebridge unpair");
    println!();
    Ok(())
}

fn unpair(paths: &Paths, device: Option<&str>, yes: bool) -> Result<()> {
    let peers = PeerList::load(paths)?;
    ui::heading("Forget a computer");
    if peers.peers.is_empty() {
        ui::hint("No paired computers to forget.");
        println!();
        return Ok(());
    }
    let peer = if let Some(device) = device {
        let matches: Vec<_> = peers
            .peers
            .iter()
            .filter(|peer| {
                peer.device_id.eq_ignore_ascii_case(device)
                    || peer.name.eq_ignore_ascii_case(device)
                    || (device.len() >= 4
                        && peer.device_id.starts_with(&device.to_ascii_lowercase()))
            })
            .collect();
        match matches.as_slice() {
            [peer] => *peer,
            [] => anyhow::bail!("no computer matches {device:?}; run pastebridge devices"),
            _ => anyhow::bail!("more than one computer matches {device:?}; use its device ID from pastebridge devices"),
        }
    } else {
        if !std::io::stdin().is_terminal() || yes {
            anyhow::bail!(
                "choose a computer in a terminal, or use pastebridge unpair <name-or-id> --yes"
            );
        }
        let options: Vec<_> = peers
            .peers
            .iter()
            .map(|peer| format!("{}  {}", peer.name, ui::muted(&peer.device_id[..8])))
            .collect();
        let Some(index) = ui::select("Forget which computer? ›", &options)? else {
            return Ok(());
        };
        &peers.peers[index]
    };
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("confirmation requires a terminal; pass --yes to forget this computer");
        }
        println!(
            "  {}  {}",
            ui::accent(&peer.name),
            ui::muted(&peer.device_id[..8])
        );
        ui::hint("Clipboard sharing with this computer will stop.");
        if !ui::confirm("Forget this computer?") {
            ui::hint("Cancelled · your pairings are unchanged.");
            println!();
            return Ok(());
        }
    }
    let mut current = PeerList::load(paths)?;
    if current.remove(&peer.device_id) {
        current.save(paths)?;
        ui::success(format!("Forgot {} on this computer.", peer.name));
        ui::hint("Its saved entry on the other computer is unchanged.");
        ui::hint("To connect again, run pastebridge pair on both computers.");
        println!(
            "  {}",
            ui::muted("Active clipboard sharing will stop within a few seconds.")
        );
    }
    println!();
    Ok(())
}
