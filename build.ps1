# kirinuki build: cargo build, then copy the exe to dist\.
# Machine-specific settings (CARGO_HOME, TEMP, the cargo path...) can go in build.local.ps1 (not in git).
param([switch]$Debug)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$cargo = 'cargo'
$local = Join-Path $PSScriptRoot 'build.local.ps1'
if (Test-Path $local) { . $local }
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot 'target' }
# cargo prints progress on stderr; with 'Stop' PowerShell 5.1 would treat that as an error
$ErrorActionPreference = 'Continue'
if ($Debug) { & $cargo build } else { & $cargo build --release }
$ErrorActionPreference = 'Stop'
if ($LASTEXITCODE -ne 0) { throw 'build failed' }
$cfg = if ($Debug) { 'debug' } else { 'release' }
$exe = Join-Path $env:CARGO_TARGET_DIR "$cfg\kirinuki.exe"
$dist = Join-Path $PSScriptRoot 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$out = Join-Path $dist 'kirinuki.exe'
Copy-Item $exe $out -Force
# Files made inside some sandboxes carry a Low integrity label, and a process never runs above its exe's
# label: kirinuki would start at Low and could not save anywhere. Replace such a label with High, which
# means "no cap" (the process runs at its parent's level). Medium is not enough when the shell runs High:
# Explorer windows opened by a Medium kirinuki cannot drag & drop into High apps.
if ((icacls $out | Out-String) -match 'Low Mandatory Level') {
    icacls $out /setintegritylevel H | Out-Null
    if ($LASTEXITCODE -ne 0) { icacls $out /setintegritylevel M | Out-Null }
}
Write-Host "ok: $out ($([math]::Round((Get-Item $exe).Length/1MB,2)) MB)"
