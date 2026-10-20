param([switch]$NoInstall)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
$workspace = if ($env:CODEX_SESSION_BAR_WORKSPACE) {
    (Resolve-Path -LiteralPath $env:CODEX_SESSION_BAR_WORKSPACE).Path
} elseif ((Split-Path -Leaf (Split-Path -Parent $repo)) -eq 'tools') {
    (Resolve-Path -LiteralPath (Join-Path $repo '../..')).Path
} else { $repo }
$env:CODEX_SESSION_BAR_WORKSPACE=$workspace
$tmpRoot = [IO.Path]::GetFullPath((Join-Path $workspace 'tmp'))
$build = [IO.Path]::GetFullPath((Join-Path $tmpRoot 'memory-watch-rust-build'))
if (-not $build.StartsWith($tmpRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Invalid temporary directory' }
[IO.Directory]::CreateDirectory($build) | Out-Null
$env:TEMP=$build; $env:TMP=$build; $env:TMPDIR=$build
$env:RUSTUP_AUTO_INSTALL='0'
$tool = Split-Path -Parent (& rustup which rustc | Select-Object -Last 1)
$env:RUSTC=Join-Path $tool 'rustc.exe'; $env:RUSTDOC=Join-Path $tool 'rustdoc.exe'
if (-not $env:CARGO_HOME -or -not ([IO.Path]::GetFullPath($env:CARGO_HOME)).StartsWith($tmpRoot+'\',[StringComparison]::OrdinalIgnoreCase)) { $env:CARGO_HOME=Join-Path $build 'cargo-home' }
$env:CARGO_TARGET_DIR=Join-Path $build 'target'
$env:PATH=$tool+';'+$env:PATH
& (Join-Path $tool 'cargo.exe') test --locked --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml') --jobs 2
if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
& (Join-Path $tool 'cargo.exe') build --release --locked --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml') --jobs 2
if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
if ($NoInstall) { Write-Output ('Built '+(Join-Path $env:CARGO_TARGET_DIR 'release/memory-watch.exe')); return }
$bin=Join-Path $PSScriptRoot 'bin'
[IO.Directory]::CreateDirectory($bin) | Out-Null
Copy-Item -LiteralPath (Join-Path $env:CARGO_TARGET_DIR 'release/memory-watch.exe') -Destination (Join-Path $bin 'memory-watch.exe') -Force
