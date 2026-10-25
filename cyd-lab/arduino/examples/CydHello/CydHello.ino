// CydHello: a minimal ESP32-2432S028R ("CYD") sketch for cyd-lab over the network.
// Draws text with TFT_eSPI, reads touch with XPT2046_Touchscreen (its own SPI bus),
// prints touches and echoes Serial input (try it in the Serial Monitor).
//
// TFT_eSPI needs the CYD pin setup. arduino-cli: pass the build flags in
// ../README.md (compiler.cpp.extra_flags); Arduino IDE: put the same #defines in
// TFT_eSPI's User_Setup.h (see the cyd-lab README).

#include <SPI.h>
#include <TFT_eSPI.h>
#include <XPT2046_Touchscreen.h>

// CYD touch controller: VSPI on IO25/39/32, CS IO33, IRQ IO36.
#define XPT2046_IRQ 36
#define XPT2046_MOSI 32
#define XPT2046_MISO 39
#define XPT2046_CLK 25
#define XPT2046_CS 33

SPIClass touchSpi(VSPI);
XPT2046_Touchscreen ts(XPT2046_CS, XPT2046_IRQ);
TFT_eSPI tft;
uint32_t touches = 0;

void banner() {
  tft.fillScreen(TFT_NAVY);
  tft.setTextColor(TFT_YELLOW, TFT_NAVY);
  tft.setTextDatum(MC_DATUM);
  tft.drawString("Hello from Arduino", 160, 60, 4);
  tft.setTextColor(TFT_WHITE, TFT_NAVY);
  tft.drawString("cyd-lab, no COM driver", 160, 100, 4);
  tft.setTextColor(TFT_CYAN, TFT_NAVY);
  tft.drawString("uploaded over rfc2217://", 160, 135, 2);
  tft.setTextColor(TFT_GREEN, TFT_NAVY);
  tft.drawString("Tap the screen", 160, 200, 2);
}

void setup() {
  Serial.begin(115200);
  touchSpi.begin(XPT2046_CLK, XPT2046_MISO, XPT2046_MOSI, XPT2046_CS);
  ts.begin(touchSpi);
  ts.setRotation(1);
  tft.init();
  tft.setRotation(1);  // landscape 320x240
  banner();
  Serial.println("CydHello: ready (cyd-lab)");
}

void loop() {
  if (ts.tirqTouched() && ts.touched()) {
    TS_Point p = ts.getPoint();
    // Raw 12-bit touch to screen pixels (typical CYD calibration).
    int x = map(p.x, 200, 3700, 0, 320);
    int y = map(p.y, 240, 3800, 0, 240);
    x = constrain(x, 0, 319);
    y = constrain(y, 0, 239);
    touches++;
    tft.fillCircle(x, y, 6, TFT_RED);
    char msg[48];
    snprintf(msg, sizeof msg, "touch %lu: x=%d y=%d (raw %d,%d)", (unsigned long)touches, x, y, p.x, p.y);
    tft.setTextColor(TFT_WHITE, TFT_NAVY);
    tft.setTextDatum(BC_DATUM);
    tft.drawString(String(msg) + "    ", 160, 238, 2);
    Serial.println(msg);
    while (ts.touched()) delay(10);  // one report per tap
  }
  while (Serial.available()) {
    String line = Serial.readStringUntil('\n');
    line.trim();
    if (line.length()) {
      Serial.print("echo: ");
      Serial.println(line);
    }
  }
}
