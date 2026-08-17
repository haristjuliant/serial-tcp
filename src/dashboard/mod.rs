//! `serial-tcp dashboard` — supervise any number of serial ports, from a browser.
//!
//! The dashboard itself holds port 4000 and hands out 4001, 4002, … to the
//! ports paired through it. Each of those is an ordinary `serve` endpoint, so
//! anything that already speaks to this tool — `serial-tcp connect`, pyserial's
//! `rfc2217://` URLs, ser2net clients — connects to them unchanged.
//!
//! There are two separate gates. The **token** guards configuration: pairing a
//! device, changing a baud rate, sending bytes. The **allowlist** guards
//! everything by address, and it is the only control the data ports can have at
//! all — those speak raw bytes or RFC 2217 and cannot be asked for a password
//! without breaking every standard client that needs to reach them.

pub mod api;
pub mod config;
pub mod http;
pub mod net;
pub mod registry;
pub mod supervisor;
pub mod tap;

use anyhow::{Context, Result};

use crate::cli::DashboardArgs;
use crate::dashboard::config::{Config, Overrides};
use crate::dashboard::http::Assets;
use crate::dashboard::net::Allowlist;
use crate::dashboard::registry::{Registry, real_devices};

pub fn run(args: DashboardArgs) -> Result<()> {
    let allow = if args.allow.is_empty() {
        None
    } else {
        Some(Allowlist::parse(&args.allow).context("could not understand an --allow value")?)
    };

    let overrides = Overrides {
        base_port: args.base_port,
        token: args.token,
        no_token: args.no_token,
        allow,
    };

    let config = Config::load_or_create(&args.config, &overrides)?;
    // Write straight back, so a token generated just now survives a restart.
    config.save(&args.config)?;

    // Claim the port before opening any hardware: failing here is the common
    // mistake (a second dashboard already running) and it should not leave a
    // device held open on the way out.
    let server = http::bind(&args.bind)?;
    let bound = server
        .server_addr()
        .to_ip()
        .map_or_else(|| args.bind.clone(), |addr| addr.to_string());

    let registry = Registry::new(&config, args.config.clone(), real_devices());
    registry.autostart_all();

    announce(&bound, &config, &args.config.display().to_string());

    http::serve(server, registry, Assets::new(args.assets_dir))
}

/// The token is useless if the user cannot find it, so print a URL they can
/// click rather than making them go and read the config file.
fn announce(bound: &str, config: &Config, config_path: &str) {
    let exposed = bound.starts_with("0.0.0.0") || bound.starts_with("[::]");
    let reachable = bound
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "[::1]");
    let allow = config.allowlist();

    println!("dashboard listening on http://{bound}");
    if config.require_token {
        println!("open  http://{reachable}/?token={}", config.token);
    } else {
        println!("open  http://{reachable}/");
    }
    println!("config  {config_path}");
    println!("access  {}", describe_access(config, &allow));

    if !exposed {
        return;
    }

    if let Some(ip) = net::primary_local_ip() {
        let suffix = if config.require_token {
            format!("/?token={}", config.token)
        } else {
            "/".to_owned()
        };
        let port = bound.rsplit(':').next().unwrap_or("4000");
        println!("from another machine: http://{ip}:{port}{suffix}");
    }

    // The dangerous combination is worth more than a log line: reachable from
    // the network, no token, and no restriction on who may connect.
    if !config.require_token && allow.is_empty() {
        eprintln!();
        eprintln!(
            "WARNING: this dashboard is on the network with no token and no address \
             restriction."
        );
        eprintln!("         Anyone who can reach it can reconfigure every device attached to it.");
        if let Some(subnet) = net::primary_local_ip().and_then(net::suggest_subnet) {
            eprintln!("         Consider --allow {subnet}");
        }
        eprintln!();
    } else if allow.is_empty() {
        log::warn!(
            "the dashboard is reachable from the whole network; --allow narrows that to \
             the addresses you name"
        );
    }
}

fn describe_access(config: &Config, allow: &Allowlist) -> String {
    let gate = if config.require_token {
        "token required"
    } else {
        "NO TOKEN"
    };
    format!("{gate}, from {}", allow.describe())
}
