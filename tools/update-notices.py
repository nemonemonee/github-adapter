"""Generate dependency notices from the locked Cargo graph and shipped crate texts."""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check", action="store_true")
args = parser.parse_args()
cargo = shutil.which("cargo")
if not cargo:
    suffix = "cargo.exe" if os.name == "nt" else "cargo"
    cargo = str(Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "bin" / suffix)
packages = {}
for target in ("aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"):
    metadata = json.loads(subprocess.check_output([cargo, "metadata", "--locked", "--format-version", "1", "--filter-platform", target], cwd=ROOT))
    graph = {node["id"]: [dep["pkg"] for dep in node["deps"]] for node in metadata["resolve"]["nodes"]}
    reachable = set()
    pending = list(metadata["workspace_members"])
    while pending:
        package_id = pending.pop()
        if package_id not in reachable:
            reachable.add(package_id)
            pending.extend(graph.get(package_id, []))
    packages.update((p["id"], p) for p in metadata["packages"] if p["id"] in reachable and p["source"])
chunks = ["GitHub Adapter — third-party dependency license notices\n\n"
          "Generated from Cargo.lock. Includes supported Windows/macOS targets and development dependencies;\n"
          "a particular platform package may use only a subset. Project license: MIT.\n"]
missing = []
for package in sorted(packages.values(), key=lambda p: (p["name"], p["version"])):
    directory = Path(package["manifest_path"]).parent
    texts = []
    seen = set()
    cached = ROOT / "licenses" / "upstream" / f"{package['name']}-{package['version']}"
    candidates = [(directory, file) for file in sorted(directory.rglob("*"))]
    if cached.is_dir():
        candidates.extend((cached, file) for file in sorted(cached.iterdir()))
    for origin, file in candidates:
        if not file.is_file() or not file.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")):
            continue
        if file.suffix.lower() in (".rs", ".h", ".c", ".html", ".json"):
            continue
        try:
            text = file.read_text(encoding="utf-8-sig").replace("\r\n", "\n").strip()
        except (UnicodeError, OSError):
            continue
        digest = hashlib.sha256(text.encode()).digest()
        if text and digest not in seen:
            seen.add(digest)
            texts.append(f"--- {file.relative_to(origin).as_posix()} ---\n{text}\n")
    if not texts:
        missing.append(f"{package['name']} {package['version']}")
    chunks.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                  f"Declared license: {package.get('license') or 'see upstream'}\n"
                  f"Source: https://crates.io/crates/{package['name']}/{package['version']}\n\n" + "\n".join(texts))
if missing:
    raise SystemExit("No shipped license text found for: " + ", ".join(missing))
result = "\n".join(chunks).encode("utf-8")
output = ROOT / "THIRD_PARTY_LICENSES.txt"
if args.check:
    if not output.exists() or output.read_bytes() != result:
        raise SystemExit("Dependency notices are stale; run python tools/update-notices.py")
else:
    output.write_bytes(result)
print(f"Dependency notices {'verified' if args.check else 'updated'}: {len(result):,} bytes.")
