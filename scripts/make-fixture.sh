#!/usr/bin/env bash
# Build a throwaway "developer home directory" so sweep has something
# interesting (and completely disposable) to find.
#
# 13 projects across 9 ecosystems, artifact directories padded with real bytes,
# source files backdated so staleness varies from days to years, plus a set of
# decoys that look like artifacts but must NOT be matched.
set -euo pipefail

FIXTURE="${1:-/private/tmp/claude-501/-Users-neelbarmecha-LINKEDIN-PROJECTS/bc69f789-0768-424f-9c98-50960cd19268/scratchpad/sweep-fixture}"

rm -rf "$FIXTURE"
mkdir -p "$FIXTURE"

# pad <path> <megabytes> -- create a file of real (non-sparse) bytes.
pad() {
  local path="$1" mb="$2"
  mkdir -p "$(dirname "$path")"
  if command -v mkfile >/dev/null 2>&1; then
    mkfile "${mb}m" "$path"
  else
    dd if=/dev/zero of="$path" bs=1048576 count="$mb" status=none
  fi
}

# fill <dir> <megabytes> <nfiles> -- spread bytes over a few plausible files.
fill() {
  local dir="$1" mb="$2" n="${3:-4}"
  local each=$(( mb / n )); [ "$each" -lt 1 ] && each=1
  for i in $(seq 1 "$n"); do
    pad "$dir/chunk-$i.bin" "$each"
  done
}

# age <stamp> <paths...> -- backdate top-level project entries.
age() {
  local stamp="$1"; shift
  touch -t "$stamp" "$@" 2>/dev/null || true
}

say() { printf '  %-22s %s\n' "$1" "$2"; }
echo "building fixture in $FIXTURE"

# --- 1. Next.js app: node_modules + .next + a gitignored dist -----------------
P="$FIXTURE/web-dashboard"
mkdir -p "$P/src"
echo '{"name":"web-dashboard","version":"1.0.0"}' > "$P/package.json"
printf 'node_modules\n.next\ndist\n' > "$P/.gitignore"
echo 'export default function App(){}' > "$P/src/app.tsx"
fill "$P/node_modules/react" 6 3
fill "$P/node_modules/typescript" 6 3
fill "$P/.next/cache" 4 2
fill "$P/dist" 3 2
mkdir -p "$P/.git" && echo 'ref: refs/heads/main' > "$P/.git/HEAD"
age 202403101200 "$P/package.json" "$P/src" "$P/src/app.tsx" "$P/.gitignore" "$P/.git/HEAD"
say web-dashboard "node_modules + .next + dist (gitignored)"

# --- 2. Marketing site: node_modules + .turbo --------------------------------
P="$FIXTURE/marketing-site"
mkdir -p "$P/pages"
echo '{"name":"marketing-site"}' > "$P/package.json"
echo '<h1>hi</h1>' > "$P/pages/index.html"
fill "$P/node_modules/next" 6 3
fill "$P/.turbo" 2 2
age 202506201200 "$P/package.json" "$P/pages" "$P/pages/index.html"
say marketing-site "node_modules + .turbo (recent)"

# --- 3 & 4. Rust ------------------------------------------------------------
P="$FIXTURE/old-cli"
mkdir -p "$P/src"
printf '[package]\nname = "old-cli"\n' > "$P/Cargo.toml"
echo 'fn main(){}' > "$P/src/main.rs"
fill "$P/target/debug/deps" 14 4
fill "$P/target/release/deps" 6 2
age 202311050900 "$P/Cargo.toml" "$P/src" "$P/src/main.rs"
say old-cli "target/ 20MB, untouched since 2023"

P="$FIXTURE/wasm-experiment"
mkdir -p "$P/src"
printf '[package]\nname = "wasm-experiment"\n' > "$P/Cargo.toml"
echo 'fn main(){}' > "$P/src/lib.rs"
fill "$P/target/wasm32-unknown-unknown" 8 3
age 202501150900 "$P/Cargo.toml" "$P/src" "$P/src/lib.rs"
say wasm-experiment "target/ 8MB"

# --- 5 & 6. Python ----------------------------------------------------------
P="$FIXTURE/ml-notebook"
mkdir -p "$P/notebooks"
echo 'torch\nnumpy\n' > "$P/requirements.txt"
echo '{}' > "$P/notebooks/train.ipynb"
mkdir -p "$P/.venv"
printf 'home = /usr/local/bin\nversion = 3.11.6\n' > "$P/.venv/pyvenv.cfg"
fill "$P/.venv/lib/python3.11/site-packages" 15 5
fill "$P/__pycache__" 1 1
fill "$P/.pytest_cache" 1 1
age 202404220900 "$P/requirements.txt" "$P/notebooks" "$P/notebooks/train.ipynb"
say ml-notebook ".venv 15MB + __pycache__ + .pytest_cache"

