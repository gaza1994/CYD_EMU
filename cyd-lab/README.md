# cyd-lab

A desktop workbench for **emulated ESP32 "Cheap Yellow Display" boards**. Each
device in the list behaves like a CYD sitting on your bench: it has its own flash chip,
a power switch, EN (reset) and BOOT buttons, a serial console, and a touch screen.
The emulation is done by [esp32sim](https://github.com/mikroverk/esp32sim), which
cyd-lab runs as an external program, one process per powered-on device.

Written in Rust with [egui/eframe](https://github.com/emilk/egui). Windows is the main
target, and it also builds and runs on Linux.

![screenshot](../cyd-lab-screenshot-3.png)

## Status (v0.4)

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
| [Install from web installer](#install-from-web-installer): ESP Web Tools manifests (URL or file), tested with openHASP | Improv Wi-Fi provisioning |
| [Arduino IDE 2 / arduino-cli](#arduino-ide-2--arduino-cli-no-driver) over the network: discovery, upload, monitor (arduino-cli on Linux) | Arduino IDE 2 on Windows not yet run; the package index isn't hosted anywhere yet |

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
  webinstall.rs ESP Web Tools manifests: parse, pick the ESP32 build, fetch parts (ureq + rustls)
  running.rs  running.json (powered-on devices), shared with the helper binaries
  bin/cyd-lab-discovery.rs  Arduino pluggable discovery (reads running.json)
  bin/cyd-lab-monitor.rs    Arduino pluggable monitor (RFC2217 / raw TCP bridge)
profiles/     built-in profiles, compiled in with include_str!
arduino/      the cydlab Arduino board package (boards.txt, platform.txt, index builder, example)
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

## Install from web installer

Many firmware projects ship an [ESP Web Tools](https://esphome.github.io/esp-web-tools/)
"Install" button (openHASP, WLED, ESPHome projects, ...). That button uses Chrome's Web
Serial picker, which only lists the computer's own serial/USB ports, so **an emulated
board never shows up there** (it would need a virtual COM driver). cyd-lab reads the
same manifest itself instead:

1. Power the device **off**.
2. Paste the manifest URL into **Web installer** (or press **…** to pick a local
   `manifest.json`) and press **Install**.
3. cyd-lab downloads the manifest, picks the build whose `chipFamily` is `ESP32`,
   downloads each part (paths are relative to the manifest URL) and writes it into the
   device's `flash.bin` at its `offset`. The console logs every step.
4. Power on.

Erase rules follow ESP Web Tools: a new install always erases the flash first, unless the
manifest says `new_install_prompt_erase: true`, in which case the **erase first** checkbox
decides. `improv: true` (Wi-Fi setup over serial) is noted in the console, but cyd-lab
doesn't run Improv. Configure Wi-Fi the firmware's own way. Downloads use ureq with
rustls, up to 16 MB per file, and `file://` and local paths work too.

Tested with openHASP's CYD manifest
`https://nightly.openhasp.com/firmware/esp32-2432s028r_full_4MB.json` (v0.7.0, one 1.7 MB
part at 0x0, `new_install_prompt_erase: true`). It boots to openHASP's Wi-Fi setup screen
(AP `HASP-xxxxxx`, QR code) with a full boot log on the console:

![openHASP installed from its web manifest](../cyd-lab-screenshot-webinstall.png)

Known gap: openHASP's picture comes out **mirrored left to right**. Arduino sketches
(TFT_eSPI `ILI9341_2_DRIVER`) and the earlier test firmware draw the right way round, so
this is about how the engine models MADCTL for the panel openHASP's build drives. It's an
engine issue (esp32sim), not a cyd-lab one. Touch and serial work.

## Arduino IDE 2 / arduino-cli (no driver)

The folder [`arduino/`](arduino/) is an Arduino board package, **cydlab**, that makes
cyd-lab devices appear in Arduino IDE 2 / arduino-cli as **network ports**. Board list,
upload and Serial Monitor all go over `rfc2217://127.0.0.1:PORT`, with no COM driver.

| Piece | What it does |
| --- | --- |
| `running.json` | cyd-lab rewrites `<data dir>/running.json` whenever a device powers on or off: name, id, profile, serial TCP port, plus cyd-lab's pid so a crashed cyd-lab doesn't leave ghosts. |
| `cyd-lab-discovery` (bin) | [Pluggable discovery](https://arduino.github.io/arduino-cli/dev/pluggable-discovery-specification/) v1: HELLO/START/STOP/LIST/START_SYNC/QUIT. Each running device is a port with protocol `cydlab`, address `rfc2217://127.0.0.1:PORT` and label `Kitchen CYD (cyd-lab)`. Sync mode sends add/remove as devices power on and off. |
| `cyd-lab-monitor` (bin) | [Pluggable monitor](https://arduino.github.io/arduino-cli/dev/pluggable-monitor-specification/) v1: bridges the IDE's data channel to the RFC 2217 port (Telnet stripped/escaped) or to `socket://` raw TCP. It never touches DTR/RTS, so opening the monitor doesn't reset the board. |
| `arduino/cydlab/esp32/boards.txt` | Board `cydlab:esp32:cyd2432s028r`, "ESP32-2432S028R CYD (cyd-lab, emulated)". Its settings are copied from esp32 3.3.12's `jczn_2432s028r`, with `build.core=esp32:esp32`, `build.variant=esp32:jczn_2432s028r` and `upload.tool.cydlab=esp32:esptool_py`. |
| `arduino/cydlab/esp32/platform.txt` | Declares the discovery and monitor. It is generated by `gen_platform.py` (see below). |
| `arduino/make_package.py` | Builds `arduino/dist/` (platform and tool archives for Linux x86_64 and Windows x86_64, plus `package_cydlab_index.json` with SHA-256 checksums) for a given `--base-url`. |
| `arduino/examples/CydHello` | A CYD sketch: TFT_eSPI text, XPT2046_Touchscreen touch dots, and Serial echo. |

**Does referencing Espressif's core work with esp32 3.x?** Mostly. `build.core=esp32:esp32`,
the variant reference and `esp32:esptool_py` all resolve. But arduino-cli then sets
`{runtime.platform.path}` to the *cydlab* platform, and esp32's `platform.txt` uses that
to find `tools/gen_esp32part.py`, `partitions/boot_app0.bin`, `espota` and `flasher`. The
first build failed with `can't open file …/cydlab/esp32/tools/gen_esp32part.py`, and
overriding `runtime.platform.path` is ignored. The simplest fix that works:
`gen_platform.py` copies the 21 esp32 properties that mention `{runtime.platform.path}`
into cydlab's `platform.txt`, pointing them at `{build.core.path}/../..` (the esp32
platform folder). Re-run it if a later esp32 core changes those lines. The package does
**not** install the esp32 core for you. Install `esp32:esp32` 3.x first.

**Even simpler:** the cydlab package also works with Espressif's own board. Once cydlab is
installed, the discovery and monitor are global, and for any other protocol esp32's
boards fall back to `upload.tool.default=esptool_py`. So
`--fqbn esp32:esp32:jczn_2432s028r -p rfc2217://127.0.0.1:PORT` uploads and monitors
too (tested).

### arduino-cli (tested on Linux, arduino-cli 1.5.1, esp32 3.3.12, esptool 5.3.1)

```sh
# build the helper binaries and the package, then serve it
cargo build --release                       # + --target x86_64-pc-windows-gnu for the .exe tools
python3 arduino/make_package.py --base-url http://127.0.0.1:8765/
python3 -m http.server -d arduino/dist 8765 &

arduino-cli config add board_manager.additional_urls https://espressif.github.io/arduino-esp32/package_esp32_index.json
arduino-cli config add board_manager.additional_urls http://127.0.0.1:8765/package_cydlab_index.json
arduino-cli core update-index
arduino-cli core install esp32:esp32 cydlab:esp32
arduino-cli lib install TFT_eSPI XPT2046_Touchscreen

# power a device on in cyd-lab, then:
arduino-cli board list
# Port                      Protocol Type                        Board Name                              FQBN                      Core
# rfc2217://127.0.0.1:22172 cydlab   cyd-lab (emulated, network) ESP32-2432S028R CYD (cyd-lab, emulated) cydlab:esp32:cyd2432s028r cydlab:esp32

cd arduino/examples/CydHello
TFT="-DUSER_SETUP_LOADED=1 -DILI9341_2_DRIVER=1 -DTFT_WIDTH=240 -DTFT_HEIGHT=320 -DUSE_HSPI_PORT=1 \
 -DTFT_MISO=12 -DTFT_MOSI=13 -DTFT_SCLK=14 -DTFT_CS=15 -DTFT_DC=2 -DTFT_RST=-1 -DTFT_BL=21 \
 -DTFT_BACKLIGHT_ON=HIGH -DLOAD_GLCD=1 -DLOAD_FONT2=1 -DLOAD_FONT4=1 \
 -DSPI_FREQUENCY=55000000 -DSPI_READ_FREQUENCY=20000000 -DSPI_TOUCH_FREQUENCY=2500000"
arduino-cli compile --fqbn cydlab:esp32:cyd2432s028r --build-property "compiler.cpp.extra_flags=$TFT" .
arduino-cli upload  --fqbn cydlab:esp32:cyd2432s028r -p rfc2217://127.0.0.1:22172 .
arduino-cli monitor --fqbn cydlab:esp32:cyd2432s028r -p rfc2217://127.0.0.1:22172
```

Results: `board list` (and `board list --watch`: add/remove as the device powers on and
off) shows the device. The upload took about 3 s of writing (829 kbit/s, hashes verified),
then "Hard resetting via RTS pin", and the board boots and draws. In the Serial Monitor,
taps print `touch 1: x=41 y=80 …` and typed lines come back as `echo: …`. Set the
device's Screen view to 270° for this landscape sketch.

![CydHello uploaded with arduino-cli](../cyd-lab-screenshot-arduino.png)

### Arduino IDE 2 on Windows (same steps; not yet run on Windows)

1. Run cyd-lab and power a device on.
2. Serve the package (`py -m http.server -d arduino\dist 8765`, or wherever it is hosted).
3. **File → Preferences… → Additional boards manager URLs** → the button next to the box →
   one URL per line:
   `https://espressif.github.io/arduino-esp32/package_esp32_index.json`
   `http://127.0.0.1:8765/package_cydlab_index.json`
   → OK.
4. **Tools → Board → Boards Manager…**: install **esp32** by Espressif Systems (3.x), then
   **cyd-lab (emulated CYD boards)**.
5. **Library Manager**: install **TFT_eSPI** and **XPT2046_Touchscreen**. In
   `Documents\Arduino\libraries\TFT_eSPI\User_Setup.h`, replace the contents with the
   `#define`s from the `TFT=` line above (`#define ILI9341_2_DRIVER`, `#define TFT_MISO 12`,
   …). This is the usual CYD setup. The IDE can't pass per-sketch flags to TFT_eSPI.
6. **Tools → Board → cyd-lab → ESP32-2432S028R CYD (cyd-lab, emulated)**. **Tools → Port**
   lists **CYD 3 (cyd-lab)** under the network/cydlab ports. The board selector at the top
   picks it automatically.
7. **Upload**, then **Serial Monitor** (any baud, it's ignored).

The discovery reads `%APPDATA%\cyd-lab\running.json`. If you start cyd-lab with a custom
`CYD_LAB_HOME`, the IDE has to inherit the same variable.

### Do I need a COM driver? (ESPHome, OpenHASP, Arduino)

* **Arduino IDE 2 / arduino-cli:** no. See above. A COM driver would only reproduce what
  the network port already does, with more to install and nothing gained.
* **Web installers (openHASP's "Install" button, WLED, ESPHome Web):** the browser's Web
  Serial picker only lists real serial ports, so it won't show the emulated board without
  a COM driver. Use **Install from web installer** with the same manifest URL. That's the
  same install, without the browser.
* **ESPHome in Home Assistant:** HA's "Install" options are Web Serial (as above), a serial
  port on the HA host, or **Manual download**. Use Manual download → "Factory format",
  then cyd-lab's **Import .bin at 0x0** (or esptool to the `rfc2217://` URL). The
  `esphome run --device` CLI treats anything that isn't `/dev/…` or `COMx` as an OTA
  host, so it can't take the URL directly. Untested here.
* **PlatformIO:** `upload_port = rfc2217://127.0.0.1:PORT` and `monitor_port` the same
  (pyserial URL handlers). Untested here.
* **esptool:** `--port rfc2217://127.0.0.1:PORT`, tested.

## Data layout

```
%APPDATA%\cyd-lab\            (Linux: ~/.local/share/cyd-lab; override with CYD_LAB_HOME)
  settings.toml               engine exe, ROM dir, live link, ports, ...
  running.json                powered-on devices + serial ports (read by cyd-lab-discovery)
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
relay started and stopped with the engine), web-installer manifests (real openHASP/ESPHome-style
manifests, erase rules, relative part URLs, a local manifest end to end), `running.json`
(round trip, stale writer), the discovery's port JSON and sync diff, and the monitor's address
parsing, Telnet framing and a two-way bridge. The esptool, web-installer and Arduino flows were
tested by hand under Xvfb.

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
