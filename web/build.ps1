# Builds the web demo into web\: the engine as WebAssembly, plus the icon.
# The Windows PowerShell twin of web/build.sh.
#   rustup target add wasm32-unknown-unknown   (once)
#   powershell -ExecutionPolicy Bypass -File web\build.ps1
# Then serve the web folder with any static server, for example:
#   python -m http.server -d web 8000
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
cargo build -p cr-wasm --target wasm32-unknown-unknown --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item 'target\wasm32-unknown-unknown\release\cr_wasm.wasm' 'web\cr_wasm.wasm' -Force
Copy-Item 'crates\cr-gui\assets\icon_256.png' 'web\icon_256.png' -Force
Get-Item 'web\cr_wasm.wasm' | Select-Object Name, Length, LastWriteTime
