#Requires -Version 7.4
<#
.SYNOPSIS
Captures remote-journal.jsonl, beside this script, from the real gate, sandboxed.

.DESCRIPTION
Runs ~/.claude/tools/gate/gate.ps1 under a scratch LOCALAPPDATA (run-<stamp>\lad under -Scratch), the slot prefix
ops253- and a stub remote (GATE_TEST_REMOTE_STUB), so no live journal, mutex, claim or build box is touched. The gates, in
order: a routed gate that passes on the one remote slot (holding it), a routed gate that falls back with `full` while it
is held, one refused routed gate per reason (unreachable, busy, parity, rebooted, tools; the parity one's command exits 1
here), a routed gate that falls back onto its parent's slot while a critical request is pending, and a local control
gate. The journal is then written as the fixture: LF, the scratch repo's path as C:\work\repo, pids renumbered 70100,
70200, ... in order of first line (the parent marker's pid as 70000) and every time shifted so the first line is at
2026-10-03T06:00:00Z. The scratch folder is left for the caller to delete.

.PARAMETER Scratch
The folder the run's scratch files go under; the caller's .tmp\cap-remote-journal by default.

.EXAMPLE
pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap.log -TimeoutSeconds 600 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-remote-journal.ps1
#>
[CmdletBinding()]
param([string]$Scratch = (Join-Path $PWD '.tmp\cap-remote-journal'))

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$gate = Join-Path $HOME '.claude\tools\gate\gate.ps1'
$prefix = 'ops253-'
$run = Join-Path $Scratch ('run-' + [DateTime]::UtcNow.ToString('yyyyMMddHHmmss'))
$lad = Join-Path $run 'lad'
$repo = Join-Path $run 'repo'
$fixture = Join-Path $PSScriptRoot 'remote-journal.jsonl'
$utf8 = [System.Text.UTF8Encoding]::new($false)

function Write-Text {
    param([string]$Path, [string]$Text)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8)
}

# A stub Invoke-RemoteCommand that writes its line to the log, holds until $Release exists or $Seconds pass, and
# returns $Status, as a run that reached the build box would (the gate self-test's New-RemoteStub).
function Write-RemoteStub {
    param([string]$Name, [int]$Status, [int]$Seconds, [string]$Release)
    $path = Join-Path $run "stub-$Name.ps1"
    $quoted = $Release.Replace("'", "''")
    $text = @(
        'function Invoke-RemoteCommand {'
        '    param([hashtable]$Options, [string[]]$Command)'
        '    [System.IO.File]::WriteAllText($Options.Log, "stub remote run on $($Options.HostAlias): $($Command -join '' '')`n")'
        '    $hold = [System.Diagnostics.Stopwatch]::StartNew()'
        "    while (`$hold.Elapsed.TotalSeconds -lt $Seconds -and -not (Test-Path -LiteralPath '$quoted')) { Start-Sleep -Milliseconds 200 }"
        "    return @{ Status = $Status; Killed = `$false; Reason = ''; Context = @{ LogPath = `$Options.Log } }"
        '}'
    ) -join "`n"
    Write-Text -Path $path -Text "$text`n"
    return $path
}

# A stub Invoke-RemoteCommand that refuses the run: it writes its lines, sleeps $Seconds and returns 75 with $Reason
# (the gate self-test's New-RefusalStub).
function Write-RefusalStub {
    param([string]$Name, [string]$Reason, [int]$Seconds)
    $path = Join-Path $run "stub-$Name.ps1"
    $text = @(
        'function Invoke-RemoteCommand {'
        '    param([hashtable]$Options, [string[]]$Command)'
        "    [System.IO.File]::WriteAllText(`$Options.Log, ""stub remote run on `$(`$Options.HostAlias)``nstub remote refused: $Reason``n"")"
        "    Start-Sleep -Seconds $Seconds"
        "    return @{ Status = 75; Killed = `$false; Reason = '$Reason'; Context = @{ LogPath = `$Options.Log } }"
        '}'
    ) -join "`n"
    Write-Text -Path $path -Text "$text`n"
    return $path
}

