#!/bin/bash
set -euo pipefail
[[ $(uname -s) == Darwin ]] || { echo 'Package on a native macOS host.' >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd -P)
identity='' notary='' unsigned=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --unsigned) unsigned=true; shift ;;
    --identity) [[ $# -ge 2 ]] || exit 2; identity=$2; shift 2 ;;
    --notary-profile) [[ $# -ge 2 ]] || exit 2; notary=$2; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
if { $unsigned && [[ -n $identity ]]; } || { ! $unsigned && [[ -z $identity ]]; }; then
  echo 'Choose --unsigned or --identity NAME.' >&2; exit 2
fi
[[ -z $notary || -n $identity ]] || { echo 'Notarization requires a Developer ID identity.' >&2; exit 2; }
case "$(uname -m)" in
  arm64) arch=arm64; target=aarch64-apple-darwin; machine=arm64 ;;
  x86_64) arch=x64; target=x86_64-apple-darwin; machine=x86_64 ;;
  *) echo 'Unsupported architecture.' >&2; exit 1 ;;
esac
version=$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)".*/\1/p' "$root/Cargo.toml")
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Invalid workspace version.' >&2; exit 1; }
signing=unsigned
[[ -z $identity ]] || signing=signed
name="github-adapter-$version-macos-$arch-$signing"
mkdir -p "$root/artifacts"
destination="$root/artifacts/$name"
[[ ! -e $destination ]] || { echo "Refusing to replace $destination" >&2; exit 1; }
stage=$(mktemp -d "$root/artifacts/.macos-package.XXXXXX")
cleanup() { case "$stage" in "$root/artifacts/.macos-package."*) rm -rf -- "$stage" ;; esac; }
trap cleanup EXIT
payload="$stage/payload"
app="$payload/GitHub Adapter.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for binary in github-adapter github-adapter-host; do
  source="$root/target/$target/release/$binary"
  [[ -f $source && ! -L $source ]] || { echo "Missing regular executable $source" >&2; exit 1; }
  [[ $(lipo -archs "$source") == "$machine" ]] || { echo 'Mach-O architecture mismatch.' >&2; exit 1; }
  [[ $("$source" --version) == "github-adapter $version" ]] || { echo 'Binary version mismatch.' >&2; exit 1; }
  cp "$source" "$app/Contents/MacOS/$binary"
  chmod 755 "$app/Contents/MacOS/$binary"
done
for asset in app.icns github-adapter-tray.png; do
  cp "$root/assets/$asset" "$app/Contents/Resources/$asset"
done
for notice in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt; do
  cp "$root/$notice" "$app/Contents/Resources/$notice"
done
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>io.github.nemonemonee.github-adapter</string>
<key>CFBundleName</key><string>GitHub Adapter</string>
<key>CFBundleDisplayName</key><string>GitHub Adapter</string>
<key>CFBundleExecutable</key><string>github-adapter-host</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>CFBundleIconFile</key><string>app.icns</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>LSUIElement</key><true/>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$app/Contents/Info.plist"
sign_args=(--force --sign -)
if ! $unsigned; then
  sign_args=(--force --options runtime --timestamp --sign "$identity")
fi
for path in "$app/Contents/MacOS/github-adapter" "$app/Contents/MacOS/github-adapter-host" "$app"; do
  codesign "${sign_args[@]}" "$path"
done
codesign --verify --deep --strict "$app"
if [[ -n $notary ]]; then
  ditto -c -k --keepParent "$app" "$stage/notary.zip"
  xcrun notarytool submit "$stage/notary.zip" --keychain-profile "$notary" --wait
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
fi
python3 - "$payload" "$version" "$arch" "$signing" <<'PY'
import hashlib,json,sys
from pathlib import Path
payload=Path(sys.argv[1]);entries={}
for p in sorted(payload.rglob('*')):
    if p.is_symlink(): raise SystemExit('Package links are unsupported')
    if p.is_file(): entries[p.relative_to(payload).as_posix()]=hashlib.sha256(p.read_bytes()).hexdigest()
(payload/'manifest.json').write_text(json.dumps({'version':sys.argv[2],'architecture':sys.argv[3],'signing':sys.argv[4],'files':entries},indent=2)+'\n')
PY
archive="$stage/$name.zip"
ditto -c -k "$payload" "$archive"
mkdir "$stage/verify"
ditto -x -k "$archive" "$stage/verify"
codesign --verify --deep --strict "$stage/verify/GitHub Adapter.app"
python3 - "$stage/verify" <<'PY'
import hashlib,json,sys
from pathlib import Path
payload=Path(sys.argv[1]);m=json.loads((payload/'manifest.json').read_text())
actual={p.relative_to(payload).as_posix() for p in payload.rglob('*') if p.is_file() and p != payload/'manifest.json'}
if actual != set(m['files']): raise SystemExit('Unexpected package contents')
for rel,digest in m['files'].items():
    p=payload/rel
    if p.is_symlink() or not p.is_file() or hashlib.sha256(p.read_bytes()).hexdigest()!=digest:
        raise SystemExit('Package content mismatch: '+rel)
PY
mkdir "$destination"
mv "$archive" "$destination/"
(cd "$destination" && shasum -a 256 "$name.zip" > "$name.zip.sha256")
printf 'Verified candidate: %s/%s.zip\n' "$destination" "$name"
