cyd-lab: emulated Cheap Yellow Display (ESP32-2432S028R) boards. No drivers, no install, no admin.
1. Run cyd-lab.exe. The first start downloads Espressif's ESP32 ROM once (5 MB). If Windows says "protected your PC": More info > Run anyway.
2. Click "+ New device", then "Power on". A blank board prints "invalid header" in the console. That is normal.
3. Drag a firmware .bin onto the window (a merged/factory image: ESPHome firmware.factory.bin, Arduino <sketch>.ino.merged.bin; or a build's bootloader + partitions + app together), or paste a web-flasher manifest URL into "Install from web".
Arduino IDE 2: click "Set up Arduino IDE", restart the IDE, pick the board and Tools > Port > "<device> (cyd-lab)", then Upload.
esptool, PlatformIO, ESPHome: the "Use with..." button next to the serial URL has copy-paste commands.
Wi-Fi / Home Assistant: in the device's Network section enter the Wi-Fi name and password your firmware uses (stored in plain text), add the ESPHome preset, and use this PC's IP. Windows Firewall asks once about esp32sim: tick Private networks, Allow.
Portable: put an empty file named "portable" next to cyd-lab.exe and all data stays in .\data.
Licences: LICENSES\ folder. Source and help: https://github.com/gaza1994/CYD_EMU
