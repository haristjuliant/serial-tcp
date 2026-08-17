use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "serial-tcp",
    version,
    about = "Bridge a serial port over TCP. Works on macOS, Windows and Linux."
)]
pub struct Cli {
    /// Enable debug logging.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List the serial ports available on this machine.
    List(ListArgs),

    /// Share a local serial port over TCP.
    Serve(ServeArgs),

    /// Connect to a remote `serve` and expose it locally.
    Connect(ConnectArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// On macOS, show both the callout (/dev/cu.*) and dial-in (/dev/tty.*)
    /// node for each device instead of just the callout one.
    #[arg(long)]
    pub all: bool,
}

/// Serial line settings, shared by `serve` and `connect --port`.
#[derive(Args, Debug, Clone)]
pub struct SerialArgs {
    /// Baud rate.
    #[arg(short, long, default_value_t = 115_200)]
    pub baud: u32,

    /// Data bits per character.
    #[arg(long, value_name = "BITS", default_value = "8")]
    pub data_bits: DataBitsArg,

    /// Parity checking mode.
    #[arg(long, default_value = "none")]
    pub parity: ParityArg,

    /// Stop bits.
    #[arg(long, value_name = "BITS", default_value = "1")]
    pub stop_bits: StopBitsArg,

    /// Flow control mode.
    #[arg(long, default_value = "none")]
    pub flow_control: FlowControlArg,
}

/// Mirrors the defaults declared above, for callers that construct these
/// directly rather than through clap.
impl Default for SerialArgs {
    fn default() -> Self {
        Self {
            baud: 115_200,
            data_bits: DataBitsArg::Eight,
            parity: ParityArg::None,
            stop_bits: StopBitsArg::One,
            flow_control: FlowControlArg::None,
        }
    }
}

#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("device").required(true).args(["port", "fake"])
))]
pub struct ServeArgs {
    /// Serial port to share, e.g. /dev/tty.usbserial-1410, /dev/ttyUSB0, COM3.
    #[arg(short, long)]
    pub port: Option<String>,

    /// Instead of a real device, create a local pseudo-terminal and serve that.
    /// The path of the other end is printed so you can drive it by hand.
    /// Useful for testing without hardware. Unix only.
    #[arg(long)]
    pub fake: bool,

    /// Address to listen on. Defaults to loopback: sharing a physical device
    /// with the whole network should be a deliberate choice.
    #[arg(long, default_value = "127.0.0.1:4000", value_name = "ADDR")]
    pub bind: String,

    /// Wire protocol. `rfc2217` additionally carries line settings and the
    /// modem control lines, and interoperates with ser2net and pyserial's
    /// `rfc2217://` URLs.
    #[arg(long, default_value = "raw")]
    pub protocol: ProtocolArg,

    #[command(flatten)]
    pub serial: SerialArgs,
}

#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("target").required(true).args(["stdio", "pty", "port"])
))]
pub struct ConnectArgs {
    /// Address of the remote `serve`, e.g. 192.168.1.10:4000.
    #[arg(long, value_name = "ADDR")]
    pub to: String,

    /// Pipe the remote port to this terminal's stdin/stdout.
    #[arg(long)]
    pub stdio: bool,

    /// Create a local pseudo-terminal and print its path, so other programs can
    /// open the remote port as if it were local. Unix only; on Windows pair
    /// this tool with com0com and use --port instead.
    #[arg(long)]
    pub pty: bool,

    /// Bridge to an existing local serial port. On Windows this is how you
    /// attach to one half of a com0com pair, e.g. --port COM10.
    #[arg(long)]
    pub port: Option<String>,

    /// Wire protocol. Must match what the server is running.
    #[arg(long, default_value = "raw")]
    pub protocol: ProtocolArg,

    #[command(flatten)]
    pub serial: SerialArgs,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolArg {
    /// A plain byte pipe. Nothing but data crosses the link.
    Raw,
    /// Telnet Com Port Control Option, RFC 2217.
    #[value(name = "rfc2217")]
    Rfc2217,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum DataBitsArg {
    #[value(name = "5")]
    Five,
    #[value(name = "6")]
    Six,
    #[value(name = "7")]
    Seven,
    #[value(name = "8")]
    Eight,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ParityArg {
    None,
    Odd,
    Even,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum StopBitsArg {
    #[value(name = "1")]
    One,
    #[value(name = "2")]
    Two,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum FlowControlArg {
    None,
    Software,
    Hardware,
}

impl From<DataBitsArg> for serialport::DataBits {
    fn from(v: DataBitsArg) -> Self {
        match v {
            DataBitsArg::Five => Self::Five,
            DataBitsArg::Six => Self::Six,
            DataBitsArg::Seven => Self::Seven,
            DataBitsArg::Eight => Self::Eight,
        }
    }
}

impl From<ParityArg> for serialport::Parity {
    fn from(v: ParityArg) -> Self {
        match v {
            ParityArg::None => Self::None,
            ParityArg::Odd => Self::Odd,
            ParityArg::Even => Self::Even,
        }
    }
}

impl From<StopBitsArg> for serialport::StopBits {
    fn from(v: StopBitsArg) -> Self {
        match v {
            StopBitsArg::One => Self::One,
            StopBitsArg::Two => Self::Two,
        }
    }
}

impl From<FlowControlArg> for serialport::FlowControl {
    fn from(v: FlowControlArg) -> Self {
        match v {
            FlowControlArg::None => Self::None,
            FlowControlArg::Software => Self::Software,
            FlowControlArg::Hardware => Self::Hardware,
        }
    }
}
