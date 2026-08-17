//! `serial-tcp list` — show what serial ports this machine has.

use anyhow::{Context, Result};
use serialport::{SerialPortInfo, SerialPortType};

use crate::cli::ListArgs;

pub fn run(args: ListArgs) -> Result<()> {
    let mut ports = serialport::available_ports().context("failed to enumerate serial ports")?;

    if !args.all {
        ports = prefer_callout(ports);
    }

    if ports.is_empty() {
        println!("No serial ports found.");
        return Ok(());
    }

    for port in &ports {
        println!("{}", port.port_name);
        for line in describe(&port.port_type) {
            println!("    {line}");
        }
    }

    if cfg!(target_os = "macos") && !args.all {
        println!();
        println!("Showing callout (/dev/cu.*) nodes only; pass --all to also see /dev/tty.*");
    }

    Ok(())
}

/// macOS exposes every device twice: a callout node (`/dev/cu.*`) and a dial-in
/// node (`/dev/tty.*`). Callout is the one you want for talking to a device —
/// opening the dial-in node blocks waiting for carrier detect — so hide the
/// dial-in twin when both are present.
fn prefer_callout(ports: Vec<SerialPortInfo>) -> Vec<SerialPortInfo> {
    let callouts: Vec<String> = ports
        .iter()
        .filter_map(|p| p.port_name.strip_prefix("/dev/cu.").map(str::to_owned))
        .collect();

    ports
        .into_iter()
        .filter(|p| match p.port_name.strip_prefix("/dev/tty.") {
            Some(stem) => !callouts.iter().any(|c| c == stem),
            None => true,
        })
        .collect()
}

fn describe(port_type: &SerialPortType) -> Vec<String> {
    match port_type {
        SerialPortType::UsbPort(info) => {
            let mut lines = vec![format!("USB {:04x}:{:04x}", info.vid, info.pid)];
            if let Some(m) = &info.manufacturer {
                lines.push(format!("manufacturer: {m}"));
            }
            if let Some(p) = &info.product {
                lines.push(format!("product: {p}"));
            }
            if let Some(s) = &info.serial_number {
                lines.push(format!("serial: {s}"));
            }
            lines
        }
        SerialPortType::BluetoothPort => vec!["Bluetooth".to_owned()],
        // macOS reports plenty of ports as PCI that plainly are not, so this
        // classification carries no signal worth showing. The name identifies
        // the port well enough on its own.
        SerialPortType::PciPort | SerialPortType::Unknown => Vec::new(),
    }
}
