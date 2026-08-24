# XaeroTools one-liner setup for Windows (PowerShell):
#   powershell -ExecutionPolicy Bypass -File setup.ps1
#
# Installs the Rust toolchain if missing - rustup-init asks whether to pull in
# the Visual Studio Build Tools first; say yes, the build needs a C compiler.
# Then builds the release binary. Node.js is NOT required (the web UI ships
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
        Say "Rust not found - downloading rustup-init (it asks about the VS Build Tools first; say yes, they are required)"
        $rustup = "$env:TEMP\rustup-init.exe"
        Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustup
        & $rustup --profile minimal
        $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
    }
}
Say "using $(cargo --version)"

# 2. C toolchain. rustup installs Rust only: on the MSVC toolchain the linker,
# headers and CRT come from the Visual Studio Build Tools, and the build needs
# them because SQLite is compiled from C source. Someone who already had Rust,
# or who declined rustup's offer, gets here without them - say so now instead
# of after a long build ends in a link error. vswhere ships with every VS 2017+
# installer at a fixed path, so it is the reliable way to ask; when the answer
# cannot be had we go ahead and build rather than block a working machine.
$rustc = Get-Command rustc -ErrorAction SilentlyContinue
$rustHost = if ($rustc) { (& rustc -vV | Where-Object { $_ -like "host: *" }) -replace "^host: ", "" } else { "" }
if ($rustHost -like "*-msvc") {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vcTools = ""
    if (Test-Path $vswhere) {
        $vcTools = & $vswhere -latest -products * -requiresAny `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 Microsoft.VisualStudio.Component.VC.Tools.ARM64 `
            -property installationPath
    }
    if (-not $vcTools) {
        Write-Host "==> no C++ build tools found - cargo cannot link without them" -ForegroundColor Red
        Write-Host "    install https://visualstudio.microsoft.com/visual-cpp-build-tools/ and tick"
        Write-Host "    the 'Desktop development with C++' workload, then re-run this script"
        exit 1
    }
}

# 3. Build.
Say "building XaeroTools (release)..."
cargo build --release -p xaerotools

# 4. Self-check against the sample corpus when it's next to the repo.
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
