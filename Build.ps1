param([switch]$Check, [switch]$NoInstall)
$ErrorActionPreference = 'Stop'
$workspace = if ($env:CODEX_SESSION_BAR_WORKSPACE) {
    (Resolve-Path -LiteralPath $env:CODEX_SESSION_BAR_WORKSPACE).Path
} elseif ((Split-Path -Leaf (Split-Path -Parent $PSScriptRoot)) -eq 'tools') {
    (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
} else { (Resolve-Path -LiteralPath $PSScriptRoot).Path }
$tmpRoot = [IO.Path]::GetFullPath((Join-Path $workspace 'tmp'))
$buildRoot = [IO.Path]::GetFullPath((Join-Path $tmpRoot 'session-bar-rust-build'))
if (-not $buildRoot.StartsWith($tmpRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Build temporary content must stay within workspace/tmp'
}
[IO.Directory]::CreateDirectory($buildRoot) | Out-Null
$env:TEMP = $buildRoot
$env:TMP = $buildRoot
$env:TMPDIR = $buildRoot
$env:RUSTUP_AUTO_INSTALL = '0'
$rustc = (& rustup which rustc 2>$null | Select-Object -Last 1)
if (-not $rustc -or -not (Test-Path -LiteralPath $rustc)) {
    throw 'An installed Rust MSVC toolchain and Visual Studio C++ Build Tools are required.'
}
$toolchain = Split-Path -Parent $rustc
$env:RUSTC = $rustc
$env:RUSTDOC = Join-Path $toolchain 'rustdoc.exe'
$env:CARGO_HOME = Join-Path $buildRoot 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $buildRoot 'target'
$env:PATH = $toolchain + ';' + $env:PATH
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
if ($Check) {
    & (Join-Path $toolchain 'cargo.exe') test --locked --jobs 2 --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed' }
}
& (Join-Path $toolchain 'cargo.exe') build --release --locked --jobs 2 --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
$artifact = Join-Path $env:CARGO_TARGET_DIR 'release/codex-session-bar.exe'
$env:CODEX_SESSION_BAR_WORKSPACE=$workspace
& (Join-Path $PSScriptRoot 'companion/memory-watch/Build.ps1') -NoInstall
$guardArtifact=Join-Path $tmpRoot 'memory-watch-rust-build/target/release/memory-watch.exe'
if ($NoInstall) {
    Write-Output ('Built ' + $artifact)
    return
}
$destination = Join-Path $PSScriptRoot 'bin'
[IO.Directory]::CreateDirectory($destination) | Out-Null
Copy-Item -LiteralPath $artifact -Destination (Join-Path $destination 'codex-session-bar.exe') -Force
Copy-Item -LiteralPath $guardArtifact -Destination (Join-Path $destination 'memory-watch.exe') -Force
Write-Output ('Built ' + (Join-Path $destination 'codex-session-bar.exe'))
