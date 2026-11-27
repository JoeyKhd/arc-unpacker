# Builds release binaries into .\dist (or -OutDir <folder>)
#   arc-unpack.exe, arc-unpack-gui.exe   self-contained: the helpers below are embedded
#   cls-host-x64.exe, cls-host-x86.exe, unarc-host-x86.exe   the helpers on their own (for development)
#   zz-arc-unpack-timer.dll   the xtool timer plugin (also embedded)
param([string]$OutDir = 'dist')
# Exit codes are checked explicitly: under Windows PowerShell 5.1, ErrorActionPreference=Stop
# would turn cargo's progress messages on stderr into terminating errors.
Set-Location $PSScriptRoot
$td = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }

function Invoke-Cargo {
    & cargo @args
    if ($LASTEXITCODE -ne 0) { throw "cargo $args failed" }
}

if (-not (rustup target list --installed | Select-String -SimpleMatch 'i686-pc-windows-msvc')) {
    & rustup target add i686-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "rustup target add failed" }
}

# 1. Helpers (32-bit hosts for 32-bit codec DLLs, 64-bit host for 64-bit ones).
Invoke-Cargo build --release --bin cls-host
Invoke-Cargo build --release --target i686-pc-windows-msvc --bin cls-host --bin unarc-host

$embed = Join-Path $PSScriptRoot "$td\embed"
New-Item -ItemType Directory -Force $embed -ErrorAction Stop | Out-Null
Copy-Item $td\release\cls-host.exe "$embed\cls-host-x64.exe" -Force -ErrorAction Stop
Copy-Item $td\i686-pc-windows-msvc\release\cls-host.exe "$embed\cls-host-x86.exe" -Force -ErrorAction Stop
Copy-Item $td\i686-pc-windows-msvc\release\unarc-host.exe "$embed\unarc-host-x86.exe" -Force -ErrorAction Stop

# The uninstaller every install puts in the game folder, and the stand-in for repack tools
# arc-unpack knows exactly (src/tools.rs).
Invoke-Cargo build --release --bin arc-unpack-uninstall --bin arc-unpack-tool
Copy-Item $td\release\arc-unpack-uninstall.exe "$embed\arc-unpack-uninstall.exe" -Force -ErrorAction Stop
Copy-Item $td\release\arc-unpack-tool.exe "$embed\arc-unpack-tool.exe" -Force -ErrorAction Stop

# The xtool timer plugin (its own tiny crate; 64-bit, like the xtool builds it is for).
Invoke-Cargo build --release --manifest-path xtool-timer\Cargo.toml --target-dir "$td\xtool-timer"
Copy-Item "$td\xtool-timer\release\xtool_timer.dll" "$embed\zz-arc-unpack-timer.dll" -Force -ErrorAction Stop

# 2. Main programs with the helpers embedded.
$env:ARC_UNPACK_EMBED_DIR = $embed.Replace('\', '/')
Invoke-Cargo build --release --features embed-helpers --bin arc-unpack --bin arc-unpack-gui
Remove-Item Env:ARC_UNPACK_EMBED_DIR

New-Item -ItemType Directory -Force $OutDir -ErrorAction Stop | Out-Null
Copy-Item $td\release\arc-unpack.exe $OutDir -Force -ErrorAction Stop
Copy-Item $td\release\arc-unpack-gui.exe $OutDir -Force -ErrorAction Stop
Copy-Item "$embed\*.exe" $OutDir -Force -ErrorAction Stop
Copy-Item "$embed\*.dll" $OutDir -Force -ErrorAction Stop
Write-Host "Built into $OutDir"
