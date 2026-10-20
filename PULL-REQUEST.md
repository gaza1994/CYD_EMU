# `--flash-file`: a flash image that survives the run

A run that loads a flash image keeps nothing of what the guest writes there. NVS (the guest's own
settings, WiFi credentials), `otadata`, calibration data and any filesystem live in the same array, so
a firmware configured once came back with its boot defaults on the next run — the "configure once,
then test" workflow did not exist. This adds the array as a file:

```sh
esp32sim ... --flash-file flash.bin        # load it if it holds an image, write it back at exit
esp32sim ... --flash-out flash.bin         # one-shot dump of the final array, nothing loaded
```

## Branch base

Cut from `main` and rebased onto `77e43c20` after the first review round; the only conflict was one
line in the option struct next to the new `--flash-octal`. Independent of the inbound-forward and
join changes; the bench used `--hostfwd` (a different branch) only to reach the guest's page over the
emulated link.

## What it does

`--flash-file F` reads the file at start when it holds an image and writes the whole array back at
exit — after the report, and also when `SIGINT`/`SIGTERM` interrupts the run (exit status 130, the
report says `interrupted by a signal`). Writing goes through a `.<name>.<pid>.tmp` file beside the
target, `sync_all`, then `rename`, so a run killed mid-write leaves the previous image rather than
half of one.

The file is exactly the configured flash size: a longer file is refused (it cannot be a dump of this
chip, and truncating a real dump would drop `otadata` or a filesystem), a shorter one is padded with
`0xff`, what an unwritten cell reads as — zeros would look like programmed data. With `--flash-image`
as well, the file wins once it holds an image and the `[emu] flash:` line says which source won;
`--flash-image` then only seeds a file that does not exist yet. `--bootloader`, `--ptable`, `--app`
and `--flash-at` are written on top of either, so a run can refresh the firmware and keep the guest's
settings.

Saving needs to read the model back, so `SocBus` gains `read_flash(offset, len) -> Option<&[u8]>` and
`flash_len()`, implemented as one-liners for the classic ESP32, S3, C3 and C6; `Machine::read_flash`
and `Machine::flash_len` forward them.

The signal handler only stores an `AtomicBool` (`signal(2)` declared directly; the workspace has no
dependencies) and a run that owns a flash file is split into 16 M-instruction chunks where the flag is
read. A run without `--flash-file` still makes one `Machine::run` call, so instruction counts, console
output and goldens are unchanged.

## Tests and evidence

- Seven unit tests in `esp-soc/src/flash.rs`: round trip at the configured size, `0xff` padding, an
  empty file as a fresh flash, a too-large file refused, a missing file as an error, a replaced image
  leaving no temporary file, and a temporary file from a killed run never read back.
- `cli/tests/flash_file.rs` (8 tests): seed + padding + write-back, a firmware that writes nothing
  leaving the file byte-identical (SHA-256), a file that holds an image winning over `--flash-image`,
  `--flash-out` not being loaded, an oversized file refused, and `SIGTERM` still writing and exiting 130.
  Hermetic: a hand-built C6 app image that spins, no mask ROM, no firmware build.
- `cli/tests/soc_contracts.rs`: `read_flash`/`flash_len` on all four chips, including the bounds rules
  and the `0xff` of an unwritten cell.
- `cargo clippy --workspace --all-targets -- -D warnings` clean; 1021 workspace tests with
  `--include-ignored --skip external_`, 0 failed, run on the rebased tree; goldens bit-identical,
  nothing regenerated. (The `external_*` tests need a PlatformIO build directory and are skipped as
  documented.)

End to end on an unmodified released ESP32-S3 image, two runs of one build: the guest booted in
station mode from a configured flash file, its own page saved `STANAME=bench-two` (`POST /_kfg_`), and
the next run — the file plus `--wifi ssid=bench-two` and nothing else — associated to `bench-two`,
took a lease at `10.0.2.15`, completed the WPA2 handshake and served its page through `--hostfwd`.
The file was 16 777 216 bytes at every step; NVS held 2 917 bytes not `0xff`.

A second flow drove the guest's file manager over the emulated link: four files with two
subdirectories, the tree read back and packed into a zip blob, then a run with a fresh, empty
filesystem restored from that blob and returned every file byte-identically.

[Receipt, commands, hashes and limits](docs/evidence/persistent-flash/README.md). The experiment
catalog entry is EX233.
