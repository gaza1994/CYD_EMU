#!/usr/bin/env python3
"""Assemble the Windows release zip (no dependencies beyond Python 3 and cargo).

    python tools/package_windows.py --bin-dir target/x86_64-pc-windows-gnu/release \
        --engine ../esp32sim-applab/target/x86_64-pc-windows-gnu/release/esp32sim.exe \
        --engine-src ../esp32sim-applab --out ../release/cyd-lab-windows-x64.zip

Layout inside the zip:
    cyd-lab/cyd-lab.exe, esp32sim.exe, README-QUICKSTART.txt
    cyd-lab/arduino/tools/cyd-lab-discovery.exe, cyd-lab-monitor.exe
    cyd-lab/arduino/examples/CydHello/
    cyd-lab/LICENSES/...
The mask ROM is not shipped: cyd-lab downloads Espressif's pinned esp-rom-elfs
release on first run and checks its SHA-256.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, zipfile

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # cyd-lab repo


def third_party(manifest, target):
    """Name, version, licence and authors of every crate linked into the Windows build."""
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked",
                          "--filter-platform", target, "--manifest-path", manifest],
                         check=True, capture_output=True, text=True).stdout
    meta = json.loads(out)
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    pkgs = {p["id"]: p for p in meta["packages"]}
    want = os.path.realpath(manifest)
    root = next(p["id"] for p in meta["packages"] if os.path.realpath(p["manifest_path"]) == want)
    seen, stack = set(), [root]
    while stack:  # normal (non-dev, non-build) dependency closure
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]["deps"]:
            if any(k["kind"] is None for k in d["dep_kinds"]):
                stack.append(d["pkg"])
    rows = []
    for i in seen:
        p = pkgs[i]
        if p["source"] is None:  # the workspace's own crates
            continue
        rows.append((p["name"], p["version"], p.get("license") or "see crate", ", ".join(p.get("authors") or [])))
    return sorted(set(rows))


def write_third_party(path, title, rows):
    with open(path, "w", encoding="utf-8", newline="\r\n") as f:
        f.write(f"Third-party Rust crates compiled into {title}\n")
        if not rows:
            f.write("\nNone: it uses only the Rust standard library (MIT OR Apache-2.0).\n")
            return
        f.write("Each crate's full licence text is in its source package on crates.io.\n\n")
        for name, ver, lic, authors in rows:
            f.write(f"{name} {ver}  [{lic}]" + (f"  {authors}" if authors else "") + "\n")


def crlf_copy(src, dst):
    with open(src, encoding="utf-8") as a, open(dst, "w", encoding="utf-8", newline="\r\n") as b:
        b.write(a.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin-dir", required=True)
    ap.add_argument("--engine", required=True)
    ap.add_argument("--engine-src", required=True, help="esp32sim checkout (LICENSE, Cargo.toml)")
    ap.add_argument("--target", default="x86_64-pc-windows-gnu")
    ap.add_argument("--out", required=True)
    ap.add_argument("--strip", default="", help="strip tool to run on the exes (optional)")
    a = ap.parse_args()

    stage = tempfile.mkdtemp(prefix="cydlab-pkg-")
    top = os.path.join(stage, "cyd-lab")
    tools = os.path.join(top, "arduino", "tools")
    lic = os.path.join(top, "LICENSES")
    os.makedirs(tools)
    os.makedirs(lic)
    exes = [(os.path.join(a.bin_dir, "cyd-lab.exe"), top), (a.engine, top),
            (os.path.join(a.bin_dir, "cyd-lab-discovery.exe"), tools),
            (os.path.join(a.bin_dir, "cyd-lab-monitor.exe"), tools)]
    for src, d in exes:
        dst = shutil.copy2(src, d)
        if a.strip:
            subprocess.run([a.strip, dst], check=True)
    shutil.copytree(os.path.join(HERE, "arduino", "examples"), os.path.join(top, "arduino", "examples"))
    crlf_copy(os.path.join(HERE, "packaging", "README-QUICKSTART.txt"), os.path.join(top, "README-QUICKSTART.txt"))

    crlf_copy(os.path.join(HERE, "LICENSE"), os.path.join(lic, "cyd-lab-LICENSE.txt"))
    crlf_copy(os.path.join(a.engine_src, "LICENSE"), os.path.join(lic, "esp32sim-LICENSE.txt"))
    crlf_copy(os.path.join(HERE, "packaging", "NOTICE.txt"), os.path.join(lic, "NOTICE.txt"))
    write_third_party(os.path.join(lic, "third-party-cyd-lab.txt"), "cyd-lab.exe and the Arduino tools",
                      third_party(os.path.join(HERE, "Cargo.toml"), a.target))
    write_third_party(os.path.join(lic, "third-party-esp32sim.txt"), "esp32sim.exe",
                      third_party(os.path.join(a.engine_src, "cli", "Cargo.toml"), a.target))

    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    if os.path.exists(a.out):
        os.remove(a.out)
    with zipfile.ZipFile(a.out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for root, _, files in os.walk(stage):
            for fn in sorted(files):
                full = os.path.join(root, fn)
                z.write(full, os.path.relpath(full, stage).replace(os.sep, "/"))
    shutil.rmtree(stage)
    print(f"{a.out}: {os.path.getsize(a.out)} bytes")


if __name__ == "__main__":
    sys.exit(main())
