# serial-tcp

Share a serial port over TCP. One tool, same commands, on macOS, Windows and
Linux — so a physical device plugged into one machine can be read from
another.

## How it works

The whole tool is one primitive — pump bytes between two endpoints — wired up
two ways:

```
serve:    Serial port  <──bridge──>  TCP listener
connect:  TCP socket   <──bridge──>  stdio | pseudo-terminal | local serial port
```

`serve` runs on the machine with the physical device. `connect` runs on every
other machine that wants to use it. That symmetry is why Windows needs no
special case anywhere in the code: one half of a com0com pair is just a serial
port like any other, so it goes through the exact same `--port` path as real
hardware.

## Build

Natively, on whichever OS you're building for:

```sh
cargo build --release
```

The binary lands at `target/release/serial-tcp` (`serial-tcp.exe` on Windows).
It has no runtime dependencies beyond what the OS already provides.

### Cross-compiling a Windows .exe from macOS or Linux

Native Rust cross-compilation only produces a linkable binary for Windows if a
Windows linker and the Windows SDK/CRT are available, which macOS and Linux
don't have out of the box. [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin)
supplies both — it downloads a copy of the SDK/CRT on first use and links
against that.

```sh
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
cargo xwin build --release --target x86_64-pc-windows-msvc
```

Output: `target/x86_64-pc-windows-msvc/release/serial-tcp.exe`. Copy that one
file to the Windows machine — nothing else needs to be installed there, not
even Rust. The first build downloads the SDK (a few hundred MB); later builds
reuse the cached copy.

`cargo check --target x86_64-pc-windows-msvc` (no `xwin`) is enough to
type-check Windows-specific code paths without linking, and needs nothing extra
installed.

### Linux static builds

Port enumeration on Linux uses libudev, a C library needed at link time. Drop
it for a static musl binary:

```sh
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
```

`serve` and `connect` are unaffected; only `list` stops finding ports.

## Quick start — no hardware needed

`--fake` creates a pseudo-terminal and serves that instead of a real device,
so you can exercise the whole path with nothing plugged in. Three terminals:

```sh
# terminal 1
serial-tcp serve --fake
# -> fake device ready at /dev/ttys001

# terminal 2
serial-tcp connect --to 127.0.0.1:4000 --stdio

# terminal 3 — stands in for the hardware
cat /dev/ttys001                    # see what terminal 2 sends
printf 'hello\n' > /dev/ttys001     # send something to terminal 2
```

Text typed in terminal 2 shows up in terminal 3's `cat`, and vice versa. The
test suite uses the same trick, so `cargo test` needs no hardware either.

## Using a real device

Find it, then share it:

```sh
serial-tcp list
serial-tcp serve --port /dev/cu.usbserial-1410 --baud 115200
```

On macOS, use the `cu.*` node, not `tty.*` — `list` hides the `tty.*` twin of
each device by default (`--all` shows both), because opening `tty.*` blocks
waiting for carrier detect.

This listens on `127.0.0.1:4000` only. From another program on the *same*
machine:

```sh
serial-tcp connect --to 127.0.0.1:4000 --stdio
```

## Reading a device across machines

This is the common case: the hardware is plugged into one computer, and you
want to read it from another. It works the same way regardless of which OS is
on which side — including Windows on one end and macOS or Linux on the other.