P="$FIXTURE/scraper"
mkdir -p "$P/src"
echo 'requests\n' > "$P/requirements.txt"
echo 'import requests' > "$P/src/main.py"
mkdir -p "$P/.venv" && printf 'home = /usr/bin\n' > "$P/.venv/pyvenv.cfg"
fill "$P/.venv/lib" 5 2
fill "$P/.mypy_cache" 2 2
fill "$P/.ruff_cache" 1 1
age 202208140900 "$P/requirements.txt" "$P/src" "$P/src/main.py"
say scraper ".venv + mypy/ruff caches, 2022"

# --- 7. Android / Gradle ----------------------------------------------------
P="$FIXTURE/android-app"
mkdir -p "$P/app/src"
echo 'plugins { id("com.android.application") }' > "$P/build.gradle.kts"
echo 'class Main' > "$P/app/src/Main.kt"
fill "$P/build/outputs" 7 3
fill "$P/.gradle/caches" 3 2
age 202409010900 "$P/build.gradle.kts" "$P/app"
say android-app "build/ + .gradle/"

# --- 8. iOS / CocoaPods -----------------------------------------------------
P="$FIXTURE/ios-app"
mkdir -p "$P/Sources"
echo "platform :ios, '16.0'" > "$P/Podfile"
echo 'import UIKit' > "$P/Sources/App.swift"
fill "$P/Pods/Alamofire" 5 2
fill "$P/Pods/SnapKit" 4 2
age 202312200900 "$P/Podfile" "$P/Sources" "$P/Sources/App.swift"
say ios-app "Pods/ 9MB"

# --- 9. Flutter -------------------------------------------------------------
P="$FIXTURE/flutter-app"
mkdir -p "$P/lib"
printf 'name: flutter_app\n' > "$P/pubspec.yaml"
echo 'void main(){}' > "$P/lib/main.dart"
fill "$P/.dart_tool" 4 2
fill "$P/build/app" 6 2
age 202502110900 "$P/pubspec.yaml" "$P/lib" "$P/lib/main.dart"
say flutter-app ".dart_tool + build/"

# --- 10. Go -----------------------------------------------------------------
P="$FIXTURE/go-service"
mkdir -p "$P/cmd"
printf 'module example.com/svc\n\ngo 1.22\n' > "$P/go.mod"
printf 'vendor/\n' > "$P/.gitignore"
echo 'package main' > "$P/cmd/main.go"
fill "$P/vendor/github.com" 5 3
age 202407190900 "$P/go.mod" "$P/cmd" "$P/.gitignore" "$P/cmd/main.go"
say go-service "vendor/ (gitignored)"

# --- 11. Zig ----------------------------------------------------------------
P="$FIXTURE/zig-toy"
mkdir -p "$P/src"
echo 'const std = @import("std");' > "$P/build.zig"
echo 'pub fn main() void {}' > "$P/src/main.zig"
fill "$P/zig-cache/o" 3 2
fill "$P/zig-out/bin" 1 1
age 202410280900 "$P/build.zig" "$P/src" "$P/src/main.zig"
say zig-toy "zig-cache + zig-out"

# --- 12. Terraform ----------------------------------------------------------
P="$FIXTURE/infra"
mkdir -p "$P"
printf 'provider "aws" {}\n' > "$P/main.tf"
fill "$P/.terraform/providers" 6 2
age 202405060900 "$P/main.tf"
say infra ".terraform/ 6MB"

# --- 13. Decoys: none of these may be reported ------------------------------
P="$FIXTURE/decoys"
mkdir -p "$P"
#   a `target` with no Cargo.toml (a designer's export folder)
fill "$P/design-exports/target" 2 1
#   a `dist` with a package.json but NOT gitignored (checked-in output)
mkdir -p "$P/vendored-lib"
echo '{"name":"vendored-lib"}' > "$P/vendored-lib/package.json"
fill "$P/vendored-lib/dist" 2 1
#   an `env` directory of config, with no pyvenv.cfg
mkdir -p "$P/config-repo/env"
echo 'KEY=value' > "$P/config-repo/env/prod.env"
#   a node_modules with no package.json beside it
fill "$P/orphan/node_modules" 2 1
#   a symlink pointing at a real artifact: must never be followed or counted
ln -s "$FIXTURE/old-cli/target" "$P/link-to-target"
say decoys "5 look-alikes that must NOT be matched"

echo
echo "fixture ready: $FIXTURE"
du -sh "$FIXTURE" 2>/dev/null | awk '{print "  total on disk: " $1}'
