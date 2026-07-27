# Long-haul COM Surrogate reproduction orchestrator.
#
# Alternates genuine shell thumbnail activity (thumb_driver bursts through
# IShellItemImageFactory / IThumbnailCache) with genuine idle periods, so the
# thumbnail extraction dllhost.exe performs its natural lifecycle: hosting the
# registered provider, winding down STA threads, freeing unused libraries and
# exiting on idle. Nothing here kills or manipulates dllhost.exe, throws
# exceptions, or simulates results - the only interventions are passive,
# non-invasive dump captures (cdb -pv) and, after evidence is preserved,
# terminating OUR OWN stuck driver client so cycles can continue.
#
# Evidence written under $EvidenceDir:
#   dllhost-timeline.csv   periodic dllhost snapshot (pid, has provider DLL)
#   crash-events.txt       full text of every relevant Application-log event
#   driver-burst-N.log     per-burst driver output
#   workload-summary.txt   cycle accounting
#   flags: surrogate-hosted.flag, crash-detected.flag, hang-detected.flag,
#          environment-mismatch.flag, inproc-contamination.flag
param(
    [int]$DurationMinutes = 330,
    [string]$SvgDir = 'svg-corpus',
    [string]$WorkDir = 'thumb-work',
    [string]$EvidenceDir = 'evidence',
    [int]$FilesPerBurst = 120,
    [string]$IdlePattern = '6,12,18',
    [int]$BurstTimeoutMinutes = 20
)

$ErrorActionPreference = 'Continue'
$driver = Join-Path $PWD 'target/debug/thumb_driver.exe'
$dumpDir = Join-Path $PWD 'dumps'
$cdb = 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe'
foreach ($d in @($WorkDir, $EvidenceDir, $dumpDir)) { New-Item -ItemType Directory -Force -Path $d | Out-Null }
if (-not (Test-Path $SvgDir)) { throw "SVG corpus directory not found: $SvgDir" }
if (-not (Test-Path $driver)) { throw "driver not built: $driver" }
$EvidenceDir = (Resolve-Path $EvidenceDir).Path
$WorkDir = (Resolve-Path $WorkDir).Path
$SvgDir = (Resolve-Path $SvgDir).Path

$deadline = (Get-Date).AddMinutes($DurationMinutes)
$idleMinutes = $IdlePattern -split ',' | ForEach-Object { [int]$_ }
$stopFile = Join-Path $EvidenceDir 'stop.flag'
Remove-Item $stopFile -ErrorAction SilentlyContinue

Write-Host "longhaul: duration=${DurationMinutes}m deadline=$deadline idlePattern=$IdlePattern filesPerBurst=$FilesPerBurst"
Write-Host "longhaul: driver=$driver svg=$SvgDir"

