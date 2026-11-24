# Builds arc-unpack plus the 32-bit and 64-bit CLS hosts into .\dist
# Exit codes are checked explicitly: under Windows PowerShell 5.1, ErrorActionPreference=Stop
# would turn cargo's progress messages on stderr into terminating errors.
Set-Location $PSScriptRoot

function Invoke-Cargo {
    & cargo @args
    if ($LASTEXITCODE -ne 0) { throw "cargo $args failed" }
}

if (-not (rustup target list --installed | Select-String -SimpleMatch 'i686-pc-windows-msvc')) {
    & rustup target add i686-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "rustup target add failed" }
}

Invoke-Cargo build --release --bin arc-unpack --bin arc-unpack-gui --bin cls-host
Invoke-Cargo build --release --target i686-pc-windows-msvc --bin cls-host --bin unarc-host

New-Item -ItemType Directory -Force dist -ErrorAction Stop | Out-Null
Copy-Item target\release\arc-unpack.exe dist\ -Force -ErrorAction Stop
Copy-Item target\release\arc-unpack-gui.exe dist\ -Force -ErrorAction Stop
Copy-Item target\release\cls-host.exe dist\cls-host-x64.exe -Force -ErrorAction Stop
Copy-Item target\i686-pc-windows-msvc\release\cls-host.exe dist\cls-host-x86.exe -Force -ErrorAction Stop
Copy-Item target\i686-pc-windows-msvc\release\unarc-host.exe dist\unarc-host-x86.exe -Force -ErrorAction Stop
Write-Host "Built into $PSScriptRoot\dist"
