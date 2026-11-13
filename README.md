# usb-serial-over-tailscale

[日本語](README-ja.md)

An ESP32-S3 board with PSRAM that joins a tailnet on its own and bridges the USB serial console of a target SBC to a TCP port on the tailnet.

![A laptop reaches the target board's serial console through Tailscale; the ESP32-S3 is powered from a USB charger and plugged into the target's USB port](web/overview.webp)

```text
Remote PC ──Tailscale / TCP:35930+n──▶ ESP32-S3 ──USB host──▶ serial interface n of the target SBC
```

Everything a Tailscale node needs (control protocol, WireGuard, DERP, NAT traversal) is implemented in Rust in this repository.
Initial setup is an interactive shell over USB serial; flashing and setup are done from a browser, with nothing to install.

## Status

Working end to end:

- Wi-Fi setup through the wizard and joining a tailnet with the approval URL.
- `tailscale ping` from another node, both via DERP and over a direct path.
- Console access to a Raspberry Pi Debug Probe (CDC-ACM) and a Toradex Dahlia (FT4232H).
- RFC 2217 sessions against both: baud rate and framing changes, DTR and RTS, break and purge from pyserial.
- On the FT4232H, three ports claimed at once and a client on the fourth port refused.
- Recovery after adapter reboot, Wi-Fi reconnection and USB replug.
- On a Seeed Studio XIAO ESP32S3, the console of a Raspberry Pi 5 over a 3.3 V UART instead of USB (see "Seeed Studio XIAO ESP32S3" below).

Not yet:

- Headless registration with an auth key.
- CP210x, CH340/CH341, and non-H-series FTDI chips such as the FT232R.
- Recovery after a reboot of the target SBC, an internet outage, or an unreachable control server.
- Long-running operation, behaviour after node key expiry (180 days by default), and reboots triggered by the task watchdog.
- esptool through RFC 2217 (its DTR and RTS sequence is tested on the host with `cargo run -p adapter-core --example rfc2217_sim`).

### Module variants

Flash size and PSRAM mode are build-time settings, so CI builds one image per ESP32-S3 module variant (`firmware/variants/`).
PSRAM is required: network thread stacks and TLS buffers live there.

| Variant | Flash | PSRAM | Status |
| --- | --- | --- | --- |
| N16R8 | 16 MB | 8 MB octal | supported |
| N8R8 | 8 MB | 8 MB octal | built |
| N8R2 | 8 MB | 2 MB quad | supported |
| N4, N8, N16 (no PSRAM) | | none | not supported |

N16R8, N8R8 and N8R2 are the variants found on the YD-ESP32-S3 board.
The Seeed Studio XIAO ESP32S3 (8 MB flash, 8 MB octal PSRAM) has its own image, `xiao-s3`, built with `--features board-xiao-s3`.

To build a variant locally, point esp-idf-sys at its defaults file and pass the variant to the image script:

```bash
cd firmware
ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/n8r2.defaults" cargo build --release
tools/mkimage.sh n8r2 release
```

## Layout

| Path | Role |
| --- | --- |
| `crates/tsnode` | Tailscale node: control plane (ts2021 Noise and a minimal HTTP/2), WireGuard, DERP, disco, STUN, TCP termination with smoltcp. Runs on a host and on ESP-IDF. |
| `crates/adapter-core` | Interactive setup shell. I/O and device operations are traits, so the wizard is unit-tested on the host. |
| `tools/hostnode` | Runs tsnode on Linux for verification: joins a real tailnet and serves an echo server. |
| `firmware` | ESP32-S3 firmware (esp-idf-svc): setup console, USB host (CDC-ACM, FTDI, CP210x, CH34x), Wi-Fi, Tailscale node, TCP bridge. |
| `hardware/case` | 3D-printed case for the YD-ESP32-S3: OpenSCAD source and ready-to-print STLs, with and without pin headers ([README](hardware/case/README.md)). |

tsnode is split into these modules.