# ---------------- background monitor ----------------
$monitor = Start-Job -ScriptBlock {
    param($EvidenceDir, $StopFile, $StartTime)
    $timeline = Join-Path $EvidenceDir 'dllhost-timeline.csv'
    $eventsFile = Join-Path $EvidenceDir 'crash-events.txt'
    'timestamp,pid,processStart,hasProviderDll,privateBytes,handles,threads' | Out-File $timeline -Encoding utf8
    $seen = @{}
    while (-not (Test-Path $StopFile)) {
        $now = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
        # dllhost lifecycle + provider-module + resource snapshot (leak visibility)
        foreach ($p in (Get-Process -Name dllhost -ErrorAction SilentlyContinue)) {
            $hasDll = $false
            try {
                $hasDll = [bool]($p.Modules | Where-Object { $_.ModuleName -ieq 'win_svg_thumbs_x64.dll' })
            } catch {}
            "$now,$($p.Id),$($p.StartTime.ToString('HH:mm:ss')),$hasDll,$($p.PrivateMemorySize64),$($p.HandleCount),$($p.Threads.Count)" | Out-File $timeline -Append -Encoding utf8
            if ($hasDll -and -not (Test-Path (Join-Path $EvidenceDir 'surrogate-hosted.flag'))) {
                "dllhost pid $($p.Id) is hosting win_svg_thumbs_x64.dll at $now" |
                    Out-File (Join-Path $EvidenceDir 'surrogate-hosted.flag') -Encoding utf8
            }
        }
        # new crash / WER events - capture the COMPLETE event immediately
        $events = Get-WinEvent -FilterHashtable @{ LogName = 'Application'; StartTime = $StartTime } -ErrorAction SilentlyContinue |
            Where-Object { $_.ProviderName -in @('Application Error', 'Windows Error Reporting') }
        foreach ($e in ($events | Sort-Object RecordId)) {
            if ($seen.ContainsKey($e.RecordId)) { continue }
            $seen[$e.RecordId] = $true
            $block = "==== NEW EVENT captured $now ====`r`n" +
                ($e | Format-List RecordId, Id, TimeCreated, ProviderName, LevelDisplayName, MachineName | Out-String) +
                "Message:`r`n$($e.Message)`r`n"
            $block | Out-File $eventsFile -Append -Encoding utf8
            if ($e.Id -eq 1000 -and $e.Message -match '(?i)dllhost\.exe') {
                if (-not (Test-Path (Join-Path $EvidenceDir 'crash-detected.flag'))) {
                    "first dllhost crash event RecordId=$($e.RecordId) at $($e.TimeCreated)" |
                        Out-File (Join-Path $EvidenceDir 'crash-detected.flag') -Encoding utf8
                }
                # preserve the log the moment evidence exists
                wevtutil epl Application (Join-Path $EvidenceDir "application-snapshot-$($e.RecordId).evtx") 2>$null
            }
        }
        Start-Sleep -Seconds 20
    }
} -ArgumentList $EvidenceDir, $stopFile, (Get-Date)

function Relay-CrashEvents {
    # print any newly recorded events to the live console
    $f = Join-Path $EvidenceDir 'crash-events.txt'
    if (Test-Path $f) {
        $len = (Get-Item $f).Length
        if ($len -gt $script:relayed) {
            Get-Content $f | Select-Object -Skip $script:relayedLines | ForEach-Object { Write-Host $_ }
            $script:relayedLines = (Get-Content $f).Count
            $script:relayed = $len
        }
    }
}
$script:relayed = 0; $script:relayedLines = 0