**On the machine with the hardware** (say it's Windows, device `COM3`):

```
serial-tcp.exe serve --port COM3 --baud 115200 --bind 0.0.0.0:4000
```

`--bind 0.0.0.0:4000` is required to accept connections from other machines —
the default `127.0.0.1` deliberately only accepts local ones. Allow inbound
TCP port 4000 through the Windows Firewall, then find this machine's LAN
address with `ipconfig`.

**On the reading machine** (say it's macOS):

```sh
serial-tcp connect --to 192.168.1.10:4000 --stdio
```

Swap `serve`/`connect` and the platform-specific pieces (`--port COM3` vs.
`--port /dev/cu.usbserial-1410`, `.exe` vs. no extension) and the same two
commands work for any direction: Windows↔macOS, Linux↔Windows, macOS↔Linux, or
any OS talking to itself.

⚠️ The link is **unauthenticated and unencrypted**. Anyone who can reach that
TCP port controls the device. If the network between the two machines isn't
trusted, don't expose it directly — tunnel over SSH instead and connect to
`127.0.0.1` on both ends:

```sh
ssh -L 4000:127.0.0.1:4000 user@windows-machine
serial-tcp connect --to 127.0.0.1:4000 --stdio
```

## Giving another program a real serial port

`--stdio` is fine for looking at raw bytes yourself, but some software will
only talk to a device node, not a socket or your terminal.

**macOS and Linux** — a pseudo-terminal is enough, no driver needed:

```sh
serial-tcp connect --to 192.168.1.10:4000 --pty
# virtual serial port ready at /dev/ttys004
```

Point the application at the printed path; it behaves like a local serial
port.

**Windows** has no pseudo-terminals, and a virtual COM port cannot be created
from user space at all — it needs a kernel driver. Use
[com0com](https://com0com.com/), which is open source and now ships a signed
build for Windows 10/11 (no test-signing mode required):

1. Install com0com and create a pair, for example `COM10` <-> `COM11`.
2. Run `serial-tcp connect --to 192.168.1.10:4000 --port COM10`.
3. Point your application at `COM11`.

If installing a driver is not acceptable in your environment, the way out is
to make the client application speak TCP (or RFC 2217, below) directly
instead of expecting a device node.

## Carrying line settings and control signals

The default `raw` protocol moves bytes and nothing else: `--baud`, `--parity`
and friends apply only to the port each process opens locally, so a client
cannot change the far end's baud rate and no modem control lines are conveyed.

`--protocol rfc2217` adds the [Telnet Com Port Control
Option](https://www.rfc-editor.org/rfc/rfc2217.html) on top of the byte
stream, which carries baud rate, character format, flow control, break, and
the DTR/RTS/CTS/DSR/CD/RI signals — and lets the client change them
mid-session.

```sh
serial-tcp serve --port /dev/cu.usbserial-1410 --protocol rfc2217
serial-tcp connect --to 192.168.1.10:4000 --protocol rfc2217 --pty
```

Both ends must agree; a raw client talking to an RFC 2217 server will see
Telnet control bytes as data.

The real payoff is that RFC 2217 is what everyone else already speaks, so no
client of ours is needed at all:

```python
import serial
port = serial.serial_for_url("rfc2217://192.168.1.10:4000", baudrate=115200)
```

pyserial is an independent implementation, which makes it a genuine
conformance check rather than a test of our own assumptions —
`tests/rfc2217.rs` covers the same ground in-process so `cargo test` catches
regressions without needing Python. The same server should work with
`ser2net` clients and commercial device servers.

### A note on `--fake` and line settings

A pseudo-terminal has no baud rate, so `tcsetattr` rejects the change and the
port keeps reporting whatever it started with. Reporting that back would be
reporting a fiction, and a conforming client treats the mismatch as a
rejection and refuses to open. With `--fake` there is no real line to
describe, so the server records what was asked for and reports that instead.
Real ports never take this path: there the server always reports what the
hardware actually did, so a rate the device cannot do is visible as such
rather than silently accepted.

## CLI reference

Every command also takes `--help` for the full, current list of flags.

```
serial-tcp list [--all]
serial-tcp serve (--port <PORT> | --fake) [--bind <ADDR>] [--protocol raw|rfc2217]
                  [--baud <N>] [--data-bits 5|6|7|8] [--parity none|odd|even]
                  [--stop-bits 1|2] [--flow-control none|software|hardware]
serial-tcp connect --to <ADDR> (--stdio | --pty | --port <PORT>) [--protocol raw|rfc2217]
                    [--baud <N>] [--data-bits 5|6|7|8] [--parity none|odd|even]
                    [--stop-bits 1|2] [--flow-control none|software|hardware]
```

`-v` / `--verbose` (before the subcommand) turns on debug logging — useful for
seeing how many bytes moved in each direction and why a session ended.

## Notes on behaviour

- `TCP_NODELAY` is set. Nagle's algorithm would coalesce small writes and
  smear the inter-frame gaps that protocols like Modbus RTU depend on.
- Bytes are forwarded as soon as they arrive rather than batched into full
  buffers, for the same reason.
- Only one client is bridged at a time. Two writers on one serial line
  interleave into garbage.
- Data the device sent while no client was connected is discarded when a
  client arrives, rather than delivered as a corrupt partial frame.

## Testing

```sh
cargo test
```

18 tests, none of which need real hardware — pseudo-terminals stand in for
the physical device, the same trick `--fake` uses.

## Not implemented yet

- No authentication or encryption (see the warning above).
- UART line status (parity/framing/overrun errors, break detection) is not
  reported to RFC 2217 clients; `SET-LINESTATE-MASK` is accepted but nothing
  is ever notified.
- Mark and space parity, and 1.5 stop bits, exist in RFC 2217 but not in the
  underlying crate's API, so they are declined rather than applied.
- The client does not reconnect if the link drops.
