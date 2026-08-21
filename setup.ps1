# XaeroTools one-liner setup for Windows (PowerShell):
#   powershell -ExecutionPolicy Bypass -File setup.ps1
#
# Installs the Rust toolchain if missing (rustup will offer to install the
# Visual Studio Build Tools prerequisites automatically — accept the defaults),
# then builds the release binary. Node.js is NOT required (the web UI ships
# prebuilt and is embedded into the .exe).
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Say($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# 1. Rust toolchain.
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    $userCargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
    if (Test-Path $userCargo) {
        $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
    } else {
        Say "Rust not found - downloading rustup-init (it will offer to install the VS Build Tools prerequisites; accept the defaults)"
        $rustup = "$env:TEMP\rustup-init.exe"
        Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustup
        & $rustup -y --profile minimal
        $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
    }
}
Say "using $(cargo --version)"

# 2. Build.
Say "building XaeroTools (release)..."
cargo build --release -p xaerotools

# 3. Self-check against the sample corpus when it's next to the repo.
if (Test-Path "..\sample data") {
    Say "sample data found - running the format round-trip self-check"
    cargo test -p xaero-core --release --test corpus 2>$null | Select-String "test result"
}

$bin = Join-Path (Get-Location) "target\release\xaerotools.exe"
Say "done! binary: $bin"
Write-Host ""
Write-Host "  Start the viewer (auto-detects .minecraft):   $bin"
Write-Host "  Point at a folder:                            $bin serve --root D:\path\to\xaero --open"
Write-Host "  All commands:                                 $bin help"