function Capture-PassiveDumps([string]$reason) {
    if (-not (Test-Path $cdb)) { Write-Host "longhaul: cdb not present; skipping passive dumps ($reason)"; return }
    foreach ($p in (Get-Process -Name dllhost -ErrorAction SilentlyContinue)) {
        $hasDll = $false
        try { $hasDll = [bool]($p.Modules | Where-Object { $_.ModuleName -ieq 'win_svg_thumbs_x64.dll' }) } catch {}
        if ($hasDll) {
            $dmp = Join-Path $dumpDir "passive-dllhost-$($p.Id)-$reason.dmp"
            Write-Host "longhaul: capturing NON-INVASIVE dump of dllhost pid $($p.Id) -> $dmp"
            & $cdb -pv -p $p.Id -c ".dump /ma `"$dmp`"; qd" *> $null
        }
    }
}

# ---------------- burst / idle cycles ----------------
$burst = 0; $stalls = 0; $consecutiveAllFailed = 0; $extraAfterCrash = $false
$summary = Join-Path $EvidenceDir 'workload-summary.txt'
"start=$(Get-Date -Format o)" | Out-File $summary -Encoding utf8

while ((Get-Date) -lt $deadline) {
    # activity burst
    $log = Join-Path $EvidenceDir ("driver-burst-{0:d4}.log" -f $burst)
    Write-Host "longhaul: === burst $burst starting at $(Get-Date -Format HH:mm:ss) ==="
    $p = Start-Process -FilePath $driver -ArgumentList @($SvgDir, $WorkDir, "$burst", "$FilesPerBurst") `
        -RedirectStandardOutput $log -RedirectStandardError "$log.err" -NoNewWindow -PassThru
    $exited = $p.WaitForExit($BurstTimeoutMinutes * 60 * 1000)
    if (-not $exited) {
        # The burst wedged: a thumbnail RPC into a hung surrogate never returns.
        # Preserve evidence passively, then terminate only OUR client process.
        $stalls++
        Write-Host "longhaul: burst $burst STALLED for ${BurstTimeoutMinutes}m - capturing passive evidence (stall #$stalls)"
        "burst $burst stalled at $(Get-Date -Format o)" | Out-File (Join-Path $EvidenceDir 'hang-detected.flag') -Append -Encoding utf8
        Capture-PassiveDumps "stall$stalls"
        $dmp = Join-Path $dumpDir "passive-driver-$($p.Id)-stall$stalls.dmp"
        if (Test-Path $cdb) { & $cdb -pv -p $p.Id -c ".dump /ma `"$dmp`"; qd" *> $null }
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue   # our own client only
        if ($stalls -ge 2) {
            Write-Host 'longhaul: second stall - evidence captured; leaving the wedged surrogate untouched and idling out the remaining window'
            break
        }
    }
    else {
        Get-Content $log, "$log.err" -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
        switch ($p.ExitCode) {
            0 { $consecutiveAllFailed = 0 }
            20 {
                $consecutiveAllFailed++
                if ($consecutiveAllFailed -ge 3) {
                    'three consecutive bursts with zero successful thumbnails' | Out-File (Join-Path $EvidenceDir 'environment-mismatch.flag') -Encoding utf8
                    Write-Host 'longhaul: environment cannot exercise the shell thumbnail path; stopping early'
                }
            }
            21 {
                'provider DLL loaded in driver process; isolation ineffective' | Out-File (Join-Path $EvidenceDir 'inproc-contamination.flag') -Encoding utf8
                Write-Host 'longhaul: in-proc contamination; stopping early'
            }
            default { Write-Host "longhaul: burst $burst driver exit code $($p.ExitCode)" }
        }
        if ((Test-Path (Join-Path $EvidenceDir 'environment-mismatch.flag')) -or (Test-Path (Join-Path $EvidenceDir 'inproc-contamination.flag'))) { break }
    }
    "burst=$burst exit=$($p.ExitCode) stalls=$stalls at=$(Get-Date -Format o)" | Out-File $summary -Append -Encoding utf8
    Relay-CrashEvents

    if (Test-Path (Join-Path $EvidenceDir 'crash-detected.flag')) {
        if ($extraAfterCrash) {
            Write-Host 'longhaul: dllhost crash captured and one extra cycle completed - finishing early with evidence'
            break
        }
        Write-Host 'longhaul: dllhost CRASH EVENT DETECTED - running one more cycle for a second sample, then finishing'
        $extraAfterCrash = $true
    }

    # genuine idle window (in 30s slices so deadline/crash checks stay responsive)
    $idle = $idleMinutes[$burst % $idleMinutes.Count]
    Write-Host "longhaul: idle for ${idle}m (surrogate cleanup window) until $((Get-Date).AddMinutes($idle).ToString('HH:mm:ss'))"
    $idleEnd = (Get-Date).AddMinutes($idle)
    while ((Get-Date) -lt $idleEnd -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 30
        Relay-CrashEvents
        if ((Test-Path (Join-Path $EvidenceDir 'crash-detected.flag')) -and -not $extraAfterCrash) { break }
    }

    # keep disk bounded: remove burst dirs older than the previous one
    Get-ChildItem $WorkDir -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name | Select-Object -SkipLast 2 | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    $burst++
}

"end=$(Get-Date -Format o) bursts=$burst stalls=$stalls" | Out-File $summary -Append -Encoding utf8
Write-Host "longhaul: workload finished after $burst bursts ($stalls stalls); collecting monitor"
New-Item -ItemType File -Path $stopFile -Force | Out-Null
Wait-Job $monitor -Timeout 60 | Out-Null
Receive-Job $monitor -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
Remove-Job $monitor -Force -ErrorAction SilentlyContinue
Relay-CrashEvents
exit 0
