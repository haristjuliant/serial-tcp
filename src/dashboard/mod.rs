//! `serial-tcp dashboard` — supervise any number of serial ports, from a browser.
//!
//! The dashboard itself holds port 4000 and hands out 4001, 4002, … to the
//! ports paired through it. Each of those is an ordinary `serve` endpoint, so
//! anything that already speaks to this tool — `serial-tcp connect`, pyserial's
//! `rfc2217://` URLs, ser2net clients — connects to them unchanged.
//!
//! Worth being clear about what the token does and does not cover: it guards
//! *configuration*, so only someone holding it can pair a device, change a baud
//! rate or send bytes from the send box. The data ports cannot be authenticated
//! without breaking every standard client that needs to reach them, so they stay
//! open exactly as `serve` is today. That is why a port binds to loopback unless
//! it is explicitly exposed, and why the UI badges the ones that are.

pub mod api;
pub mod config;
pub mod http;
pub mod registry;
pub mod supervisor;
pub mod tap;

use anyhow::Result;

use crate::cli::DashboardArgs;
use crate::dashboard::config::Config;
use crate::dashboard::http::Assets;
use crate::dashboard::registry::{Registry, real_devices};

pub fn run(args: DashboardArgs) -> Result<()> {
    let config = Config::load_or_create(&args.config, args.base_port, args.token)?;
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

    announce(&bound, &config.token, &args.config.display().to_string());

    http::serve(server, registry, Assets::new(args.assets_dir))
}

/// The token is useless if the user cannot find it, so print a URL they can
/// click rather than making them go and read the config file.
fn announce(bound: &str, token: &str, config_path: &str) {
    let exposed = bound.starts_with("0.0.0.0") || bound.starts_with("[::]");
    let reachable = bound
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "[::1]");

    println!("dashboard listening on http://{bound}");
    println!("open  http://{reachable}/?token={token}");
    println!("config  {config_path}");

    if exposed {
        let port = bound.rsplit(':').next().unwrap_or("4000");
        println!("from another machine: http://<this-machine's-ip>:{port}/?token={token}");
    }
}
