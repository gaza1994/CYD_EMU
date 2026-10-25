# Board profiles (classic ESP32 SPI-display boards)

The "Cheap Yellow Display" family and its cousins share one model, `GenericDisplay`
(`esp32/src/board/cyd.rs`). A small TOML profile (`esp32/src/board/profiles/*.toml`) gives
each board its panel, touch controller, backlight, RGB LED and the pins the report lists.
Supporting a new SPI-panel board means writing a profile; it needs no Rust.

```sh
esp32sim --chip esp32 --board cyd-2432s032c ...          # a built-in profile, by name or alias
esp32sim --chip esp32 --board-config myboard.toml ...    # your own (also: --board generic-display --board-config F)
esp32sim --list-boards                                   # the built-ins as JSON, for front ends
```

`--list-boards` prints one object per profile:
`{"name","aliases","chip","description","controller","width","height","touch","rgb_led","sources"}`.
`touch` is `xpt2046`, `gt911`, `cst816` or `none`. A front end such as a "new device" dialog
can list these instead of hard-coding board names.

## Built-in profiles

`width`×`height` is the native glass, before any rotation the firmware applies. Each profile
was tested with an ESPHome 2026.10.0 LVGL 3×3 grid at native orientation. Every cell's centre
was tapped through the web UI. The configs are in `cyd-emulator/variants/esphome/`, the
screenshots in `cyd-emulator/variants/`.

| `--board` | Panel | Glass | Touch | Backlight | Pinout source | Display | Touch (9 taps) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `cyd-2432s028r` (`cyd`) | ILI9341 TN | 240×320 | XPT2046, own bus 25/32/39, CS 33, IRQ 36 | 21 | ESP32-Cheap-Yellow-Display PINS.md, Sunton PlatformIO board JSON esp32-2432S028R | upright, MADCTL 0x48 | 9/9, error 0 px |
| `cyd-2432s028r-2usb` (`cyd-2usb`) | ILI9341-compatible | 320×240 | XPT2046, own bus, X/Y swapped | 21 (LEDC PWM) | Sunton PlatformIO board JSON esp32-2432S028Rv3, an owner's unit | upright with LVGL rotation 180 | 9/9, error ≤1 px (owner's unmodified firmware) |
| `cyd-2432s024r` | ILI9341 | 240×320 | XPT2046 on the panel's bus 14/13/12, CS 33, IRQ 36 | 27 | Sunton PlatformIO board JSON esp32-2432S024R | upright, 0x48 | 9/9, 0 px |
| `cyd-2432s024c` | ILI9341 | 240×320 | CST816S (I2C 33/32 @0x15, INT 21, RST 25) | 27 | Sunton PlatformIO board JSON esp32-2432S024C | upright, 0x48 | 9/9, 1 px (driver scaling) |
| `cyd-2432s032r` | ST7789 IPS | 240×320 | XPT2046 on the panel's bus | 27 | Sunton PlatformIO board JSON esp32-2432S032R | upright, true colours (INVON, RGB) | 9/9, 0 px |
| `cyd-2432s032c` | ST7789 IPS | 240×320 | GT911 (I2C 33/32 @0x5d/0x14, INT 21, RST 25) | 27 | Sunton PlatformIO board JSON esp32-2432S032C | upright, true colours | 9/9, 1 px |
| `cyd-3248s035r` | ST7796 | 320×480 | XPT2046 on the panel's bus | 27 | Sunton PlatformIO board JSON esp32-3248S035R | upright, 0x48 | 9/9, 0 px |
| `cyd-3248s035c` | ST7796 | 320×480 | GT911 | 27 | Sunton PlatformIO board JSON esp32-3248S035C | upright, 0x48 | 9/9, 1 px |

The Sunton PlatformIO board JSONs are at `https://github.com/rzeldent/platformio-espressif32-sunton`;
ESP32-Cheap-Yellow-Display PINS.md is at `https://github.com/witnessmenow/ESP32-Cheap-Yellow-Display`.
Each profile's `sources` key carries its exact URLs.

