Generic layout cyd-lab uses for "Install anyway" when an app-only image is dropped onto a
blank board (no bootloader, no partition table):

bootloader.bin              ESP-IDF v5.5.5 second-stage bootloader for the classic ESP32
                            (DIO, 4 MB), as shipped by the Arduino-ESP32 core 3.3.12
                            (esp32-libs) and exported by arduino-cli --export-binaries.
                            ESP-IDF is Apache-2.0 (see NOTICE.txt).
partitions-default-4mb.bin  Arduino-ESP32's "default" 4 MB table (partitions-default-4mb.csv):
                            nvs 0x9000/0x5000, otadata 0xe000/0x2000, app0 0x10000/0x140000,
                            app1 0x150000/0x140000, spiffs 0x290000/0x160000,
                            coredump 0x3F0000/0x10000.

This may differ from the layout the firmware was built for. The merged/factory image is
the like-for-like choice.
