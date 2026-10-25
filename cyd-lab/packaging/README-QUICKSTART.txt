cyd-lab: emulated Cheap Yellow Display (ESP32-2432S028R) boards. No drivers, no install, no admin.
1. Run cyd-lab.exe. The first start downloads Espressif's ESP32 ROM once (5 MB). If Windows says "protected your PC": More info > Run anyway.
2. Click "+ New device", then "Power on". A blank board prints "invalid header" in the console. That is normal.
3. Click "Install firmware..." and pick one from the gallery (ESPHome for Home Assistant with screen, openHASP, NerdMiner, Bruce, ...): it downloads, checks and writes it, then boots. Or drag your own firmware .bin onto the window (a merged/factory image: ESPHome firmware.factory.bin, Arduino <sketch>.ino.merged.bin; or a build's bootloader + partitions + app together), or click the firmware tag under the buttons for "Flash a .bin...", "Install from a web flasher" and "Write at an offset".
Arduino IDE 2: click "Set up Arduino IDE", restart the IDE, pick the board and Tools > Port > "<device> (cyd-lab)", then Upload.
esptool, PlatformIO, ESPHome: the "Use with..." button next to the serial URL has copy-paste commands.
Status tags: the row under the buttons (Firmware, Wi-Fi, HA, Serial, and Hotspot while the device has one) opens each setting on a click; grey means fine, amber needs a look.
Download mode: one click holds BOOT, pulses EN and lets go, for esptool. "Hold BOOT" latches BOOT down until you click it again.
Wi-Fi setup: when firmware is waiting for Wi-Fi (Improv, or its own setup hotspot), a banner under the tags shows "Send Wi-Fi details": it sends the device's emulated Wi-Fi name and password.
Wi-Fi / Home Assistant: click the Wi-Fi tag and enter the Wi-Fi name and password your firmware uses (stored in plain text), then the HA tag: add the ESPHome preset, and use this PC's IP. Forwarded web pages open in your default browser at http://127.0.0.1:<port>/. Windows Firewall asks once about esp32sim: tick Private networks, Allow.
Portable: put an empty file named "portable" next to cyd-lab.exe and all data stays in .\data.
Licences: LICENSES\ folder. Source and help: https://github.com/gaza1994/CYD_EMU
