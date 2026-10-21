param([switch]$SkipBuild, [string]$GuardExecutable)
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
$guardArtifact=if($GuardExecutable){(Resolve-Path -LiteralPath $GuardExecutable).Path}else{Join-Path $tmpRoot 'memory-watch-rust-build/target/release/memory-watch.exe'}
if (-not (Test-Path -LiteralPath $guardArtifact)) {throw 'Build the Memory Watch companion first'}
$guardVersionOutput=Join-Path $releaseRoot 'guard-version.txt'
$guardCheck=Start-Process -FilePath $guardArtifact -ArgumentList '--version' -WindowStyle Hidden -PassThru -Wait -RedirectStandardOutput $guardVersionOutput -RedirectStandardError (Join-Path $releaseRoot 'guard-version.err.txt')
if($guardCheck.ExitCode -ne 0 -or ([IO.File]::ReadAllText($guardVersionOutput)).Trim() -ne ('Memory Watch '+$version)){throw 'Memory Watch version does not match the release'}
Copy-Item -LiteralPath $guardArtifact -Destination (Join-Path $package 'bin/memory-watch.exe')
[IO.Directory]::CreateDirectory((Join-Path $package 'companion/memory-watch')) | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'companion/memory-watch/README.md') -Destination (Join-Path $package 'companion/memory-watch/README.md')
# An explicit allowlist prevents settings, authentication, logs and caches entering a release.
foreach ($file in @('emoji_catalog.json', 'Launch.cmd', 'Enable-Autostart.ps1', 'SKILL.md', 'README.md', 'CHANGELOG.md', 'DEVELOPMENT.md', 'CONTRIBUTING.md', 'RELEASING.md', 'UNICODE-LICENSE.txt')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination (Join-Path $package $file)
}
[IO.Directory]::CreateDirectory((Join-Path $package 'assets/images')) | Out-Null
Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'assets/images') -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $package ('assets/images/' + $_.Name))
}
$stagedArchive = Join-Path $releaseRoot ($baseName + '.zip')
Compress-Archive -LiteralPath $package -DestinationPath $stagedArchive -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $stagedArchive -Algorithm SHA256).Hash.ToLowerInvariant()
# Final deliverables survive tmp cleanup; only build/staging content lives in tmp.
$destination = Join-Path $PSScriptRoot ('releases/v' + $version)
[IO.Directory]::CreateDirectory($destination) | Out-Null
$archive = Join-Path $destination ($baseName + '.zip')
if (Test-Path -LiteralPath $archive) {
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) {
        throw 'A different archive already exists for this version; it was not overwritten.'
    }
} else { Copy-Item -LiteralPath $stagedArchive -Destination $archive }
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) { throw 'Final archive verification failed' }
$checksum = Join-Path $destination 'SHA256SUMS.txt'
[IO.File]::WriteAllText($checksum, $hash + '  ' + [IO.Path]::GetFileName($archive) + "`n", [Text.Encoding]::ASCII)
$portable = Join-Path $destination $baseName
if (-not (Test-Path -LiteralPath $portable)) { Expand-Archive -LiteralPath $archive -DestinationPath $destination }
$resolvedStaging = (Resolve-Path -LiteralPath $releaseRoot).Path
if (-not $resolvedStaging.StartsWith($tmpRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing cleanup outside workspace/tmp' }
Remove-Item -LiteralPath $resolvedStaging -Recurse -Force
[pscustomobject]@{Version=$version; Directory=$destination; Portable=$portable; Archive=$archive; SHA256=$hash; Checksums=$checksum}
