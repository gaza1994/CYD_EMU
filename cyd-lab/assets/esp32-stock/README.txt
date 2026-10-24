Stock ESP-IDF v5.5.6 second-stage bootloader (classic ESP32, DIO 40 MHz, 4 MB) and
ESPHome 2026.10.0's default 4 MB partition table (otadata 0x9000, phy_init 0xb000,
app0/ota_0 0x10000, app1/ota_1 0x1d0000, nvs 0x390000), exactly as ESPHome generated
them for an esp32dev build (they are the 0x1000 and 0x8000 parts of its
firmware.factory.bin). cyd-lab only uses them for "Install anyway" when someone drops
an app-only (OTA format) image onto a blank board. ESP-IDF is Apache-2.0.
