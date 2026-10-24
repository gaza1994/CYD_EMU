#!/usr/bin/env python3
"""Rebuild the bundled "ESPHome for Home Assistant (with screen)" images.

Compiles each firmware/esphome-ha/*.yaml below with the `esphome` on PATH (or --esphome),
copies its firmware.factory.bin to assets/firmware/ and updates that file's sha256 and
size in assets/cyd-lab-catalog.toml. Then rebuild cyd-lab.

  cyd-2432s028r.yaml       original micro-USB board (ILI9341 240x320, LVGL rotation 90)
  cyd-2432s028r-2usb.yaml  two-USB board (native 320x240, LVGL rotation 180)
"""
import argparse, hashlib, os, re, shutil, subprocess

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(HERE, "firmware", "esphome-ha")
OVERLAY = os.path.join(HERE, "assets", "cyd-lab-catalog.toml")
# (config, ESPHome build directory under .esphome/build, bundled file name)
IMAGES = [
    ("cyd-2432s028r.yaml", "cyd-panel", "esphome-ha-cyd-2432s028r.factory.bin"),
    ("cyd-2432s028r-2usb.yaml", "cyd-panel-2usb", "esphome-ha-cyd-2432s028r-2usb.factory.bin"),
]

ap = argparse.ArgumentParser()
ap.add_argument("--esphome", default="esphome")
ap.add_argument("--only", help="build just this config")
a = ap.parse_args()
text = open(OVERLAY).read()
for yaml, build, name in IMAGES:
    if a.only and a.only != yaml:
        continue
    subprocess.run([a.esphome, "compile", os.path.join(SRC, yaml)], check=True)
    out = os.path.join(HERE, "assets", "firmware", name)
    shutil.copyfile(os.path.join(SRC, ".esphome", "build", build, "build", "firmware.factory.bin"), out)
    data = open(out, "rb").read()
    sha = hashlib.sha256(data).hexdigest()
    # the bundled source block naming this file: file = "..." then sha256 and size
    pat = re.compile(r'(file = "' + re.escape(name) + r'"\n(?:[^\[\n].*\n)*?sha256 = ")[0-9a-f]+("\nsize = )\d+')
    text, n = pat.subn(lambda m: f"{m.group(1)}{sha}{m.group(2)}{len(data)}", text)
    assert n == 1, f"no bundled source for {name} in the overlay"
    print(f"{out}: {len(data)} bytes, sha256 {sha}")
open(OVERLAY, "w").write(text)
