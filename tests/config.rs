//! The config file: what survives a restart, and what happens when it is broken.

mod common;

use common::TempDir;

use serial_tcp::cli::{DataBitsArg, ParityArg, ProtocolArg, SerialArgs, StopBitsArg};
use serial_tcp::dashboard::config::{CURRENT_VERSION, Config, PortConfig, generate_token, slug};

fn sample_port() -> PortConfig {
    PortConfig {
        id: "com8".to_owned(),
        device: "COM8".to_owned(),
        label: "GPS module".to_owned(),
        tcp_port: 4001,
        protocol: ProtocolArg::Rfc2217,
        serial: SerialArgs {
            baud: 460_800,
            data_bits: DataBitsArg::Eight,
            parity: ParityArg::None,
            stop_bits: StopBitsArg::One,
            flow_control: serial_tcp::cli::FlowControlArg::None,
        },
        expose: true,
        autostart: true,
    }
}

#[test]
fn a_saved_config_comes_back_unchanged() {
    let dir = TempDir::new("config");
    let path = dir.join("serial-tcp.json");

    let mut original = Config::new(4001, generate_token().unwrap());
    original.ports.push(sample_port());
    original.save(&path).unwrap();

    let loaded = Config::load_or_create(&path, 4001, None).unwrap();

    assert_eq!(loaded.token, original.token);
    assert_eq!(loaded.base_port, 4001);
    assert_eq!(loaded.ports.len(), 1);

    let port = &loaded.ports[0];
    assert_eq!(port.device, "COM8");
    assert_eq!(port.serial.baud, 460_800);
    assert_eq!(port.protocol, ProtocolArg::Rfc2217);
    assert_eq!(port.stop_bits_value(), 1);
    assert!(port.expose);
    assert!(port.autostart);
}

/// Counts belong in JSON as numbers. Deriving would have written "Eight".
#[test]
fn line_settings_are_written_as_numbers_and_lowercase_names() {
    let dir = TempDir::new("config-shape");
    let path = dir.join("serial-tcp.json");

    let mut config = Config::new(4001, "t".to_owned());
    config.ports.push(sample_port());
    config.save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"data_bits\": 8"), "got: {text}");
    assert!(text.contains("\"stop_bits\": 1"), "got: {text}");
    assert!(text.contains("\"parity\": \"none\""), "got: {text}");
    assert!(text.contains("\"protocol\": \"rfc2217\""), "got: {text}");
    // Line settings sit alongside the rest, not nested under "serial".
    assert!(!text.contains("\"serial\""), "got: {text}");
}

#[test]
fn a_missing_config_is_created_with_a_fresh_token() {
    let dir = TempDir::new("config-new");
    let path = dir.join("serial-tcp.json");

    let config = Config::load_or_create(&path, 4100, None).unwrap();

    assert_eq!(config.version, CURRENT_VERSION);
    assert_eq!(config.base_port, 4100);
    assert!(config.ports.is_empty());
    assert_eq!(config.token.len(), 64, "32 bytes, hex encoded");
    assert!(config.token.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn two_generated_tokens_differ() {
    assert_ne!(generate_token().unwrap(), generate_token().unwrap());
}

/// Refusing to boot over a bad config file would lock the user out of the very
/// screen they need to fix it.
#[test]
fn a_corrupt_config_is_moved_aside_rather_than_fatal() {
    let dir = TempDir::new("config-corrupt");
    let path = dir.join("serial-tcp.json");
    std::fs::write(&path, "{ this is not json").unwrap();

    let config = Config::load_or_create(&path, 4001, None).unwrap();
    assert!(config.ports.is_empty());
    assert!(!config.token.is_empty());

    let backup = dir.join("serial-tcp.json.bak");
    assert!(backup.exists(), "the unreadable file should be kept");
    assert_eq!(
        std::fs::read_to_string(backup).unwrap(),
        "{ this is not json"
    );
}

#[test]
fn a_config_from_a_newer_version_is_not_guessed_at() {
    let dir = TempDir::new("config-future");
    let path = dir.join("serial-tcp.json");
    std::fs::write(
        &path,
        r#"{"version": 99, "token": "abc", "base_port": 4001, "ports": []}"#,
    )
    .unwrap();

    let config = Config::load_or_create(&path, 4001, None).unwrap();
    assert_eq!(config.version, CURRENT_VERSION);
    assert_ne!(config.token, "abc", "a fresh config means a fresh token");
    assert!(dir.join("serial-tcp.json.bak").exists());
}

#[test]
fn an_explicit_token_wins_over_the_stored_one() {
    let dir = TempDir::new("config-token");
    let path = dir.join("serial-tcp.json");

    Config::new(4001, "stored".to_owned()).save(&path).unwrap();

    let config = Config::load_or_create(&path, 4001, Some("chosen".to_owned())).unwrap();
    assert_eq!(config.token, "chosen");
}

/// A partially written file would lose every paired port, so saving must be
/// all-or-nothing and must not leave litter behind.
#[test]
fn saving_leaves_no_temporary_file() {
    let dir = TempDir::new("config-atomic");
    let path = dir.join("serial-tcp.json");

    let mut config = Config::new(4001, "t".to_owned());
    config.ports.push(sample_port());
    config.save(&path).unwrap();
    config.save(&path).unwrap(); // overwriting must work too

    assert!(path.exists());
    assert!(!dir.join("serial-tcp.json.tmp").exists());

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 1, "unexpected leftovers: {entries:?}");
}

#[test]
fn device_names_become_usable_ids() {
    assert_eq!(slug("COM8"), "com8");
    assert_eq!(slug("/dev/cu.usbserial-1410"), "dev-cu-usbserial-1410");
    assert_eq!(slug("/dev/ttyUSB0"), "dev-ttyusb0");
    assert_eq!(slug("///"), "port");
}

/// Small helper so the assertions above read as intent rather than plumbing.
trait StopBitsValue {
    fn stop_bits_value(&self) -> u8;
}

impl StopBitsValue for PortConfig {
    fn stop_bits_value(&self) -> u8 {
        self.serial.stop_bits.as_u8()
    }
}
