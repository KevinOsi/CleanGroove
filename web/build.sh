#!/bin/sh
# Builds the web demo into web/: the engine as WebAssembly, plus the icon.
#   rustup target add wasm32-unknown-unknown   (once)
#   sh web/build.sh            (Windows PowerShell: webuild.ps1)
# Then serve the web/ folder with any static server, for example:
#   python3 -m http.server -d web 8000
set -e
cd "$(dirname "$0")/.."
cargo build -p cr-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/cr_wasm.wasm web/cr_wasm.wasm
cp crates/cr-gui/assets/icon_256.png web/icon_256.png
ls -l web/cr_wasm.wasm