All profiles share SPI SCLK 14, MOSI 13, MISO 12, CS 15 and DC 2 for the panel; the common-anode
RGB LED on 4/16/17; BOOT on 0; the LDR on 34; the TF card on 18/23/19/5; and the speaker on 26.
The TF card and speaker are listed in the report but not modelled.

### Batch variation

Sunton changes parts between batches without changing the model number. The two-USB 2432S028R
is the main example: some write-ups say it uses an ST7789, but the unit this profile was
calibrated on is ILI9341-driven, with native 320×240 glass. ESPHome drives it as `model: ILI9341`,
`dimensions: 320x240`, LVGL `rotation: 180`, MADCTL 0x48. If your board's colours are inverted
or its image is mirrored, copy the profile and change `display.controller`, `display.ips`,
`display.bgr` or `display.mirror_x`/`flip_y`. If touches land off target, change `touch.raw_x`
and `raw_y`.

## Keys

```toml
name = "my-board"                 # required; the --board name
aliases = ["mine"]
chip = "esp32"                    # profiles are for the classic ESP32; others are rejected
description = "..."
sources = ["https://..."]         # where the pinout came from

[display]
controller = "ili9341"            # st7735, st7789, ili9341, ili9342 (320x240 RAM), st7796, ili9488, gc9a01
width = 240                       # glass; default the controller's RAM size
height = 320
offset = [0, 0]                   # glass origin in controller RAM (e.g. 135x240 ST7789 at [52, 40])
mirror_x = false                  # glass column x shows RAM column cols-1-x
flip_y = false                    # glass row y shows RAM row rows-1-y
scan_control = false              # the controller's SS/GS scan bits undo the mirror (ST7796-style)
bgr = false                       # glass is BGR: red and blue swap unless MADCTL's BGR bit matches
ips = false                       # IPS: colours are true only after INVON (0x21)
sclk = 14                         # required, as are mosi, cs, dc
mosi = 13
miso = 12
cs = 15
dc = 2
rst = 4                           # optional
backlight = 21                    # optional; LEDC PWM duty on it dims the picture
backlight_active = "high"         # or "low"

[touch]
controller = "xpt2046"            # xpt2046, gt911, cst820, cst816s, none
sclk = 25                         # xpt2046: sclk, mosi, miso, cs, irq (may equal the display's bus)
mosi = 32
miso = 39
cs = 33
irq = 36
raw_x = [3800, 240]               # raw X reading at the glass's first and last pixel
raw_y = [200, 3700]
swap_xy = false                   # X runs along the glass's y
# I2C controllers instead: sda, scl, address (gt911 0x5d, cst8xx 0x15), int, rst,
#   swap_xy, invert_x, invert_y. GT911 answers at both 0x5d and 0x14, on both I2C ports.

[led]
pins = [4, 16, 17]                # red, green, blue
active = "low"                    # common anode

[board]
boot = 0
ldr = 34
sd = [18, 23, 19, 5]              # listed only
speaker = 26                      # listed only
```

Unknown keys are errors, so typos are caught. `--board-config` checks `chip` against `--chip`.

## How the touch controllers behave

* XPT2046: PENIRQ is held low while the panel is pressed. Conversions return the raw values the
  profile's `raw_x`/`raw_y` ranges imply, so a firmware's own calibration lands on target.
* GT911 and CST816S/CST820 give one short low pulse on INT for every report (press, each move,
  and the release), as the real chips do. ESPHome and most Arduino drivers read on the falling
  edge, so a press held as a level would hide the release. A tap shorter than the guest's
  first read is held until the press has been read; one more pulse then reports the release.

## Not covered yet

* The ESP32-S3 RGB-panel boards (ESP32-8048S043, 8048S050, 8048S070, the 4848S040 with its
  ST7701 init over 3-wire SPI). They need the S3 LCD_CAM RGB mode and a parallel-pixel board
  model, not an SPI DCS panel, so they are a follow-up in `esp32s3`.
* The ESP32-2432S022 (8-bit parallel ST7789) and other i80-bus boards.
* Audio out on GPIO 26 and the TF card.
