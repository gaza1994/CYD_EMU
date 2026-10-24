#!/usr/bin/env python3
"""Build the cydlab Arduino package: archives + package_cydlab_index.json.

  python3 arduino/make_package.py --base-url http://127.0.0.1:8765/
  python3 -m http.server -d arduino/dist 8765
  # Board Manager URL: http://127.0.0.1:8765/package_cydlab_index.json

Inputs: cydlab/esp32 (platform.txt, boards.txt) and the release builds of
cyd-lab-discovery / cyd-lab-monitor for Linux x86_64 (target/release) and Windows
x86_64 (target/x86_64-pc-windows-gnu/release). Missing hosts are skipped with a note.
Output: arduino/dist/ (archives + index); the index is also copied to
arduino/package_cydlab_index.json. To publish, upload dist/ somewhere (e.g. a GitHub
release) and regenerate with that --base-url.
"""
import argparse, hashlib, io, json, os, shutil, tarfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
VERSION = "0.1.0"
TOOLS = ["cyd-lab-discovery", "cyd-lab-monitor"]
# Arduino host triplet -> (cargo target dir, executable suffix)
HOSTS = {
    "x86_64-linux-gnu": ("target/release", ""),
    "x86_64-mingw32": ("target/x86_64-pc-windows-gnu/release", ".exe"),
}

def tar_gz(dst, top, files):
    """files: [(arcname under top/, source path or bytes, mode)]"""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.PAX_FORMAT) as t:
        d = tarfile.TarInfo(top); d.type = tarfile.DIRTYPE; d.mode = 0o755; t.addfile(d)
        for name, src, mode in files:
            data = open(src, "rb").read() if isinstance(src, str) else src
            ti = tarfile.TarInfo(f"{top}/{name}"); ti.size = len(data); ti.mode = mode
            t.addfile(ti, io.BytesIO(data))
    open(dst, "wb").write(buf.getvalue())
    return entry(dst)

def entry(path):
    data = open(path, "rb").read()
    return {"archiveFileName": os.path.basename(path), "checksum": "SHA-256:" + hashlib.sha256(data).hexdigest(), "size": str(len(data))}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:8765/")
    a = ap.parse_args()
    base = a.base_url if a.base_url.endswith("/") else a.base_url + "/"
    dist = os.path.join(HERE, "dist")
    shutil.rmtree(dist, ignore_errors=True); os.makedirs(dist)

    plat_dir = os.path.join(HERE, "cydlab", "esp32")
    pfiles = [(f, os.path.join(plat_dir, f), 0o644) for f in sorted(os.listdir(plat_dir)) if f.endswith(".txt")]
    pname = f"cydlab-esp32-{VERSION}.tar.gz"
    pe = tar_gz(os.path.join(dist, pname), f"cydlab-esp32-{VERSION}", pfiles)
    platform = {
        "name": "cyd-lab (emulated CYD boards)", "architecture": "esp32", "version": VERSION, "category": "ESP32",
        "help": {"online": "https://github.com/espressif/arduino-esp32"},
        "url": base + pname, **pe,
        "boards": [{"name": "ESP32-2432S028R CYD (cyd-lab, emulated)"}],
        "toolsDependencies": [],
        "discoveryDependencies": [{"packager": "cydlab", "name": "cyd-lab-discovery"}],
        "monitorDependencies": [{"packager": "cydlab", "name": "cyd-lab-monitor"}],
    }
    tools = []
    for tool in TOOLS:
        systems = []
        for host, (tdir, ext) in HOSTS.items():
            exe = os.path.join(ROOT, tdir, tool + ext)
            if not os.path.exists(exe):
                print(f"skip {tool} for {host}: {exe} not built"); continue
            name = f"{tool}-{VERSION}-{host}.tar.gz"
            e = tar_gz(os.path.join(dist, name), tool, [(tool + ext, exe, 0o755)])
            systems.append({"host": host, "url": base + name, **e})
        tools.append({"name": tool, "version": VERSION, "systems": systems})
    index = {"packages": [{
        "name": "cydlab", "maintainer": "Gareth Johnstone", "websiteURL": "https://github.com/espressif/arduino-esp32",
        "email": "", "help": {"online": "https://github.com/espressif/arduino-esp32"},
        "platforms": [platform], "tools": tools,
    }]}
    text = json.dumps(index, indent=2) + "\n"
    open(os.path.join(dist, "package_cydlab_index.json"), "w").write(text)
    open(os.path.join(HERE, "package_cydlab_index.json"), "w").write(text)
    print(f"wrote {dist} (base URL {base})")

if __name__ == "__main__":
    main()
