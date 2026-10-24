# cyd-lab

A desktop workbench for **emulated ESP32 "Cheap Yellow Display" boards**. Each
device in the list behaves like a CYD sitting on your bench: it has its own flash chip,
a power switch, EN (reset) and BOOT buttons, a serial console, and a touch screen.
The emulation is done by [esp32sim](https://github.com/mikroverk/esp32sim), which
cyd-lab runs as an external program, one process per powered-on device.

Written in Rust with [egui/eframe](https://github.com/emilk/egui). Windows is the main
target, and it also builds and runs on Linux.

![screenshot](../cyd-lab-screenshot-2.png)

## Status (v0.2)

| Works | Stubbed / not yet |
| --- | --- |
| Profile library (built-in TOML plus a user folder) | Real `COMxx` ports (com0com + hub4com): planned commands shown, not automated |
| Device store: create, rename, delete, erase flash, import a .bin at an offset | `--serial-tcp` end to end: wired up, but no engine build with it was available to test |
| Flash persists across power cycles (`--flash-file`, graceful stop) when the engine supports it | Graceful stop on **Windows** (the engine has no quit path there yet → killed after the grace period) |
| Several devices running at once; graceful stop of all of them when the app exits | |
| Power; EN = in-process reset when BOOT is unchanged, otherwise save + restart with the new strap | |
| Live screen, touch (verified pixel-exact at 0° and 90°) and UART0 line input over `--web` | |
| Per-device serial TCP port + COM name, `rfc2217://127.0.0.1:PORT` with Copy | |
| Engine feature detection (`--help` + scanning the exe), shown in Settings and as badges | |
| PNG fallback (`--tft-png`), console with a 5000-line ring buffer | |

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
* **EN: soft reset when possible, restart when needed.** On a real board EN resets the chip
  and the ROM samples the strapping pins again. If BOOT hasn't changed since power-on, cyd-lab
  sends the engine's in-process `{"t":"reset"}` (POWERON reset, same strap, flash untouched).
  If BOOT changed (or there is no live link), it does a graceful stop - so flash is saved -
  and restarts with the new strap (`0x0f` = download, `0x13` = SPI boot).
* **Graceful stop, then kill.** With `--flash-file` the engine writes `flash.bin` only when it
  quits cleanly. `request_stop` asks politely through every channel at once (a WS `quit`
  message for future engines, closing stdin, and SIGTERM on Unix), shows "saving flash…",
  and kills the process if it hasn't exited after `stop_grace_secs` (Settings, default 6 s).
  Today's engine answers SIGINT/SIGTERM on Unix: it prints `[emu] flash: wrote N bytes …`
  and exits with code 130, which cyd-lab reports as "flash.bin saved". On Windows there is
  no such path yet, so the engine is killed after the grace period and that run's writes are
  lost (the console says so). Without `--flash-file` support the engine is killed at once,
  since there is nothing to save. On Linux engines also get `PR_SET_PDEATHSIG` so they do not
  outlive a crashed cyd-lab. (The current engine then exits on its next log line without
  saving; only a normal quit saves.)
* **Feature detection.** `EngineFeatures::probe` runs `esp32sim --help` once per exe and also
  scans the binary for flag names, because `--help` doesn't list every flag (`--flash-file`
  is missing from it). New flags are used as soon as an engine build has them.
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
| `--no-dump` | always | |
| `--realtime` | Settings (default on) | |
| profile `extra_args`, then Settings `extra_args` | | anything else, e.g. `--no-jit` |
| env `ESP32SIM_ROM_DIR` | Settings ROM dir | |

Not used yet: `--flash-at OFFSET=FILE`. Import writes into `flash.bin` directly instead,
so the image keeps what was imported.

WebSocket messages used (`esp32sim docs/web-ui.md`): binary type 1 (TFT frame) in;
`{"t":"touch","x","y","down"}`, `{"t":"serial","src":"uart0","line"}`, `{"t":"reset"}` (EN)
and `{"t":"quit"}` (ignored by today's engine) out. Each touch is held for at least 120 ms,
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
esptool.py --port "rfc2217://127.0.0.1:22170?ign_set_control" chip_id
# PlatformIO / Arduino: upload_port = rfc2217://127.0.0.1:22170
```

The engine maps DTR/RTS through the CH340 auto-reset circuit (`engine::bridge::auto_reset`:
EN low = RTS ∧ ¬DTR, IO0 low = DTR ∧ ¬RTS), so esptool can reset the board into the ROM
bootloader. If the engine has no `--serial-tcp`, the port is reserved and the UI says it is
not served.

### COM ports on Windows (TODO, not automated, unverified)

Tools that want a real `COMxx` need a [com0com](https://sourceforge.net/projects/com0com/)
virtual pair plus hub4com relaying the hidden end to the RFC2217 port. The planned commands
(generated by `engine::bridge::planned_com0com_commands`, and shown in the COM tooltip) for
`COM20` / port 22170:

```
:: once per device, admin (signed com0com driver):
setupc.exe install PortName=COM20,EmuBR=yes PortName=CNCB20,EmuBR=yes

:: while the device is powered:
hub4com.exe --create-filter=escparse,com,parse --create-filter=pinmap,com,pinmap:"--rts=cts --dtr=dsr --break=break" ^
  --create-filter=linectl,com,lc:"--br=remote --lc=remote" --add-filters=0:com ^
  --create-filter=telnet,tcp,telnet:"--comport=client" --create-filter=pinmap,tcp,pinmap:"--rts=cts --dtr=dsr --break=break" ^
  --create-filter=linectl,tcp,lc:"--br=local --lc=local" --add-filters=1:tcp ^
  --octs=off \\.\CNCB20 --use-driver=tcp 127.0.0.1:22170
```

Plan: a "Create COM port…" action that runs `setupc` elevated once, then cyd-lab spawns
hub4com next to the engine at power-on and stops it at power-off. Neither command has been
run against the engine yet.

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
auto-reset truth table, the planned com0com commands, rotation mapping, and process lifecycles
against fake engine scripts (graceful stop, kill after the grace period, restart).

## What the engine needs next

1. **A graceful quit that works on Windows**: honour a WS `{"t":"quit"}` message and/or EOF on
   stdin (cyd-lab already sends both), and handle Ctrl-C / console close there, writing
   `--flash-file` before exiting. Today only Unix signals save the flash.
2. **Save before logging on the way out**: when stderr/stdout is a closed pipe (cyd-lab has
   died), `eprintln!` panics before `save_flash` runs. Write the flash first, or ignore write
   errors on the log lines.
3. **`--serial-tcp PORT` (RFC2217)**: cyd-lab passes it as soon as the exe advertises it; then
   test esptool against the URL and wire up hub4com.
4. **List `--flash-file` in `--help`** so detection doesn't need to scan the binary, and
   ideally a machine-readable `--capabilities` output.
5. **Exit code**: a clean signal-triggered quit exits 130; a 0 (or a distinct documented code)
   would be easier to tell apart from a real interrupt.

## License

MIT © 2026 Gareth Johnstone