| Module | Contents |
| --- | --- |
| `controlbase`, `h2`, `control` | `/key` fetch, Noise IK handshake over `POST /ts2021`, `/machine/register` and `/machine/map` over HTTP/2. `/key` is always TLS; the Noise transport tries plain :80 first and falls back to TLS. |
| `wireguard` | Noise IKpsk2 handshake (initiator and responder), transport, replay window, rekey and keepalive timers. |
| `derp` | DERP relay client (TLS, fast-start, ping/pong, NotePreferred). |
| `disco`, `stun` | ping, pong and call-me-maybe for direct paths; public endpoint discovery with STUN. |
| `magicsock` | Path selection between UDP and DERP, handshake handling, peer table. |
| `netstack` | TCP termination on the tailnet addresses with smoltcp. |
| `node` | Ties the above together in threads. Keys persist in NVS or a file. |

## Usage

### Prepare the board (soldering required)

Buy the [**YD-ESP32-S3** from VCC-GND Studio (its page on AliExpress)](https://www.aliexpress.com/item/1005004025123046.html).
On that page choose **N16R8** or **N8R2**: those two are supported.
The board has two USB-C ports, labelled `COM` and `USB` ([photos and documents from its maker](https://github.com/vcc-gnd/YD-ESP32-S3)).

**A note on radio certification.** The YD-ESP32-S3 is a good, inexpensive board. One thing to know before buying: its Wi-Fi module (marked `ESP32-S3-N16R8` or `ESP32-S3-N8R2`) is not one of Espressif's certified modules. It has no Japanese 技適 (we looked it up in the MIC database), and we have not found a radio certification for it in any other country either. Whether that matters depends on where you are: many countries put the obligation on sellers rather than on hobbyists, so please check the rules that apply to you. In Japan, the Radio Act asks the user's own equipment to carry the 技適 mark, so this board cannot be used there; for Japan we are making a board around the certified ESP32-S3-MINI-1 module, see [`hardware/board/`](hardware/board/) (designed and ordered in November 2026, not yet verified).

Short the `USB-OTG` solder pad on the back of the board (shown below before bridging).

<img src="web/usb-otg-pad.webp" alt="USB-OTG solder pad on the back of the board, still open" width="360">
This is required: it feeds 5 V from the board to the target's USB port, and without it the target chip never powers up and is never detected.
The two ports have fixed roles: **`COM` = 5 V power in** (a charger or a PC), **`USB` = the SBC** (the target's debug USB port). For flashing and setup, `COM` goes to the PC (it powers the board as well); `USB` can stay on the SBC.
See "Power and wiring" below for the safety rule that follows from it.

### Flash from a browser

Open [signal-slot.github.io/usb-serial-over-tailscale](https://signal-slot.github.io/usb-serial-over-tailscale/) in Chrome or Edge to flash and configure the board without installing anything.
The page opens the board's COM port (CH343, 1a86:55d3) or its USB Serial/JTAG port (303a:1001) with WebSerial and writes the CI-built binaries with ESP Web Tools (esptool-js).
A terminal on the same page runs the setup wizard.
It works on Windows 10 and later, macOS, Linux and ChromeOS; it does not work in Firefox, Safari or on phones.
Connect the COM port: it flashes through the board's auto-download circuit, carries the boot log, and the target can stay plugged into the native port.
The native USB port works too and starts the wizard as soon as a terminal opens, but it disappears when setup completes and the port becomes the USB host.
If the operating system does not list the COM port, use the native port instead.

Pick the module variant on the page from the marking on the module's metal can (for example `ESP32-S3-N16R8`).
CI (GitHub Actions) builds every variant on each push. Tags matching `v*` also attach `usb-serial-over-tailscale-<variant>.bin` (bootloader, partition table and app merged into one file) to a release.
The merged file can be written with `esptool write_flash 0x0`, but it also overwrites the NVS area with 0xFF, which erases the configuration and registration.
Updates from the page write only the three regions and keep the configuration.

### Initial setup

An unconfigured board plugged into a PC through the native USB port shows up as a USB serial port (Espressif USB JTAG/serial, 303a:1001).
Open it in a terminal and press Enter to start the wizard.
The COM port carries the same shell at 115200 together with the log output; it stays quiet until Enter, and `setup` starts the wizard there.

```bash
screen /dev/ttyACM0 115200
```

Leave DTR and RTS as a terminal leaves them, both asserted.
On this port they are wired to the chip's reset and boot pins, so a tool that
drives them apart resets the board into the ROM download mode, where it goes
quiet and answers nothing. `espflash monitor --port /dev/ttyACM0` is the safe
way to watch the port from a script.

```text
usb-serial-over-tailscale 0.1.0 - setup mode
Press Enter to start setup, or type a command:
=== Setup ===
Scanning Wi-Fi...
   1) home-wifi        (-48 dBm)
Select network number, or type an SSID (Enter to rescan, q to quit): 1
Password for home-wifi: ********
Connected (192.168.1.42)
Hostname on the tailnet [target-console]:
Registering with Tailscale... (press q to stop waiting)
Open this URL in a browser to approve the device:
  https://login.tailscale.com/a/xxxxxxxx
Waiting for approval...
Tailscale is up: target-console.example.ts.net 100.x.y.z
Setup complete. Reboot into normal mode now? [Y/n]
```

`setup` only adds a Wi-Fi network when the hostname and the Tailscale registration are already done.
`setup all` starts over.
To use an auth key, run `authkey tskey-auth-...` and then `login`.

The main commands are listed below; `help` prints the full list.

| Command | Description |
| --- | --- |
| `wifi <ssid> <pw>` | Add a Wi-Fi network and connect. Several can be stored; the strongest known AP is chosen at boot and on reconnect. |
| `wifi list`, `wifi forget <ssid>` | List and remove stored networks. |
| `usbdrv <auto\|cdc\|ftdi\|cp210x\|ch34x>` | USB-serial bridge chip. The default detects it from VID and PID. |
| `port <n>`, `baud <rate>` | First TCP port (default 35930; USB interface n of the target is served on port+n) and baud rate of the target (default 115200). |
| `status`, `reset`, `reboot` | Show status, erase configuration and keys, reboot. |

The same shell is always available on the UART port (115200 bps, shared with the log).
Press Enter to get a prompt.
Holding BOOT while powering on enters setup mode again.

### Connecting

Every serial interface of the target is served on its own TCP port: the first port (35930 unless `port` changed it) plus the USB interface number.
Which interfaces those are is read from the device when it is plugged in, so nothing has to be configured.

| Target | Interface | Port |
| --- | --- | --- |
| A plain CDC-ACM console (one function) | 0 | 35930 |
| Raspberry Pi Debug Probe (interface 0 is CMSIS-DAP) | 1 | 35931 |
| Toradex Dahlia, MCU console (FT4232H port C) | 2 | 35932 |
| Toradex Dahlia, Verdin console (FT4232H port D) | 3 | 35933 |

From any machine on the tailnet, open a raw TCP connection to that port of the hostname or tailnet address, or an RFC 2217 (Telnet) one.

```bash
socat -,rawer,escape=0x1d TCP:target-console:35933   # Ctrl-] to exit
nc target-console 35933
python -m serial.tools.miniterm rfc2217://target-console:35933 9600   # RFC 2217: the client sets the line
esptool --port rfc2217://target-console:35930 flash_id                 # DTR and RTS reset an ESP32 target
```

One connection at a time is served per port.
A second connection while one is active receives `busy` and is closed.
A peer that stops responding is dropped after 120 seconds and the next connection is accepted.

The first byte the client sends decides what the session is.
A client that opens with Telnet negotiation (IAC, 0xFF), as pyserial's `rfc2217://` and esptool do, gets an RFC 2217 session: it can set the baud rate, data bits, parity and stop bits, drive DTR and RTS, hold a break and purge the receive buffer, and every setting is answered with the value in force, so a refused one is visible to the client.
The settings last until the client disconnects; the next session starts again from the adapter's `baud` and 8N1 with DTR and RTS asserted.
Flow control is reported as none, and modem-state and line-state notifications are not sent.
Anything else is a raw session: bytes flow unchanged in both directions, there is no protocol on top, and the baud rate is the adapter's setting.
A `telnet` client that negotiates first (netkit, inetutils) gets an RFC 2217 session in character mode; the BSD one waits for the server to speak first and so gets a raw session in line mode.
Bytes the target sends before the client's first byte go out unframed in either case.

The ports are independent: each has its own USB interface, its own busy state and its own line settings, and a client on one port does not block another.
They share one USB device and one tailnet address.
The USB interface is claimed when the first client connects to its port and stays claimed after the client leaves; output from the target is dropped while nobody is connected.
The ESP32-S3's host controller has eight pipes, one for control and two per claimed interface (three for CDC-ACM), so at most three ports of an FT4232H can be claimed, and the claims last until the device is unplugged or the adapter reboots. A client on a fourth port is told `usb: cannot open the interface: no free USB host pipe` and closed.
`status` on the shell lists the interfaces with their ports, marks the open ones with their line settings and the idle claimed ones with `claimed`.

### Power and wiring

Power the board through the COM-side USB port, from a PC or a USB charger.
The native port is the USB host port for the target; the target's debug port is not expected to supply power.

To supply 5 V from the native port, bridge the `USB-OTG` solder pad on the back of the board.
The pad bypasses the diode between the board's 5 V and the native port, so the 5 V of the COM side comes out of the native port (one diode drop lower). After bridging it **never plug both ports into PCs at the same time**: the COM side would feed the PC on the native port.
For flashing and setup, connect the COM port to the PC; if you use the native port for that instead, connect only that one.
In normal operation, connect the COM port to power and the native port to the target.
If the target is a CDC-ACM or FTDI-style bridge chip, that chip is usually bus-powered and needs this 5 V.

### Seeed Studio XIAO ESP32S3

The XIAO ESP32S3 is a second board the firmware runs on.
It has one USB-C port and no USB host role here: it bridges a **3.3 V UART** instead of a USB serial adapter, on port 35930.
Use it when the target has a UART on pins or a debug connector, such as the 3-pin debug connector of a Raspberry Pi 5.

| XIAO pin | Role | Connect to |
| --- | --- | --- |
| `GND` | ground | the target's GND |
| `3V3` | not used | leave unconnected |
| `D10` | TX (out of the XIAO) | the target's RX |
| `D9` | RX (into the XIAO) | the target's TX |

The four pins are next to each other on one side of the board (from the USB-C end: 5V, GND, 3V3, D10, D9, D8, D7), so one 4-pin socket or pin header carries the link.
The UART is 3.3 V only: do not connect 5 V or RS-232 signals, and put a level translator in between for a 1.8 V target.
Attach the antenna that comes with the board.

Flash it from the same page: choose "Seeed Studio XIAO ESP32S3" under "Board".
If no serial port appears, hold the `B` (BOOT) button while plugging the board in.
The USB port stays the setup console after setup as well (there is no second port), and it powers the board: a charger or a USB port of the target will do.
The baud rate is set with `baud` (115200 by default) or by an RFC 2217 client; there are no DTR and RTS lines.

Verified with the plain XIAO ESP32S3 on a Raspberry Pi 5; the Sense and Plus versions have not been tried.
The firmware limits the Wi-Fi transmit power to 15 dBm on this board: at 19.5 dBm and above it did not get past association with two access points, and at 18 dBm and below it did.
We have not confirmed the board's radio certifications from its maker's documents; check the marking on the board and the rules that apply to you.
[`hardware/xiao-carrier/`](hardware/xiao-carrier/) is a small carrier board for it (not made yet).

### A board that resets every few seconds

If the COM port log shows a new `ESP-ROM` banner every 1 to 4 seconds with `rst:0x3 (RTC_SW_SYS_RST)`, read the line `last reset:` near the top of the next boot (or `Reset:` in `status`).
`brownout` means the supply voltage dropped while Wi-Fi was transmitting or the target's bridge chip was starting.
The fix is the power: a port or charger that can supply the board and the target's bridge chip together, with a short cable.
The IDF's own "Brownout detector was triggered" line can be missing from the log, and the ROM banner looks the same as after `reboot`.
The `Password length is zero` warning in older firmware came from a placeholder configuration on every boot and did not mean the stored password was empty; `wifi list` shows whether each network has one.
`RECOVERY.md` has the procedure as it was done for one board.

## Building and flashing

Prerequisites are the `esp` toolchain installed with `espup`, `ldproxy` and `espflash`.
ESP-IDF v5.3.3 and the `espressif/usb_host_cdc_acm` component are fetched into `firmware/.embuild/` on the first build (several GB, several minutes).

```bash
# host-side unit tests and verification tool
cargo test
cargo run -p hostnode -- probe-control                      # control-plane check (an AuthURL from a keyless registration is enough)
cargo run -p hostnode -- run --hostname tsnode-test        # join a real tailnet and serve an echo server (prints the approval URL)

# firmware
cd firmware
cargo build --release
cargo run --release      # espflash flash --monitor --flash-size 16mb --partition-table partitions.csv
```

Do not source `~/export-esp.sh`.
gcc and clang come from what esp-idf-sys installs into `.embuild/`; with espup's Xtensa gcc first on PATH the link fails.

The partition table is `firmware/partitions.csv` (6 MB factory app).
To flash with esptool, build the binaries into `dist/<variant>/` with `tools/mkimage.sh <variant> release`.

```bash
cd firmware && tools/mkimage.sh n16r8 release
PY=$(ls -d .embuild/espressif/python_env/*/bin/python | head -1)
$PY -m esptool --chip esp32s3 --port /dev/ttyACM0 --baud 921600 write_flash \
  0x0 dist/n16r8/bootloader.bin 0x8000 dist/n16r8/partition-table.bin 0x10000 dist/n16r8/app.bin
```

The cargo runner pins the board's UART port to `/dev/ttyACM0`; adjust `.cargo/config.toml` for your machine.
`hostnode run` stores its keys in plain text in `hostnode-state.json`.

## Design decisions

- Capability version 106; map responses are received uncompressed. Headscale is not supported.
- Authentication defaults to the approval URL; an auth key is optional and is removed from NVS once registration succeeds.
- Tailscale ACLs (PacketFilter) are enforced on the receiving side. Every packet is dropped until the filter arrives.
- The home DERP region is chosen at boot by STUN round-trip time to each region, falling back to `tok` and then to the lowest region ID.
- Direct paths are established with disco ping/pong and call-me-maybe. The source of an authenticated UDP packet from a peer is also adopted as a path.
- WireGuard timestamps are corrected with the offset between the system clock and control time, and with SNTP.
- Network thread stacks live in PSRAM; only the control thread, which writes flash, keeps an internal-RAM stack.
- The USB host relies on the CDC-ACM host driver's fallback of opening any interface with two bulk endpoints; FTDI, CP210x and CH34x get only their chip-specific setup through vendor control transfers.
- The interface stays claimed between sessions because of the USB data toggle. A pipe the host creates starts at DATA0, while the device keeps the toggle of each endpoint from the session before. Claiming the interface per session made the device acknowledge and drop the first packet of every other session, with no error on the host: the first keystroke vanished. Asking the device to reset its toggles with CLEAR_FEATURE(ENDPOINT_HALT) was tried and does not work on the Debug Probe (TinyUSB), so the pipes are kept instead.
- A claim is not released while the device is attached, so a fourth port cannot take the pipes of an idle one. Closing an interface of a device that keeps answering IN polls (an FTDI chip does, every 2 ms) races with the CDC driver's task: the driver halts and clears the pipe and releases the interface while its task resubmits the poll, and the release or the host controller's interrupt handler aborts the firmware about one close in ten. Retrying the release and halting the pipes again from the closing thread (through a linker `--wrap`) and ESP-IDF v5.3.5 (espressif/esp-idf#17707) were tried and still crashed.
- A break on a Linux console, or bytes sent at a lower baud rate than the console's, is a BREAK to the target: `agetty` then cycles through its baud rates, and the kernel's SysRq takes the next byte as a command, which can reboot the target. Test line settings against a console that has nothing to lose.

## Out of scope

Secure Boot and flash encryption, OTA, a web management UI, multiple simultaneous connections, USB hubs and a dedicated PCB.

## License

MIT.
The Tailscale and WireGuard protocol code was written from the protocol descriptions and the Tailscale (BSD-3-Clause) and wireguard-go (MIT) sources.
The USB-serial baud-rate encodings were written from FTDI's application note AN232B-05, Silicon Labs' AN571, Espressif's esp-usb VCP drivers (Apache-2.0) and FreeBSD's `uftdi` and `uchcom` drivers (BSD-2-Clause).
