param([Parameter(Mandatory=$true)][string]$PlanPath,
      [Parameter(Mandatory=$true)][ValidateSet('Prepare','Install')][string]$Mode,
      [switch]$Notify)
$ErrorActionPreference = 'Stop'
# Do not inherit PowerShell 7's module paths when running Windows PowerShell.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$plan = [IO.File]::ReadAllText($PlanPath, [Text.Encoding]::UTF8) | ConvertFrom-Json
$launchTemp = if ($plan.system_tmp) { [IO.Path]::GetFullPath([string]$plan.system_tmp) } else { $env:TEMP }
$launchTmp = $launchTemp
$launchTmpDir = $launchTemp
function FileHash([string]$path) {
    $stream = [IO.File]::OpenRead($path)
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($algorithm.ComputeHash($stream))).Replace('-','').ToLowerInvariant() }
    finally { $algorithm.Dispose(); $stream.Dispose() }
}
function StartHidden([string]$executable,[string]$arguments,[string]$directory,[bool]$capture) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName=$executable; $info.Arguments=$arguments; $info.WorkingDirectory=$directory
    $info.UseShellExecute=$false; $info.CreateNoWindow=$true; $info.WindowStyle=[Diagnostics.ProcessWindowStyle]::Hidden
    $info.RedirectStandardOutput=$capture; $info.RedirectStandardError=$capture
    if (-not $capture) {
        $info.EnvironmentVariables['TEMP']=$launchTemp
        $info.EnvironmentVariables['TMP']=$launchTmp
        $info.EnvironmentVariables['TMPDIR']=$launchTmpDir
    }
    [Diagnostics.Process]::Start($info)
}
function Full([string]$path) { [IO.Path]::GetFullPath($path).TrimEnd('\') }
function Within([string]$path, [string]$root) {
    $resolved = Full $path
    if (-not $resolved.StartsWith((Full $root)+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Update path escapes its root' }
    $resolved
}
function NoLinks([string]$path, [string]$root) {
    $current = Full $path
    while ($current.Length -ge (Full $root).Length) {
        if (Test-Path -LiteralPath $current) {
            if ((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Linked update paths are not supported' }
        }
        if ($current -eq (Full $root)) { break }
        $current = Split-Path -Parent $current
    }
}
function WriteJson([string]$path, $value) {
    $pending = $path+'.pending'
    [IO.File]::WriteAllText($pending,($value | ConvertTo-Json -Depth 5),[Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $pending -Destination $path -Force
}
function Allowed([string]$path) {
    $rootFiles=@('emoji_catalog.json','Launch.cmd','Enable-Autostart.ps1','SKILL.md','README.md','CHANGELOG.md',
        'DEVELOPMENT.md','CONTRIBUTING.md','RELEASING.md','UNICODE-LICENSE.txt','bin/codex-session-bar.exe')
    ($rootFiles -ccontains $path) -or ($path -cmatch '^assets/images/[a-zA-Z0-9_.-]+\.(png|svg|jpg|jpeg|webp|gif)$')
}
$tempRoot = (Resolve-Path -LiteralPath $plan.tmp_root).Path
$folder = Within $plan.folder $tempRoot
if ((Full (Split-Path -Parent $folder)) -ne (Full $tempRoot)) { throw 'Update work directory must be a direct child of its temporary root' }
NoLinks $folder $tempRoot
$env:TEMP=$folder; $env:TMP=$folder; $env:TMPDIR=$folder
$app = (Resolve-Path -LiteralPath $plan.app).Path
NoLinks $app $app
$archive = Within $plan.archive $folder
$stage = Within (Join-Path $folder 'stage') $folder
$backup = Within (Join-Path $folder 'backup') $folder
$manifestPath = Join-Path $folder 'files.json'
$version = [string]$plan.version
if ($version -cnotmatch '^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$' -or $plan.sha256 -notmatch '^[0-9a-f]{64}$') { throw 'Invalid update version or checksum' }
$modified = New-Object 'Collections.Generic.List[string]'
$original = @{}
$parent = $null
$started = $null
try {
    if ($Mode -eq 'Prepare') {
        if ((FileHash $archive) -ne $plan.sha256) { throw 'Package checksum mismatch' }
        if (Test-Path -LiteralPath $stage) { throw 'Staging directory already exists' }
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        $files = New-Object 'Collections.Generic.List[object]'
        $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        $prefix = 'codex-session-bar-'+$version+'-windows-x64/'
        $total = [int64]0
        try {
            if ($zip.Entries.Count -gt 1024) { throw 'Too many package entries' }
            foreach ($entry in $zip.Entries) {
                $name = $entry.FullName.Replace('\','/')
                if (-not $name.StartsWith($prefix,[StringComparison]::Ordinal)) { throw 'Unexpected package root' }
                $relative = $name.Substring($prefix.Length)
                if ($relative -match '(^|/)\.\.(/|$)|:|^/' -or $relative.Contains([char]0)) { throw 'Unsafe package path' }
                if ($name.EndsWith('/')) { continue }
                if (-not (Allowed $relative) -or -not $seen.Add($relative)) { throw 'Unexpected or duplicate package file' }
                if ((($entry.ExternalAttributes -shr 16) -band 0xf000) -eq 0xa000) { throw 'Linked package entries are not supported' }
                $total += $entry.Length
                if ($total -gt 128MB) { throw 'Unpacked update exceeds 128 MiB' }
                $target = Within (Join-Path $stage $relative) $stage
                [IO.Directory]::CreateDirectory((Split-Path -Parent $target)) | Out-Null
                [IO.Compression.ZipFileExtensions]::ExtractToFile($entry,$target,$false)
                $files.Add(@{path=$relative;sha256=(FileHash $target)})
                $destination = Within (Join-Path $app $relative) $app
                NoLinks $destination $app
                if (Test-Path -LiteralPath $destination -PathType Container) { throw 'A package file conflicts with an installed directory' }
            }
        } finally { $zip.Dispose() }
        if (-not $seen.Contains('bin/codex-session-bar.exe') -or -not $seen.Contains('emoji_catalog.json')) { throw 'Incomplete Session Bar package' }
        $probe = StartHidden (Join-Path $stage 'bin/codex-session-bar.exe') '--version' $folder $true
        if (-not $probe.WaitForExit(10000)) { $probe.Kill(); $probe.WaitForExit(); throw 'Updated executable version check timed out' }
        if ($probe.ExitCode -ne 0 -or $probe.StandardOutput.ReadToEnd().Trim() -ne ('Codex Session Bar '+$version)) { throw 'Updated executable version mismatch' }
        $probe.Dispose()
        WriteJson $manifestPath @{version=$version;files=@($files.ToArray())}
        exit 0
    }
    NoLinks $stage $folder
    $manifest = [IO.File]::ReadAllText($manifestPath,[Text.Encoding]::UTF8) | ConvertFrom-Json
    if ($manifest.version -ne $version -or @($manifest.files).Count -gt 1024) { throw 'Invalid staged manifest' }
    foreach ($file in $manifest.files) {
        if (-not (Allowed $file.path)) { throw 'Invalid staged file' }
        $source = Within (Join-Path $stage $file.path) $stage
        NoLinks $source $stage
        if ((FileHash $source) -ne $file.sha256) { throw 'Staged file changed after verification' }
    }
    $parent = [Diagnostics.Process]::GetProcessById([int]$plan.parent_pid)
    if ($parent.StartTime.ToUniversalTime().ToFileTimeUtc() -ne [int64]$plan.parent_created) { throw 'Session Bar process identity changed' }
    # Open the process handle before signalling readiness; PID reuse cannot pass
    # this wait and no process is killed by the installer.
    $null = $parent.Handle
    WriteJson (Join-Path $folder 'ready.json') @{phase='ready';pid=$PID}
    if (-not $parent.WaitForExit(60000)) { throw 'Session Bar did not exit; installation was not changed' }
    [IO.Directory]::CreateDirectory($backup) | Out-Null
    # Back up every original before the first replacement, preserving settings,
    # tags, credentials, caches and all other files outside this allowlist.
    foreach ($file in $manifest.files) {
        $target = Within (Join-Path $app $file.path) $app
        NoLinks $target $app
        $exists = Test-Path -LiteralPath $target -PathType Leaf
        $original[$file.path] = $exists
        if ($exists) {
            $saved = Within (Join-Path $backup $file.path) $backup
            [IO.Directory]::CreateDirectory((Split-Path -Parent $saved)) | Out-Null
            Copy-Item -LiteralPath $target -Destination $saved
        }
    }
    foreach ($file in $manifest.files) {
        $source = Within (Join-Path $stage $file.path) $stage
        $target = Within (Join-Path $app $file.path) $app
        NoLinks $target $app
        [IO.Directory]::CreateDirectory((Split-Path -Parent $target)) | Out-Null
        $modified.Add($file.path)
        Copy-Item -LiteralPath $source -Destination $target -Force
        if ((FileHash $target) -ne $file.sha256) { throw 'Installed file verification failed' }
    }
    $started = StartHidden (Join-Path $app 'bin/codex-session-bar.exe') '' $app $false
    if ($started.WaitForExit(1000) -and $started.ExitCode -ne 0) { throw 'Updated Session Bar could not start' }
    WriteJson (Join-Path $folder 'result.json') @{phase='installed';version=$version;pid=$started.Id}
    exit 0
} catch {
    $failure = $_.Exception.Message
    $rollbackErrors = New-Object 'Collections.Generic.List[string]'
    if ($started -and -not $started.HasExited) { $started.Kill(); $started.WaitForExit(10000) | Out-Null }
    foreach ($relative in $modified) {
        try {
            $target = Within (Join-Path $app $relative) $app
            NoLinks $target $app
            if ($original[$relative]) {
                $saved = Within (Join-Path $backup $relative) $backup
                if (-not (Test-Path -LiteralPath $target -PathType Leaf) -or (FileHash $target) -ne (FileHash $saved)) {
                    Copy-Item -LiteralPath $saved -Destination $target -Force
                }
            } elseif (Test-Path -LiteralPath $target -PathType Leaf) { Remove-Item -LiteralPath $target -Force }
        } catch { $rollbackErrors.Add($_.Exception.Message) }
    }
    WriteJson (Join-Path $folder 'result.json') @{phase='failed';error=$failure;rollback_errors=@($rollbackErrors.ToArray())}
    if ($Mode -eq 'Install') {
        if ($modified.Count -gt 0 -and $rollbackErrors.Count -eq 0) {
            $env:TEMP=$launchTemp; $env:TMP=$launchTmp; $env:TMPDIR=$launchTmpDir
            Start-Process -FilePath (Join-Path $app 'bin/codex-session-bar.exe') -WorkingDirectory $app -WindowStyle Hidden | Out-Null
        }
        if ($Notify) {
            Add-Type -AssemblyName PresentationFramework
            $message = if ($plan.chinese) { '更新失败，已尝试恢复原程序。设置未被覆盖。' } else { 'The update failed. The previous files were restored where possible. Settings were not overwritten.' }
            if ($rollbackErrors.Count) { $message += $(if ($plan.chinese) { "`n部分文件未能恢复：" } else { "`nRollback requires attention: " })+($rollbackErrors -join '; ') }
            $title = if ($plan.chinese) { 'Session Bar 更新' } else { 'Session Bar update' }
            [System.Windows.MessageBox]::Show($message+"`n"+$failure,$title,'OK','Warning') | Out-Null
        }
    }
    Write-Error $failure
    exit 1
} finally { if ($parent) { $parent.Dispose() } }