# Starts a heavy gate in the scratch repo running $Script, with the sandbox's variables, the stub $Stub, the parent marker
# $Held (none when empty) and the remote wait $Wait (the gate's 60 s when empty); returns the process.
function Invoke-Gate {
    param([string]$Name, [string]$Script, [string]$Stub, [string]$Held, [string]$Wait)
    $info = [System.Diagnostics.ProcessStartInfo]::new('pwsh')
    $info.UseShellExecute = $false
    $info.WorkingDirectory = $repo
    $words = @('-NoProfile', '-File', $gate, '-Log', ".tmp\$Name.log", '-TimeoutSeconds', '60', '-Slot', 'heavy', '--',
        'pwsh', '-NoProfile', '-c', $Script)
    foreach ($word in $words) { $info.ArgumentList.Add($word) }
    $variables = [ordered]@{
        LOCALAPPDATA = $lad; GATE_TEST_SLOT_PREFIX = $prefix; GATE_TEST_SLOT_COUNTS = (Join-Path $run 'counts.json')
        GATE_TEST_REMOTE_CONFIG = (Join-Path $run 'remote.json'); GATE_TEST_REMOTE_STUB = $Stub
        GATE_TEST_POLL_MILLISECONDS = '250'; GATE_TEST_HEARTBEAT_SECONDS = '1'
    }
    foreach ($name in $variables.Keys) { $info.Environment[$name] = $variables[$name] }
    [void]$info.Environment.Remove('GATE_SLOT_HELD')
    [void]$info.Environment.Remove('GATE_TEST_REMOTE_WAIT_SECONDS')
    if ($Held) { $info.Environment['GATE_SLOT_HELD'] = $Held }
    if ($Wait) { $info.Environment['GATE_TEST_REMOTE_WAIT_SECONDS'] = $Wait }
    return [System.Diagnostics.Process]::Start($info)
}

# Waits up to 90 s for the gate to end and checks its exit status.
function Wait-Gate {
    param([System.Diagnostics.Process]$Process, [string]$Name, [int]$Expected)
    if (-not $Process.WaitForExit(90000)) { throw "gate $Name (pid $($Process.Id)) did not end within 90 s" }
    if ($Process.ExitCode -ne $Expected) { throw "gate $Name exited $($Process.ExitCode), expected $Expected; see $repo\.tmp\$Name.log" }
    Write-Information "gate $Name (pid $($Process.Id)) exited $($Process.ExitCode)" -InformationAction Continue
}

# Waits up to $Seconds for the file at $Path.
function Wait-File {
    param([string]$Path, [int]$Seconds)
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $Path)) {
        if ($watch.Elapsed.TotalSeconds -ge $Seconds) { throw "$Path did not appear within $Seconds s" }
        Start-Sleep -Milliseconds 200
    }
}

# A journal time shifted by $Shift, written as the gate writes it (UTC, seven fractional digits).
function Get-ShiftedTime {
    param([string]$Text, [TimeSpan]$Shift)
    $at = [DateTimeOffset]::Parse($Text, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::AssumeUniversal)
    return $at.Add($Shift).UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.fffffff'Z'", [Globalization.CultureInfo]::InvariantCulture)
}

