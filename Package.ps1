param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$workspace = if ($env:CODEX_SESSION_BAR_WORKSPACE) {
    (Resolve-Path -LiteralPath $env:CODEX_SESSION_BAR_WORKSPACE).Path
} elseif ((Split-Path -Leaf (Split-Path -Parent $PSScriptRoot)) -eq 'tools') {
    (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
} else { (Resolve-Path -LiteralPath $PSScriptRoot).Path }
$tmpRoot = [IO.Path]::GetFullPath((Join-Path $workspace 'tmp'))
$releaseRoot = [IO.Path]::GetFullPath((Join-Path $tmpRoot ('session-bar-release-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))))
if (-not $releaseRoot.StartsWith($tmpRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Release staging must stay within workspace/tmp'
}
[IO.Directory]::CreateDirectory($releaseRoot) | Out-Null
$env:TEMP = $releaseRoot
$env:TMP = $releaseRoot
$env:TMPDIR = $releaseRoot
$manifest = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Cargo.toml'))
$version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Groups[1].Value
if (-not $version) { throw 'Package version is unavailable' }
if (-not $SkipBuild) { & (Join-Path $PSScriptRoot 'Build.ps1') -Check -NoInstall }
$executable = Join-Path $tmpRoot 'session-bar-rust-build/target/release/codex-session-bar.exe'
if (-not (Test-Path -LiteralPath $executable)) { throw 'Build the release executable first' }
$versionOutput = Join-Path $releaseRoot 'version.txt'
$process = Start-Process -FilePath $executable -ArgumentList '--version' -WindowStyle Hidden -PassThru -Wait -RedirectStandardOutput $versionOutput -RedirectStandardError (Join-Path $releaseRoot 'version.err.txt')
if ($process.ExitCode -ne 0 -or ([IO.File]::ReadAllText($versionOutput)).Trim() -ne ('Codex Session Bar ' + $version)) {
    throw 'Executable version does not match Cargo.toml'
}
$baseName = 'codex-session-bar-' + $version + '-windows-x64'
$package = Join-Path $releaseRoot $baseName
[IO.Directory]::CreateDirectory((Join-Path $package 'bin')) | Out-Null
Copy-Item -LiteralPath $executable -Destination (Join-Path $package 'bin/codex-session-bar.exe')
# An explicit allowlist prevents settings, authentication, logs and caches entering a release.
foreach ($file in @('emoji_catalog.json', 'Launch.cmd', 'Enable-Autostart.ps1', 'SKILL.md', 'README.md', 'CHANGELOG.md', 'DEVELOPMENT.md', 'CONTRIBUTING.md', 'RELEASING.md', 'UNICODE-LICENSE.txt')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination (Join-Path $package $file)
}
[IO.Directory]::CreateDirectory((Join-Path $package 'assets/images')) | Out-Null
Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'assets/images') -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $package ('assets/images/' + $_.Name))
}
$archive = Join-Path $releaseRoot ($baseName + '.zip')
Compress-Archive -LiteralPath $package -DestinationPath $archive -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
$checksum = Join-Path $releaseRoot 'SHA256SUMS.txt'
[IO.File]::WriteAllText($checksum, $hash + '  ' + [IO.Path]::GetFileName($archive) + "`n", [Text.Encoding]::ASCII)
[pscustomobject]@{Version=$version; Archive=$archive; SHA256=$hash; Checksums=$checksum}
