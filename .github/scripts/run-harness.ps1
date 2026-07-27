# Runs the unload harness in one mode under a hang watchdog and classifies the
# outcome so the workflow can distinguish a genuine crash/hang (bug reproduced)
# from a harness or precondition failure (inconclusive).
#
# Harness exit-code contract (see Testing/unload-harness/src/main.rs):
#   0        survived
#   2        usage error                (harness)
#   3        unload precondition failed (DllCanUnloadNow was not S_OK)
#   10       environment/render validation failure (inconclusive)
#   12       FreeLibrary failed, unload not exercised (inconclusive)
#   101      Rust panic                 (harness)
#   negative unhandled Windows exception (real crash)
# The watchdog adds:
#   124      hang (suspected loader-lock deadlock) - also a positive repro
#
# Publishes `verdict=survived|crash|hang|precondition|harness` to the step's
# GITHUB_OUTPUT when running under Actions.
param(
    [Parameter(Mandatory = $true)][string]$Mode,
    [int]$Iterations = 50,
    [int]$TimeoutSec = 300,
    [int]$Repeats = 1
)

$ErrorActionPreference = 'Stop'
$exe = Join-Path $PWD 'target/debug/unload_harness.exe'
$dll = Join-Path $PWD 'target/release/win_svg_thumbs_x64.dll'
$dumpDir = Join-Path $PWD 'dumps'
New-Item -ItemType Directory -Force -Path $dumpDir | Out-Null
$cdb = 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe'

function Publish-Verdict([string]$v) {
    if ($env:GITHUB_OUTPUT) { "verdict=$v" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8 }
}

if (-not (Test-Path $exe)) { Publish-Verdict 'harness'; throw "harness not built: $exe" }
if (-not (Test-Path $dll)) { Publish-Verdict 'harness'; throw "DLL not built: $dll" }

for ($r = 1; $r -le $Repeats; $r++) {
    Write-Host "=== mode=$Mode run $r/$Repeats (iterations=$Iterations, timeout=${TimeoutSec}s) ==="
    $out = Join-Path $PWD "harness-$Mode-$r.out.log"
    $err = Join-Path $PWD "harness-$Mode-$r.err.log"
    $p = Start-Process -FilePath $exe -ArgumentList @($Mode, "$Iterations", $dll) `
        -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $exited = $p.WaitForExit($TimeoutSec * 1000)
    if (-not $exited) {
        Write-Host "--- HANG: still running after ${TimeoutSec}s; capturing non-invasive dump, then killing ---"
        if (Test-Path $cdb) {
            $dmp = Join-Path $dumpDir "hang-$Mode-run$r-pid$($p.Id).dmp"
            & $cdb -pv -p $p.Id -c ".dump /ma `"$dmp`"; qd" *> $null
            Write-Host "hang dump written: $dmp"
        }
        Stop-Process -Id $p.Id -Force
        Start-Sleep -Seconds 1
        Get-Content $out, $err -ErrorAction SilentlyContinue | Write-Host
        Publish-Verdict 'hang'
        Write-Host "RESULT($Mode): HANG (suspected loader-lock deadlock)"
        exit 124
    }
    $p.WaitForExit()   # ensure ExitCode is populated
    $code = $p.ExitCode
    Get-Content $out, $err -ErrorAction SilentlyContinue | Write-Host
    if ($code -ne 0) {
        $hex = '0x{0:X8}' -f $code
        if ($code -eq 3) {
            Publish-Verdict 'precondition'
            Write-Host "RESULT($Mode): PRECONDITION NOT MET - DllCanUnloadNow blocked the unload (exit $code). Inconclusive, not a repro."
            exit 3
        }
        elseif ($code -lt 0 -or $code -ge 0x40000000) {
            Publish-Verdict 'crash'
            Write-Host "RESULT($Mode): CRASH - unhandled exception, exit code $code ($hex)"
            exit 1
        }
        else {
            # 2, 10, 12, 101 and anything else: the harness could not create or
            # exercise the hazardous state - inconclusive, NOT proof of the bug.
            Publish-Verdict 'harness'
            Write-Host "RESULT($Mode): HARNESS/ENVIRONMENT FAILURE - exit code $code ($hex). Inconclusive, not a repro."
            exit 10
        }
    }
    Write-Host "RESULT($Mode): OK (run $r/$Repeats)"
}
Publish-Verdict 'survived'
exit 0
