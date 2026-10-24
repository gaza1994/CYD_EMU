# cyd-lab

A desktop workbench for **emulated ESP32 "Cheap Yellow Display" boards**. Each
device in the list behaves like a CYD sitting on your bench: it has its own flash chip,
a power switch, EN (reset) and BOOT buttons, a serial console, and a touch screen.
The emulation is done by [esp32sim](https://github.com/mikroverk/esp32sim), which
cyd-lab runs as an external program, one process per powered-on device.

Written in Rust with [egui/eframe](https://github.com/emilk/egui). Windows is the main
target, and it also builds and runs on Linux.

![screenshot](../cyd-lab-screenshot-3.png)

## Status (v0.3)

| Works | Stubbed / not yet |
| --- | --- |
| Profile library (built-in TOML plus a user folder) | Optional real `COMxx` ports on Windows (off by default): implemented (com0com detection, "Create COMx pair", hub4com relay per running device) but **never run on Windows**, only compile-checked |
| Device store: create, rename, delete, erase flash, import a .bin at an offset | |
| Flash persists across power cycles (`--flash-file`, graceful stop, exit 0) on Linux and, by the engine's contract, Windows | |
| `--serial-tcp`: esptool `write-flash` over `rfc2217://127.0.0.1:PORT` tested end to end (921600 baud, hash verified, board reboots and draws) | |
| Several devices running at once; graceful stop of all of them when the app exits | |
| Power; EN and BOOT act in-process (no restart), see [Serial bridge](#serial-bridge) | |
| Status dot follows the chip: green running, blue "download mode (serial)" when esptool put it there, red "halted: waiting for EN" | |
| Live screen, touch (verified pixel-exact at 0° and 90°) and UART0 line input over `--web` | |
| Engine feature detection (`--help`, binary scan as a fallback), shown in Settings and as badges | |
| PNG fallback (`--tft-png`), console with a 5000-line ring buffer | |

## Architecture

```
src/
  main.rs     window setup; Windows release builds use the GUI subsystem (no console window)
  app.rs      egui UI: device list, controls, screen, console, dialogs. Owns one DeviceView per device
  profile.rs  board profiles (TOML): parse, validate, library = built-ins + user dir
  device.rs   on-disk device store: device.toml + flash.bin per device; import/erase/rename/delete
  engine.rs   Backend trait, ProcessBackend (esp32sim child process + optional hub4com relay),
              command-line mapping, feature detection, ChipStatus (boot mode from console lines),
              port allocation, auto-reset truth table (bridge module), Windows job object (winjob)
  rfc2217.rs  minimal RFC2217 client: pulses DTR/RTS on the engine's serial port for EN with BOOT
  combridge.rs com0com/hub4com: detection, setupc/hub4com command lines, COM relay plan
              (Windows-only parts are UNTESTED at runtime)
  weblink.rs  WebSocket client for esp32sim --web: frames in, touch/serial/btn/reset/quit out
profiles/     built-in profiles, compiled in with include_str!
```

**Why it is built this way**

* **The engine sits behind one trait (`engine::Backend`).** The UI only calls `start / stop /
  reset / poll / take_frame / touch / serial_line` and reads `capabilities()`. Today
  `ProcessBackend` runs `esp32sim` as a child process. Later an `InProcessBackend` could link
  the esp32sim crates directly, and the UI would not change. `Capabilities` tells the UI
  what the engine can do (live screen, touch, PNG, flash write-back, COM bridge). The badges
  under the device header come from it, so new engine features show up without UI rewrites.
* **One process per device.** The emulator is CPU-heavy and keeps global state. Separate
  processes give separate memory, so one crashed board can't take down the app or the other
  boards, and Windows spreads them across cores. On Windows every child goes into one Job
  object with `KILL_ON_JOB_CLOSE`, so the children die with the app even if it crashes.
  Children are started with `CREATE_NO_WINDOW`. On a normal exit, `on_exit` and `Drop` stop
  every child.
* **The live link reuses esp32sim's own `--web` WebSocket.** It already streams TFT frames
  (binary type 1, RGB565) and accepts `touch` and `serial` messages, the same protocol its
  browser UI uses. A native client (tungstenite, no Origin header, so the engine treats it
  as a local tool) gives a live screen and working touch today with no engine changes. One
  thread per device keeps only the newest frame in a single-slot mailbox, so a slow UI drops
  frames instead of slowing the engine. `--tft-png` is the fallback, but the engine writes
  it only when a run ends.
* **EN and BOOT without restarting.** BOOT is a live pin: holding it sends
  `{"t":"btn","pin":0,"v":0}` (releasing sends `v:1`), so firmware that reads GPIO0 sees it.
  EN resets the chip, and the ROM samples GPIO0 again. If the chip is already in the mode the
  BOOT checkbox asks for, EN is the engine's `{"t":"reset"}` (POWERON reset, the strap is
  kept). If it is not (BOOT held but the chip is running the app, or released while it sits in
  download mode), the WebSocket has no way to change the strap, so cyd-lab does what a USB
  adapter does: it connects to the engine's `--serial-tcp` port and plays esptool's DTR/RTS
  sequence through the auto-reset circuit (`rfc2217::pulse_en`: EN low, then IO0 held low as
  EN rises for download mode). The chip resets in place, flash is untouched. The engine
  takes one serial client at a time, so this disconnects a tool that is connected at that
  moment (the console says so). Without a serial port it falls back to save + restart with
  `--strap 0x0f`/`0x13`.
* **Chip status from the console.** `ChipStatus::observe` reads the engine's
  `[emu] serial: EN reset[, IO0 low: download mode]`, `[emu] serial: chip stopped …` and the
  ROM banner (`boot:0x13 (SPI_FAST_FLASH_BOOT)` vs `boot:0xf (UART_BOOT(UART0))`), so the
  status dot turns blue when esptool resets the board into the bootloader, red when the chip
  stops and waits for a reset, and green again after it boots the app.
* **Graceful stop, then kill.** With `--flash-file` the engine writes `flash.bin` only when it
  quits cleanly. When the engine has `--quit-on-stdin-eof`, cyd-lab passes it and stops a
  device with the WS message `{"t":"quit"}` plus closing stdin. Both work the same on
  Windows and Linux, and the engine saves and exits 0 (`[emu] flash: wrote N bytes …`,
  reported as "flash.bin saved"). Older engines also get SIGTERM on Unix (exit 130). After
  `stop_grace_secs` (Settings, default 6 s) the process is killed and that run's writes are
  lost (the console says so). Without `--flash-file` support the engine is killed at once.
  If cyd-lab itself dies: on Linux the engine gets `PR_SET_PDEATHSIG` = SIGTERM and stdin
  EOF, both of which save. On Windows the Job object (`KILL_ON_JOB_CLOSE`) kills it
  **without** saving (see open issues).
* **Feature detection.** `EngineFeatures::probe` runs `esp32sim --help` once per exe; current
  engines list `--flash-file`, `--serial-tcp`, `--quit-on-stdin-eof` and `--web` there. Only
  if a flag is missing from the help does it scan the exe for the flag name (older builds
  whose help was incomplete). New flags are used as soon as an engine build has them.
* **Devices are plain folders.** `device.toml` plus a raw `flash.bin` (blank = 0xFF) is easy
  to back up, diff, or hand to `esptool`/`esp32sim` directly. Writes are atomic (temp file,
  then rename). The id is a slug fixed at creation, so renaming changes only the display name.
* **Profiles are data.** A profile holds everything about a board: chip, engine board model,
  flash, display/touch/LED/LDR pins, USB-serial chip. A new board is a TOML file in the user
  profiles folder, not a code change. Unknown keys are rejected so typos show up.

**Threads**: UI thread (egui) · per running device: stdout reader, stderr reader, WebSocket
link. All of them post to an mpsc channel and call a `Waker` (`ctx.request_repaint`), so the UI
sleeps when nothing happens.

## Engine contract (esp32sim CLI mapping)

`engine::LaunchSpec::build` turns (profile, device, settings, BOOT state) into:

| esp32sim flag | Source | Notes |
| --- | --- | --- |
| `--chip esp32` | profile `chip` | |
| `--board <name>` | profile `[engine] board` | `cyd-2432s028r`, `tdisplay` |
| `--boot rom` | always | runs the real mask ROM, so straps and download mode behave like hardware |
| `--rom <rom_dir>/<rom>` | Settings ROM dir + profile `[engine] rom` | `esp32_rev300_rom.elf`; left out if no ROM dir is set |
| `--flash-file <device>/flash.bin` | device, engine has `--flash-file` | read at start, written back on a clean quit; the "flash write-back" badge |
| `--flash-image <device>/flash.bin` | device, older engines | the whole flash, written at 0; writes are lost at power-off |
| `--flash-mb N` | profile `[flash] size_mb` | |
| `--psram-mb N` | profile `psram_mb` | **never** for classic ESP32 (the engine rejects it); only when > 0 |
| `--strap 0x13` / `--strap 0x0f` | BOOT not held / held | SPI boot / UART download |
| `--mac xx:..` | device `mac` | made at creation, Espressif OUI 24:0A:C4 |
| `--console uart0` | profile `[engine] console` | |
| `--tft-png <device>/screen.png` | Settings "PNG snapshot", board has a display | written when the run ends |
| `--max-seconds N` | Settings (default off) | interactive devices run until powered off |
| `--web PORT` | Settings "Live link" | ports from 18320 up, one per running device |
| `--web-dir <device>/web` | Settings web dir, else an empty per-device folder | without it esp32sim looks for `web/` next to its exe and panics if it is missing |
| `--serial-tcp PORT` | device `serial_tcp_port`, only if the engine has the flag | RFC2217 server for UART0; ports from 22170 up, saved per device |
| `--quit-on-stdin-eof` | only if the engine has the flag | stdin is a pipe kept open by cyd-lab; closing it = save and exit 0 |
| `--no-dump` | always | |
| `--realtime` | Settings (default on) | |
| profile `extra_args`, then Settings `extra_args` | | anything else, e.g. `--no-jit` |
| env `ESP32SIM_ROM_DIR` | Settings ROM dir | |

Not used yet: `--flash-at OFFSET=FILE`. Import writes into `flash.bin` directly instead,
so the image keeps what was imported.

WebSocket messages used (`esp32sim docs/web-ui.md`): binary type 1 (TFT frame) in;
`{"t":"touch","x","y","down"}`, `{"t":"serial","src":"uart0","line"}`, `{"t":"btn","pin":0,"v"}`
(BOOT), `{"t":"reset"}` (EN, same strap) and `{"t":"quit"}` (save and exit 0) out. Each touch is held for at least 120 ms,
because the engine samples the panel at its own pace and a press and release in the same
frame (a fast click, or `xdotool click`) would otherwise be missed. Touch
coordinates are native panel pixels. The UI maps the mouse back through the view rotation
(0/90/180/270, saved per device).

## Serial bridge

Each device has a **TCP serial port** (auto-assigned from 22170 when the device is created,
changeable, with an **Auto** button that picks a free one) and a **COM name** (`COM20`, ...).
The header shows `rfc2217://127.0.0.1:PORT` with a **Copy** button. When the engine has
`--serial-tcp`, cyd-lab passes the port and the URL is live while the device runs:

```
esptool --chip esp32 --port rfc2217://127.0.0.1:22170 --baud 921600 \
  write-flash 0x1000 ESP32_GENERIC-v1.29.0.bin 0x200000 cyd-vfs.bin
# PlatformIO / Arduino: upload_port = rfc2217://127.0.0.1:22170
```

Tested (esptool 5.6.0): 737.7 kbit/s effective, hashes verified, "Hard resetting via RTS pin"
reboots the board into the new firmware, and the image survives a power cycle. The engine
maps DTR/RTS through the CH340 auto-reset circuit (EN low = RTS ∧ ¬DTR, IO0 low = DTR ∧ ¬RTS),
so no `?ign_set_control` is needed. It serves one client at a time; while one is connected,
UART0 output goes to it rather than to cyd-lab's console. If the engine has no
`--serial-tcp`, the port is reserved and the UI says it is not served.

### Optional extra: COM ports on Windows (off by default, UNTESTED on Windows)

**You don't need this.** Everything in cyd-lab works over the network: esptool, the Arduino
IDE / arduino-cli (see [Arduino](#arduino-ide-2--arduino-cli-no-driver)), PlatformIO and
ESPHome all accept `rfc2217://127.0.0.1:PORT` directly, and web installers can be imported
with [Install from web installer](#install-from-web-installer). A real `COMxx` port is only
for old tools that insist on one. It is off by default: turn on **Settings → COM ports
(optional)** to show the COM name field and start the relay at power-on.


Tools that want a real `COMxx` need a [com0com](https://sourceforge.net/projects/com0com/)
virtual pair. hub4com relays the hidden end of the pair to the RFC2217 port. For `COM20` /
port 22170:

```
:: once per device, as administrator (signed com0com driver):
setupc.exe install PortName=COM20 PortName=CNCB20

:: while the device is powered (what com2tcp-rfc2217.bat \\.\CNCB20 127.0.0.1 22170 runs):
hub4com.exe --create-filter=escparse,com,parse --create-filter=pinmap,com,pinmap:"--rts=cts --dtr=dsr" ^
  --create-filter=linectl,com,lc:"--br=local --lc=local" --add-filters=0:com ^
  --create-filter=telnet,tcp,telnet:"--comport=client" ^
  --create-filter=pinmap,tcp,pinmap:"--rts=cts --dtr=dsr --break=break" ^
  --create-filter=linectl,tcp,lc:"--br=remote --lc=remote" --add-filters=1:tcp ^
  --octs=off \\.\CNCB20 --use-driver=tcp *127.0.0.1:22170
```

What cyd-lab does (`combridge.rs`, `cfg(windows)`):

* **Detection.** It looks for `setupc.exe` and `hub4com.exe` in `CYD_LAB_COM0COM_DIR`,
  `%ProgramFiles(x86)%\com0com`, `%ProgramFiles%\com0com` (and `hub4com*` subfolders of
  them), then `PATH`. The COM row of the device header shows what is missing, with where to
  get it ("com0com is not installed. Install the signed com0com driver (com0com 3.0.0.0
  signed) …", "hub4com.exe not found. Unzip hub4com (hub4com-2.1.0.0) into …").
  `setupc list` tells whether `COMx` already exists.
* **"Create COMx pair"** runs `setupc install PortName=COMx PortName=CNCBx` once, through
  an administrator prompt (PowerShell `Start-Process -Verb RunAs`).
* **At power-on**, if the pair exists and hub4com was found, cyd-lab starts `hub4com.exe`
  directly (rather than the .bat via `cmd`, so killing it is reliable) with the arguments
  above. It goes into the same Job object as the engine, its output is shown in the console
  with a `[hub4com]` prefix, and it is stopped at power-off. The `*` before the address
  makes hub4com reconnect, because it can start before the engine's port is listening.
* On Linux and macOS the COM row says COM ports are Windows-only and suggests `socat` for a
  pty, e.g. `socat pty,link=/tmp/ttyCYD,raw tcp:127.0.0.1:22170` (raw bytes only, no DTR/RTS).

Unit tests cover the command lines, detection in a fake folder tree, `setupc list` parsing
and the missing-component messages. The UAC prompt, `setupc` and hub4com themselves
have not been run.

## Data layout

```
%APPDATA%\cyd-lab\            (Linux: ~/.local/share/cyd-lab; override with CYD_LAB_HOME)
  settings.toml               engine exe, ROM dir, live link, ports, ...
  profiles\*.toml             user profiles (a matching id replaces a built-in)
  devices\<id>\device.toml    name, profile, com_port, serial_tcp_port, mac, notes, view_rotation, created
  devices\<id>\flash.bin      raw flash image (size from the profile)
  devices\<id>\screen.png     last --tft-png
  devices\<id>\web\           empty --web-dir
```

## Build and run

### Windows

1. Install Rust with the MSVC toolchain from <https://rustup.rs> (Visual Studio Build Tools
   with "Desktop development with C++").
2. `cargo run --release`
3. In **Settings**, set the esp32sim exe (`esp32sim.exe`) and the ROM directory (the folder
   with `esp32_rev300_rom.elf`, from esp32sim's `tools/fetch-rom-elfs.sh`).
4. **+ New device** → ESP32-2432S028R. Import a merged image at `0x0`, or a bootloader at
   `0x1000`, partitions at `0x8000` and app at `0x10000`. Then **Power on**.

### Linux

```
sudo apt install libxkbcommon-dev libgl1-mesa-dev   # usually already there on a desktop
cargo run --release
```

### Cross-building the Windows exe from Linux

```
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/cyd-lab.exe
```

### Tests

`cargo test` covers profile parsing and validation, device creation (blank 0xFF flash of the
profile's size, unique ids, COM names and serial TCP ports), firmware import at offsets with
bounds checks, command-line construction (strap selection, PSRAM rule, `--flash-file` vs
`--flash-image`, `--serial-tcp` only when supported, quoting), engine feature detection, the
auto-reset truth table and the RFC2217 EN sequences, chip status from console lines,
com0com/hub4com command lines, detection and messages, rotation mapping, and process
lifecycles against fake engine scripts (graceful stop, kill after the grace period, restart,
relay started and stopped with the engine). The esptool flow was tested by hand under Xvfb.

## What the engine needs next

Done in the engine since v0.2: graceful quit on Windows (WS `quit`, `--quit-on-stdin-eof`,
Ctrl-C/close), saving even when the log pipe is closed, `--serial-tcp` (RFC2217), and
`--help` listing every flag. Still useful:

1. **A WS message to set the strap / hold IO0 across a reset** (for example
   `{"t":"reset","strap":"0x0f"}`), so EN with a changed BOOT doesn't have to borrow the
   serial port and kick off a connected tool.
2. **Chip state as structured events** (a WS `{"t":"state","mode":"download"}` message, or a
   machine-readable status line), instead of cyd-lab parsing console text.
3. **Serial output to both the client and the console** (or a WS copy of UART0 TX) while a
   client is connected. Today the console goes quiet while esptool or a hub4com relay
   holds the port.
4. **A save-on-Job-close story for Windows**: if cyd-lab crashes, the Job object kills the
   engine before it can save. A periodic or on-write flush of `--flash-file` would cover it.
5. Optional: `--capabilities` with machine-readable output.

## License

MIT © 2026 Gareth Johnstone