# The captured journal's lines as the fixture holds them.
function ConvertTo-FixtureLine {
    param([string[]]$Lines, [int]$ParentPid)
    $escapedRepo = $repo.Replace('\', '\\')
    $first = [regex]::Match($Lines[0], '"time":"([^"]+)"').Groups[1].Value
    $start = [DateTimeOffset]::Parse('2026-10-03T06:00:00Z', [Globalization.CultureInfo]::InvariantCulture)
    $shift = $start - [DateTimeOffset]::Parse($first, [Globalization.CultureInfo]::InvariantCulture)
    $pids = [ordered]@{}
    $pids["$ParentPid"] = 70000
    foreach ($line in $Lines) {
        $found = [regex]::Match($line, '"pid":(\d+)').Groups[1].Value
        if (-not $pids.Contains($found)) { $pids[$found] = 70000 + 100 * $pids.Count }
    }
    foreach ($line in $Lines) {
        $text = $line.Replace($escapedRepo, 'C:\\work\\repo')
        $text = [regex]::Replace($text, '"(time|processStart)":"([^"]+)"', {
                param($match)
                '"{0}":"{1}"' -f $match.Groups[1].Value, (Get-ShiftedTime -Text $match.Groups[2].Value -Shift $shift)
            })
        $text = [regex]::Replace($text, '"(pid|parentPid)":(\d+)', {
                param($match)
                $known = $pids[$match.Groups[2].Value]
                if ($null -eq $known) { throw "no pid mapping for $($match.Value)" }
                '"{0}":{1}' -f $match.Groups[1].Value, $known
            })
        $text
    }
}

New-Item -ItemType Directory -Force (Join-Path $lad 'gate'), (Join-Path $repo '.tmp') | Out-Null
& git -C $repo init -q
if ($LASTEXITCODE -ne 0) { throw "git init failed in $repo" }
Write-Text -Path (Join-Path $run 'counts.json') -Text '{"heavy":1,"light":1}'
Write-Text -Path (Join-Path $run 'remote.json') -Text '{"host":"builder@build-box","slots":1,"repos":{"repo":["pwsh -NoProfile -c *remote*"]}}'
$remoteClaim = Join-Path $lad "gate\slots\${prefix}gate-remote-slot-0.json"
$noRelease = Join-Path $run 'no-release'

# An ok remote run holding the one remote slot, and a routed gate that falls back with full while it holds it.
$holderRelease = Join-Path $run 'holder-release'
$holder = Invoke-Gate -Name 'remote-ok' -Script 'exit 0 # remote' -Stub (Write-RemoteStub -Name 'holder' -Status 0 -Seconds 30 -Release $holderRelease)
Wait-File -Path $remoteClaim -Seconds 60
$full = Invoke-Gate -Name 'full' -Script 'exit 0 # remote' -Stub (Write-RemoteStub -Name 'ok' -Status 0 -Seconds 0 -Release $noRelease) -Wait '3'
Wait-Gate -Process $full -Name 'full' -Expected 0
Write-Text -Path $holderRelease -Text ''
Wait-Gate -Process $holder -Name 'remote-ok' -Expected 0

# One routed gate refused after its remote take per reason; the parity one's local run fails.
foreach ($reason in 'unreachable', 'busy', 'parity', 'rebooted', 'tools') {
    $status = if ($reason -eq 'parity') { 1 } else { 0 }
    $refused = Invoke-Gate -Name "refused-$reason" -Script "exit $status # remote" -Stub (Write-RefusalStub -Name "refused-$reason" -Reason $reason -Seconds 0)
    Wait-Gate -Process $refused -Name "refused-$reason" -Expected $status
}

# A routed gate under this process's light marker that falls back while a critical request is pending (this process
# holds the critical mutex once the gate holds its remote slot): it runs on its parent's slot.
$marker = "light:${PID}:$((Get-Process -Id $PID).StartTime.ToUniversalTime().Ticks)"
$parentStub = Write-RefusalStub -Name 'parent-busy' -Reason 'busy' -Seconds 4
$parent = Invoke-Gate -Name 'parent' -Script 'exit 0 # remote' -Stub $parentStub -Held $marker
Wait-File -Path $remoteClaim -Seconds 60
$critical = [System.Threading.Mutex]::new($false, "Local\${prefix}gate-critical")
try {
    if (-not $critical.WaitOne(10000)) { throw 'could not take the sandbox critical mutex within 10 s' }
    try { Wait-Gate -Process $parent -Name 'parent' -Expected 0 }
    finally { $critical.ReleaseMutex() }
}
finally { $critical.Dispose() }

# A local control run: its command is not routed.
$local = Invoke-Gate -Name 'local' -Script 'exit 0 # local' -Stub (Write-RemoteStub -Name 'unused' -Status 0 -Seconds 0 -Release $noRelease)
Wait-Gate -Process $local -Name 'local' -Expected 0

$journal = Join-Path $lad "gate\${prefix}events.jsonl"
$lines = @([System.IO.File]::ReadAllLines($journal) | Where-Object { $_ })
$fixtureLines = @(ConvertTo-FixtureLine -Lines $lines -ParentPid $PID)
Write-Text -Path $fixture -Text (($fixtureLines -join "`n") + "`n")
Write-Information "captured $($lines.Count) lines from $journal into $fixture" -InformationAction Continue
