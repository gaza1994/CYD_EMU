#!/usr/bin/env python3
"""Rebuild the bundled "ESPHome for Home Assistant (with screen)" image.

Compiles firmware/esphome-ha/cyd-2432s028r.yaml with the `esphome` on PATH (or
--esphome), copies firmware.factory.bin to assets/firmware/ and updates its sha256
and size in assets/cyd-lab-catalog.toml. Then rebuild cyd-lab.
"""
import argparse, hashlib, os, re, shutil, subprocess

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
YAML = os.path.join(HERE, "firmware", "esphome-ha", "cyd-2432s028r.yaml")
OUT = os.path.join(HERE, "assets", "firmware", "esphome-ha-cyd-2432s028r.factory.bin")
OVERLAY = os.path.join(HERE, "assets", "cyd-lab-catalog.toml")

ap = argparse.ArgumentParser()
ap.add_argument("--esphome", default="esphome")
a = ap.parse_args()
subprocess.run([a.esphome, "compile", YAML], check=True)
built = os.path.join(HERE, "firmware", "esphome-ha", ".esphome", "build", "cyd-panel", "build", "firmware.factory.bin")
shutil.copyfile(built, OUT)
data = open(OUT, "rb").read()
sha = hashlib.sha256(data).hexdigest()
text = open(OVERLAY).read()
block = re.search(r'type = "bundled"\n(?:.*\n)*?(?=\n|\[)', text)
assert block, "bundled source not found in the overlay"
b = block.group(0)
nb = re.sub(r'sha256 = "[0-9a-f]+"', f'sha256 = "{sha}"', b)
nb = re.sub(r"size = \d+", f"size = {len(data)}", nb)
open(OVERLAY, "w").write(text.replace(b, nb))
print(f"{OUT}: {len(data)} bytes, sha256 {sha}")
