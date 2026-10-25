# Tasmota32 (LVGL build) on ESP32-2432S028R

1. Flash `tasmota32-lvgl.factory.bin` @0x0 (manifest entry `tasmota32-lvgl`).
2. Join the `tasmota-XXXXXX` AP, browse to 192.168.4.1, set Wi-Fi.
3. Console -> apply the template (source: templates.blakadder.com/sunton_ESP32-2432S028.html):

```
Backlog Template {"NAME":"ESP32-2432S028","GPIO":[6210,1,800,0,448,0,1,1,672,704,736,768,449,450,1,1,0,992,1,1,0,737,480,1,0,0,0,0,705,769,4768,1,0,0,0,673],"FLAG":0,"BASE":1}; Module 0
```

4. Tools -> Manage File System -> upload `tasmota-cyd-display.ini` renamed to `display.ini`, then `Restart 1`.
   The 2-USB (ST7789) board uses the template on templates.blakadder.com/cyd.html instead.
