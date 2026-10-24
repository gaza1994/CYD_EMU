# cyd-lab

A desktop workbench for **emulated ESP32 "Cheap Yellow Display" boards**. Each
device in the list behaves like a CYD sitting on your bench: it has its own flash chip,
a power switch, EN (reset) and BOOT buttons, a serial console, and a touch screen.
The emulation is done by [esp32sim](https://github.com/mikroverk/esp32sim), which
cyd-lab runs as an external program, one process per powered-on device.

Written in Rust with [egui/eframe](https://github.com/emilk/egui). Windows is the main
target, and it also builds and runs on Linux.

![screenshot](../cyd-lab-screenshot.png)

## Status (v0.1)

| Works | Stubbed / not yet |
| --- | --- |
| Profile library (built-in TOML plus a user folder) | COM-port bridge: shows "COMxx: not bridged yet" |
| Device store: create, rename, delete, erase flash, import a .bin at an offset | Flash write-back: firmware writes are lost at power-off |
| Several devices running at once, all killed when the app exits | EN/BOOT without restarting the process |
| Power, EN (restarts the process), BOOT held → strap 0x0f, otherwise 0x13 | |
| Live screen, touch and UART0 line input over esp32sim's `--web` WebSocket | |
| PNG fallback (`--tft-png`), placeholder when nothing is running | |
| Console: stdout/stderr reader threads, a 5000-line ring buffer, autoscroll | |

## Architecture

```
src/
  main.rs     window setup; Windows release builds use the GUI subsystem (no console window)
  app.rs      egui UI: device list, controls, screen, console, dialogs. Owns one DeviceView per device
  profile.rs  board profiles (TOML): parse, validate, library = built-ins + user dir
  device.rs   on-disk device store: device.toml + flash.bin per device; import/erase/rename/delete
  engine.rs   Backend trait, ProcessBackend (esp32sim child process), command-line mapping,
              port allocation, COM bridge design (bridge module), Windows job object (winjob)
  weblink.rs  WebSocket client for esp32sim --web: frames in, touch/serial out
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
* **EN restarts the process.** On a real board EN resets the chip, and the ROM reads the
  strapping pins again. Restarting the process with the current BOOT state (strap
  `0x0f` = download, `0x13` = SPI boot) does exactly that, and is always correct. The engine
  also has an in-process `reset` WS message that a later version can use when BOOT hasn't
  changed.
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
| `--flash-image <device>/flash.bin` | device | the whole flash, written at 0 |
| `--flash-mb N` | profile `[flash] size_mb` | |
| `--psram-mb N` | profile `psram_mb` | **never** for classic ESP32 (the engine rejects it); only when > 0 |
| `--strap 0x13` / `--strap 0x0f` | BOOT not held / held | SPI boot / UART download |
| `--mac xx:..` | device `mac` | made at creation, Espressif OUI 24:0A:C4 |
| `--console uart0` | profile `[engine] console` | |
| `--tft-png <device>/screen.png` | Settings "PNG snapshot", board has a display | written when the run ends |
| `--max-seconds N` | Settings (default off) | interactive devices run until powered off |
| `--web PORT` | Settings "Live link" | ports from 18320 up, one per running device |
| `--web-dir <device>/web` | Settings web dir, else an empty per-device folder | without it esp32sim looks for `web/` next to its exe and panics if it is missing |
| `--no-dump` | always | |
| `--realtime` | Settings (default on) | |
| profile `extra_args`, then Settings `extra_args` | | anything else, e.g. `--no-jit` |
| env `ESP32SIM_ROM_DIR` | Settings ROM dir | |

Not used yet: `--flash-at OFFSET=FILE`. Import writes into `flash.bin` directly instead,
so the image keeps what was imported.

WebSocket messages used (`esp32sim docs/web-ui.md`): binary type 1 (TFT frame) in;
`{"t":"touch","x","y","down"}` and `{"t":"serial","src":"uart0","line"}` out. Touch
coordinates are native panel pixels. The UI maps the mouse back through the view rotation
(0/90/180/270, saved per device).

## Data layout

```
%APPDATA%\cyd-lab\            (Linux: ~/.local/share/cyd-lab; override with CYD_LAB_HOME)
  settings.toml               engine exe, ROM dir, live link, ports, ...
  profiles\*.toml             user profiles (a matching id replaces a built-in)
  devices\<id>\device.toml    name, profile, com_port, mac, notes, view_rotation, created
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
profile's size, unique ids and COM ports), firmware import at offsets with bounds checks,
command-line construction (strap selection, PSRAM rule, optional flags, quoting), the
auto-reset truth table, rotation mapping, and a process lifecycle against a fake engine script.

## What the engine needs next

1. **Flash write-back**: write the flash back to `flash.bin` (on exit, or memory-mapped), for
   example `--flash-writeback` or treating `--flash-image` as read-write. NVS, SPIFFS/LittleFS
   and OTA then survive a power cycle. Then set `Capabilities::flash_writeback = true`.
2. **COM bridge**: a raw UART0 byte stream (not just lines), for example a TCP serial port
   `--uart0-tcp PORT`, plus DTR/RTS → EN/IO0 control without restarting. cyd-lab would connect
   it to one end of a com0com virtual pair (`COMxx`), mapping the CH340 auto-reset circuit
   (`engine::bridge::auto_reset`: EN low = RTS ∧ ¬DTR, IO0 low = DTR ∧ ¬RTS). esptool, the
   Arduino IDE and ESPHome would then flash the emulated board as if it were real. The ROM's
   download mode already runs (strap 0x0f).
3. **Touch input**: already works over `--web` (`touch` message, XPT2046 in the
   `cyd-2432s028r` model). Next would be a dedicated, versioned control channel (stdin or a
   socket) so cyd-lab doesn't depend on the browser UI protocol.
4. **Framebuffer streaming**: already works (WS binary type 1). Next would be dirty-rect
   updates, or a shared-memory framebuffer for an in-process backend.
5. **Buttons / reset without a restart**: the WS `btn`/`gpio`/`reset` messages exist. A
   documented way to set GPIO0 and pulse EN would let BOOT+EN work like on hardware without
   a restart.

## License

MIT © 2026 Gareth Johnstone
