#requires -Version 7.0
$ErrorActionPreference='Stop'
$repo=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$workspace=if($env:CODEX_SESSION_BAR_WORKSPACE){(Resolve-Path -LiteralPath $env:CODEX_SESSION_BAR_WORKSPACE).Path}elseif((Split-Path -Leaf (Split-Path -Parent $repo)) -eq 'tools'){(Resolve-Path -LiteralPath (Join-Path $repo '../..')).Path}else{$repo}
$temp=[IO.Path]::GetFullPath((Join-Path $workspace 'tmp'))
if((Split-Path -Parent $temp) -ne $workspace){throw 'Invalid workspace tmp'}
$task=[IO.Path]::GetFullPath((Join-Path $temp ('session-bar-updater-test-'+[DateTime]::UtcNow.ToString('yyyyMMdd-HHmmssfff')+'-'+$PID)))
if(-not $task.StartsWith($temp+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Unsafe test root'}
[IO.Directory]::CreateDirectory($task)|Out-Null
$env:TEMP=$task;$env:TMP=$task;$env:TMPDIR=$task
$native=Join-Path $task ('native-'+[DateTime]::UtcNow.ToString('HHmmss'))
[IO.Directory]::CreateDirectory($native)|Out-Null
$source=Join-Path $PSScriptRoot 'fixtures/update.rs'
$env:RUSTUP_AUTO_INSTALL='0'
$rustc=if($env:RUSTC){$env:RUSTC}else{(& rustup which rustc | Select-Object -Last 1)}
foreach($version in @('9.9.8','9.9.9')){
    $env:FIXTURE_VERSION=$version
    & $rustc --crate-name updater_fixture -C opt-level=2 -C debuginfo=0 $source -o (Join-Path $task ('fixture-'+$version+'.exe'))
    if($LASTEXITCODE -ne 0){throw 'Fixture compilation failed'}
}
$script=Join-Path $native 'install.ps1'
$sourceScript=Join-Path $repo 'src/update/install.ps1'
[IO.File]::WriteAllText($script,[IO.File]::ReadAllText($sourceScript),[Text.UTF8Encoding]::new($true))
$shell=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
Add-Type -AssemblyName System.IO.Compression.FileSystem
function Case([string]$name,[string]$bad){
    $folder=Join-Path $native ($name+'-work');$app=Join-Path $native ($name+' 安装目录')
    [IO.Directory]::CreateDirectory((Join-Path $app 'bin'))|Out-Null
    [IO.Directory]::CreateDirectory($folder)|Out-Null
    Copy-Item -LiteralPath (Join-Path $task 'fixture-9.9.8.exe') -Destination (Join-Path $app 'bin/codex-session-bar.exe')
    [IO.File]::WriteAllText((Join-Path $app 'settings.json'),'{"preserve":"settings"}')
    [IO.File]::WriteAllText((Join-Path $app 'tags.json'),'{"preserve":"tags"}')
    [IO.File]::WriteAllText((Join-Path $app 'README.md'),'previous documentation')
    $before=(Get-FileHash -LiteralPath (Join-Path $app 'bin/codex-session-bar.exe')).Hash
    $zipPath=Join-Path $folder 'package.zip'
    $zip=[IO.Compression.ZipFile]::Open($zipPath,[IO.Compression.ZipArchiveMode]::Create)
    $prefix='codex-session-bar-9.9.9-windows-x64/'
    try{
        $binary=if($bad -eq 'version'){Join-Path $task 'fixture-9.9.8.exe'}else{Join-Path $task 'fixture-9.9.9.exe'}
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip,$binary,$prefix+'bin/codex-session-bar.exe')|Out-Null
        foreach($pair in @(@('README.md','new documentation'),@('emoji_catalog.json','{}'))){
            $entry=$zip.CreateEntry($prefix+$pair[0]);$writer=[IO.StreamWriter]::new($entry.Open());$writer.Write($pair[1]);$writer.Dispose()
        }
        if($bad -eq 'traversal'){$entry=$zip.CreateEntry($prefix+'../escaped.txt');$writer=[IO.StreamWriter]::new($entry.Open());$writer.Write('no');$writer.Dispose()}
    }finally{$zip.Dispose()}
    $hash=(Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if($bad -eq 'hash'){$hash='0'*64}
    $plan=@{version='9.9.9';app=$app;tmp_root=$native;folder=$folder;archive=$zipPath;sha256=$hash;parent_pid=0;parent_created=0;chinese=$false}
    $planPath=Join-Path $folder 'plan.json'
    function SavePlan { [IO.File]::WriteAllText($planPath,($plan|ConvertTo-Json),[Text.UTF8Encoding]::new($false)) }
    SavePlan
    function Helper([string]$mode) {
        $arguments='-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File "'+$script+'" -PlanPath "'+$planPath+'" -Mode '+$mode
        Start-Process -FilePath $shell -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $folder ($mode+'.txt')) -RedirectStandardError (Join-Path $folder ($mode+'-errors.txt'))
    }
    $prepare=Helper 'Prepare'
    if(-not $prepare.WaitForExit(15000)){$prepare.Kill();throw 'Prepare hung'}
    if($bad -in @('hash','traversal','version')){
        if($prepare.ExitCode -eq 0){throw ('Unsafe package accepted: '+$name)}
        if((Get-FileHash -LiteralPath (Join-Path $app 'bin/codex-session-bar.exe')).Hash -ne $before){throw 'Preflight altered installed executable'}
        if(Test-Path -LiteralPath (Join-Path $folder 'escaped.txt')){throw 'ZIP escaped staging'}
        Write-Output ($name+': rejected; installed files untouched')
        return
    }
    if($prepare.ExitCode -ne 0){throw ('Preparation failed: '+$folder)}
    $parent=Start-Process -FilePath (Join-Path $app 'bin/codex-session-bar.exe') -ArgumentList '--parent' -WindowStyle Hidden -PassThru
    $plan.parent_pid=$parent.Id;$plan.parent_created=$parent.StartTime.ToUniversalTime().ToFileTimeUtc();SavePlan
    $lock=$null
    try{
        if($bad -eq 'locked'){$lock=[IO.File]::Open((Join-Path $app 'README.md'),[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)}
        $helper=Helper 'Install'
        $deadline=[DateTime]::UtcNow.AddSeconds(15)
        while(-not (Test-Path -LiteralPath (Join-Path $folder 'ready.json'))){
            if($helper.HasExited -or [DateTime]::UtcNow -gt $deadline){throw ('Helper failed handoff: '+$folder)}
            Start-Sleep -Milliseconds 50
        }
        [IO.File]::WriteAllText((Join-Path $app 'bin/exit-parent'),'exit')
        if(-not $parent.WaitForExit(5000)){throw 'Fixture parent did not exit'}
        if(-not $helper.WaitForExit(15000)){$helper.Kill();throw 'Installer hung'}
        $result=[IO.File]::ReadAllText((Join-Path $folder 'result.json'))|ConvertFrom-Json
        if($bad -eq 'locked'){
            if($result.phase -ne 'failed' -or @($result.rollback_errors).Count -gt 0){throw ('Rollback failed: '+$folder)}
            if((Get-FileHash -LiteralPath (Join-Path $app 'bin/codex-session-bar.exe')).Hash -ne $before){throw 'Original executable was not restored'}
            if(-not (Test-Path -LiteralPath (Join-Path $app 'bin/started-9.9.8.txt'))){throw 'Original executable was not restarted'}
        }else{
            if($helper.ExitCode -ne 0 -or $result.phase -ne 'installed'){throw ('Installation failed: '+$folder)}
            if(-not (Test-Path -LiteralPath (Join-Path $app 'bin/started-9.9.9.txt'))){throw 'Updated executable was not restarted'}
        }
        foreach($file in @('settings.json','tags.json')){
            if([IO.File]::ReadAllText((Join-Path $app $file)) -notmatch 'preserve'){throw 'User settings changed'}
        }
        Write-Output ($name+': '+$result.phase+'; settings and tags preserved')
    }finally{
        if($lock){$lock.Dispose()}
        if(-not $parent.HasExited){[IO.File]::WriteAllText((Join-Path $app 'bin/exit-parent'),'exit');$parent.WaitForExit(5000)|Out-Null}
    }
}
Case 'successful-update' ''
Case 'rollback-on-copy-failure' 'locked'
Case 'bad-hash' 'hash'
Case 'path-traversal' 'traversal'
Case 'wrong-version' 'version'
