#Requires -Version 7.4
<#
.SYNOPSIS
Captures remote-run-journal.jsonl, beside this script, from gate.exe runs routed to stand-in builders, sandboxed.

.DESCRIPTION
Builds gate.exe, fake-tool.exe and fake-ssh.exe with the test seams through the repo's gate, then runs heavy gates in a
scratch checkout (run-<stamp>\repo under -Scratch) whose state root, LOCALAPPDATA, builder root and stand-in files sit
inside it, so every slot, mutex and claim is named under the scratch root's hash and no live journal, slot or build box
is touched. The checkout's routing file sends its *tool* commands to the builder, with one remote slot.

The first pass routes through the scripted remote client (--test-remote-client), in order: a remote run that passes,
holding the one remote slot; a gate that falls back with full while it is held (a remote wait of 1 s); one gate refused
after its remote take per reason (unreachable, busy, parity, rebooted, tools; the parity one's command exits 1 here); a
gate under this process's light marker that is refused once it holds its remote slot and runs on its parent's slot,
since this process opens the scratch root's critical mutex before the refusal; and a local control gate whose command
matches no pattern.

The second pass routes through the Rust client over the local transport (--test-remote-transport local:<builder>), each
ssh call made through fake-ssh, for the refusals the client makes itself: unreachable (two banner drops), rebooted (a
dropped run call and a later boot time), tools (a failing dotnet restoring the tool manifest on the builder), parity (a
global.json pin the builder's SDK list lacks), busy (the builder's work-folder lock held by this process) and version
(a builder that says another gate version).

The script checks each gate's exit status, that each refused gate journals fallback with its reason, that the parent
gate journals parent and that the other gates journal none, and only then writes the journal as the fixture: LF, the
scratch checkout's path as C:\work\repo, the scratch root's hash as 5ca1ab1e, the work-folder key as repo-c0ffee00,
pids renumbered 70100, 70200, ... in order of first line (the parent marker's pid as 70000) and every time shifted so
the first line is at 2026-10-12T06:00:00Z. The scratch folder is left for the caller to delete.

.PARAMETER Scratch
The folder the run's scratch files go under; the repo's .tmp\cap-remote-run by default.

.EXAMPLE
pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap-remote-run.log -TimeoutSeconds 900 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-remote-run.ps1
#>
[CmdletBinding()]
param([string]$Scratch)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..\..')).Path
if (-not $Scratch) { $Scratch = Join-Path $root '.tmp\cap-remote-run' }
$Scratch = [System.IO.Path]::GetFullPath($Scratch, $PWD.Path)
$stamp = 'run-' + [DateTime]::UtcNow.ToString('yyyyMMddHHmmss', [cultureinfo]::InvariantCulture)
$repo = Join-Path $Scratch "$stamp\repo"
$state = Join-Path $repo 'state'
$lad = Join-Path $repo 'appdata'
$builder = Join-Path $repo 'builder'
$ssh = Join-Path $repo 'ssh'
$journal = Join-Path $state 'events.jsonl'
$fixture = Join-Path $PSScriptRoot 'remote-run-journal.jsonl'
$gate = Join-Path $root 'target\test-release\gate.exe'
$fakeTool = Join-Path $root 'target\test-release\fake-tool.exe'
$fakeSsh = Join-Path $root 'target\test-release\fake-ssh.exe'
$utf8 = [System.Text.UTF8Encoding]::new($false)
$builderHost = 'builder'
$rootPlaceholder = '5ca1ab1e'
$keyPlaceholder = 'repo-c0ffee00'
$firstTime = '2026-10-12T06:00:00Z'
# The variables a gate around this script may have set, which no gate it starts may inherit: they would name another
# state root, a parent slot, or another repository for git.
$inherited = @('GATE_STATE_DIR', 'GATE_SLOT_HELD', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_CONFIG_PARAMETERS',
    'GIT_COMMON_DIR', 'GIT_OBJECT_DIRECTORY')

function Write-Text {
    param([string]$Path, [string]$Text)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8)
}

# Puts this process in a fresh job that allows breakaway, nested in the job it runs in: a box run starts its processes
# with CREATE_BREAKAWAY_FROM_JOB, which fails while the immediate job refuses breakaway, as the gate's own job does. The
# black-box suite's allow_breakaway does the same; every gate started afterwards starts in this job.
function Enter-BreakawayJob {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class CaptureBreakawayJob {
    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimits {
        public long PerProcessUserTimeLimit; public long PerJobUserTimeLimit; public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize; public UIntPtr MaximumWorkingSetSize; public uint ActiveProcessLimit;
        public UIntPtr Affinity; public uint PriorityClass; public uint SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IoCounters { public ulong Reads, Writes, Others, ReadBytes, WriteBytes, OtherBytes; }
    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimits {
        public BasicLimits Basic; public IoCounters Io; public UIntPtr ProcessMemoryLimit; public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed; public UIntPtr PeakJobMemoryUsed;
    }
    const int ExtendedLimitInformation = 9;
    const uint LimitBreakawayOk = 0x800;
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr CreateJobObjectW(IntPtr attributes, IntPtr name);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetInformationJobObject(IntPtr job, int kind, ref ExtendedLimits limits, uint size);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();

    // The job's handle is never closed: the job lasts as long as this process, which it holds.
    public static void Enter() {
        IntPtr job = CreateJobObjectW(IntPtr.Zero, IntPtr.Zero);
        if (job == IntPtr.Zero) { throw new Win32Exception(); }
        ExtendedLimits limits = new ExtendedLimits();
        limits.Basic.LimitFlags = LimitBreakawayOk;
        if (!SetInformationJobObject(job, ExtendedLimitInformation, ref limits, (uint)Marshal.SizeOf(limits))) { throw new Win32Exception(); }
        if (!AssignProcessToJobObject(job, GetCurrentProcess())) { throw new Win32Exception(); }
    }
}
'@
    [CaptureBreakawayJob]::Enter()
}

# The root hash a gate names its kernel objects under for the state root $Path: the first 8 hex digits of the SHA-256 of
# the path lower-cased, with backslashes and no trailing one.
function Get-RootHash {
    param([string]$Path)
    $normalized = $Path.Replace('/', '\').ToLowerInvariant().TrimEnd('\')
    $digest = [System.Security.Cryptography.SHA256]::HashData($utf8.GetBytes($normalized))
    return -join ($digest[0..3] | ForEach-Object { $_.ToString('x2') })
}

# Writes $Steps, one a line, as the stand-in file $Name in the checkout's ssh folder, which no snapshot sends; its path.
function Write-Step {
    param([string]$Name, [string[]]$Steps)
    $path = Join-Path $ssh $Name
    Write-Text -Path $path -Text (($Steps -join "`n") + "`n")
    return $path
}

# Starts a heavy gate in the checkout logging to .tmp\$Name.log, with $Seams before -- and $Command after it, the
# scratch LOCALAPPDATA and the variables $Variables; returns the process.
function Start-Gate {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'It starts a sandboxed gate in the scratch checkout, which the script waits on; there is nothing to confirm.')]
    param([string]$Name, [string[]]$Seams, [string[]]$Command, [hashtable]$Variables = @{})
    $info = [System.Diagnostics.ProcessStartInfo]::new($gate)
    $info.UseShellExecute = $false
    $info.WorkingDirectory = $repo
    $words = @('run', '--slot', 'heavy', '--timeout', '600', '--log', ".tmp\$Name.log", '--state-dir', $state,
        '--test-servers-under', '.', '--test-poll-ms', '50', '--test-git-limit-ms', '3600000') + $Seams + @('--') + $Command
    foreach ($word in $words) { $info.ArgumentList.Add($word) }
    foreach ($name in $inherited) { [void]$info.Environment.Remove($name) }
    $info.Environment['LOCALAPPDATA'] = $lad
    foreach ($name in $Variables.Keys) { $info.Environment[$name] = $Variables[$name] }
    return [System.Diagnostics.Process]::Start($info)
}

# Waits up to 180 s for the gate to end and checks its exit status.
function Wait-Gate {
    param([System.Diagnostics.Process]$Process, [string]$Name, [int]$Expected)
    if (-not $Process.WaitForExit(180000)) { throw "gate $Name (pid $($Process.Id)) did not end within 180 s" }
    if ($Process.ExitCode -ne $Expected) { throw "gate $Name exited $($Process.ExitCode), expected $Expected; see $repo\.tmp\$Name.log" }
    Write-Information "gate $Name (pid $($Process.Id)) exited $($Process.ExitCode)" -InformationAction Continue
}

# The journal's lines, read while gates may be appending to it.
function Read-Journal {
    if (-not (Test-Path -LiteralPath $journal)) { return @() }
    $stream = [System.IO.File]::Open($journal, 'Open', 'Read', 'ReadWrite, Delete')
    try { $text = [System.IO.StreamReader]::new($stream, $utf8).ReadToEnd() }
    finally { $stream.Dispose() }
    return @($text -split "`n" | Where-Object { $_ })
}

# Waits up to 120 s for the journal's take line of the gate $GatePid, its remote take for a routed gate.
function Wait-Take {
    param([int]$GatePid)
    $needle = "`"event`":`"take`",`"pid`":$GatePid,"
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    while (-not (Read-Journal | Where-Object { $_.Contains($needle) })) {
        if ($watch.Elapsed.TotalSeconds -ge 120) { throw "gate $GatePid journalled no take within 120 s" }
        Start-Sleep -Milliseconds 100
    }
}

# A field of a journal line, or $null when the line has none.
function Get-Field {
    param([object]$Line, [string]$Name)
    $property = $Line.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

# The journal's lines of the gate $GatePid, parsed.
function Get-OwnEvent {
    param([int]$GatePid)
    return @(Read-Journal | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.pid -eq $GatePid })
}

# Checks the journal of the gate $GatePid: one fallback with $Reason, or none when $Reason is empty; and a parent line
# when $Parent. Returns its event names, for the summary.
function Assert-Journal {
    param([string]$Name, [int]$GatePid, [string]$Reason, [switch]$Parent)
    $own = Get-OwnEvent -GatePid $GatePid
    $names = @($own | ForEach-Object { $_.event })
    $reasons = @($own | Where-Object { $_.event -eq 'fallback' } | ForEach-Object { Get-Field -Line $_ -Name 'reason' })
    $expected = if ($Reason) { @($Reason) } else { @() }
    if (($reasons -join ',') -ne ($expected -join ',')) {
        throw "gate $Name journalled fallbacks [$($reasons -join ', ')], expected [$($expected -join ', ')]: $($names -join ' ')"
    }
    if ($Parent.IsPresent -ne ($names -contains 'parent')) { throw "gate $Name journalled $($names -join ' '), parent expected: $Parent" }
    Write-Information "  journal of $Name`: $($names -join ' ')$(if ($Reason) { " (fallback $Reason)" })" -InformationAction Continue
}

# Runs a gate to its end and checks it as Wait-Gate and Assert-Journal do.
function Invoke-Gate {
    param([string]$Name, [string[]]$Seams, [string[]]$Command, [int]$Expected, [string]$Reason, [hashtable]$Variables = @{})
    $process = Start-Gate -Name $Name -Seams $Seams -Command $Command -Variables $Variables
    Wait-Gate -Process $process -Name $Name -Expected $Expected
    Assert-Journal -Name $Name -GatePid $process.Id -Reason $Reason
}

# A journal time shifted by $Shift, written as the gate writes it (UTC, seven fractional digits).
function Get-ShiftedTime {
    param([string]$Text, [TimeSpan]$Shift)
    $at = [DateTimeOffset]::Parse($Text, [cultureinfo]::InvariantCulture, [System.Globalization.DateTimeStyles]::AssumeUniversal)
    return $at.Add($Shift).UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.fffffff'Z'", [cultureinfo]::InvariantCulture)
}

# The captured journal's lines as the fixture holds them.
function ConvertTo-FixtureLine {
    param([string[]]$Lines, [int]$ParentPid, [string]$RootHash, [string]$Key)
    $escapedRepo = ($repo | ConvertTo-Json).Trim('"')
    $first = [regex]::Match($Lines[0], '"time":"([^"]+)"').Groups[1].Value
    $start = [DateTimeOffset]::Parse($firstTime, [cultureinfo]::InvariantCulture)
    $shift = $start - [DateTimeOffset]::Parse($first, [cultureinfo]::InvariantCulture)
    $pids = [ordered]@{}
    $pids["$ParentPid"] = 70000
    foreach ($line in $Lines) {
        $found = [regex]::Match($line, '"pid":(\d+)').Groups[1].Value
        if (-not $pids.Contains($found)) { $pids[$found] = 70000 + 100 * $pids.Count }
    }
    foreach ($line in $Lines) {
        $text = $line.Replace($escapedRepo, 'C:\\work\\repo').Replace("gate-$RootHash-", "gate-$rootPlaceholder-").Replace($Key, $keyPlaceholder)
        $text = [regex]::Replace($text, '"(\w+)":"(\d{4}-\d\d-\d\dT[^"]+Z)"', {
                param($match)
                '"{0}":"{1}"' -f $match.Groups[1].Value, (Get-ShiftedTime -Text $match.Groups[2].Value -Shift $shift)
            })
        $text = [regex]::Replace($text, '"(pid|parentPid)":(\d+)', {
                param($match)
                $known = $pids[$match.Groups[2].Value]
                if ($null -eq $known) { throw "no pid mapping for $($match.Value)" }
                '"{0}":{1}' -f $match.Groups[1].Value, $known
            })
        foreach ($leak in $stamp, $RootHash, $Key) {
            if ($text.Contains($leak)) { throw "a fixture line still holds the run's own '$leak': $text" }
        }
        $text
    }
}

# A PATH with the folder $First in front of this process's own, less every folder holding a dotnet when $NoDotnet.
function Get-PathWith {
    param([string]$First, [switch]$NoDotnet)
    $folders = @($env:PATH -split ';' | Where-Object { $_ })
    if ($NoDotnet) {
        $folders = @($folders | Where-Object {
                $folder = $_
                -not ('exe', 'cmd', 'bat', 'com' | Where-Object { Test-Path -LiteralPath (Join-Path $folder "dotnet.$_") -PathType Leaf })
            })
    }
    return (@($First) + $folders) -join ';'
}

& pwsh -NoProfile -WorkingDirectory $root -File (Join-Path $root 'tools\gate.ps1') -Log '.tmp/cap-remote-run-build.log' -TimeoutSeconds 900 -Slot heavy -- cargo build -p gate --features test-seams --profile test-release
if ($LASTEXITCODE -ne 0) { throw "the build failed (status $LASTEXITCODE); see $root\.tmp\cap-remote-run-build.log" }

Enter-BreakawayJob
New-Item -ItemType Directory -Force $state, (Join-Path $lad 'gate'), $ssh, (Join-Path $repo '.tmp') | Out-Null
& git -C $repo init -q
if ($LASTEXITCODE -ne 0) { throw "git init failed in $repo" }
Copy-Item -LiteralPath $fakeTool -Destination (Join-Path $repo 'tool.exe')
Write-Text -Path (Join-Path $repo '.gitignore') -Text "state/`nappdata/`n*.log`nbuilder/`nssh/`n.tmp/`n"
$routing = [ordered]@{ host = $builderHost; slots = 1; repos = @{ repo = @('*tool*') }; outputs = @{ repo = @() } }
Write-Text -Path (Join-Path $lad 'gate\remote.json') -Text ($routing | ConvertTo-Json -Compress -Depth 4)
$rootHash = Get-RootHash -Path $state
$scripted = { param([string]$Name, [string[]]$Steps) @('--test-remote-client', (Write-Step -Name "$Name.txt" -Steps $Steps)) }
$tool = @('.\tool.exe', 'out', 'ran here')
Write-Information "scratch checkout $repo, root hash $rootHash" -InformationAction Continue

# Pass 1, the scripted client. A remote run that holds the one remote slot until its go file exists, and a routed gate
# that falls back with full while it holds it.
$holderGo = Join-Path $ssh 'holder-go'
$holder = Start-Gate -Name 'remote-ok' -Seams (& $scripted 'remote-ok' @("wait $holderGo", 'out ran there', 'status 0')) -Command $tool
Wait-Take -GatePid $holder.Id
$fullSeams = (& $scripted 'full' @('status 0')) + @('--test-remote-wait-ms', '1000')
Invoke-Gate -Name 'full' -Seams $fullSeams -Command $tool -Expected 0 -Reason 'full'
Write-Text -Path $holderGo -Text ''
Wait-Gate -Process $holder -Name 'remote-ok' -Expected 0
Assert-Journal -Name 'remote-ok' -GatePid $holder.Id -Reason ''

# One routed gate refused after its remote take per reason; the parity one's command fails here.
foreach ($reason in 'unreachable', 'busy', 'parity', 'rebooted', 'tools') {
    $status = if ($reason -eq 'parity') { 1 } else { 0 }
    $command = if ($reason -eq 'parity') { @('.\tool.exe', 'out', 'ran here', 'exit', '1') } else { $tool }
    Invoke-Gate -Name "refused-$reason" -Seams (& $scripted "refused-$reason" @("refuse $reason")) -Command $command -Expected $status -Reason $reason
}

# A routed gate under this process's light marker, refused once it holds its remote slot. Before the refusal this process
# opens the scratch root's critical mutex, so a critical request is pending as the gate looks for a local slot, and its
# light parent lends it its own: it journals parent and takes no slot.
$parentGo = Join-Path $ssh 'parent-go'
$started = [DateTimeOffset]::new((Get-Process -Id $PID).StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
$marker = "light:${PID}:${started}:$rootHash"
$parentSeams = (& $scripted 'parent' @("wait $parentGo", 'refuse busy')) + @('--test-parent-job-trusted')
$parent = Start-Gate -Name 'parent' -Seams $parentSeams -Command $tool -Variables @{ GATE_SLOT_HELD = $marker }
Wait-Take -GatePid $parent.Id
$critical = [System.Threading.Mutex]::new($false, "Local\gate-$rootHash-critical")
try {
    Write-Text -Path $parentGo -Text ''
    Wait-Gate -Process $parent -Name 'parent' -Expected 0
}
finally { $critical.Dispose() }
Assert-Journal -Name 'parent' -GatePid $parent.Id -Reason 'busy' -Parent

# A local control gate: its command matches no routing pattern.
Invoke-Gate -Name 'local' -Seams @() -Command @('cmd', '/c', 'exit', '0') -Expected 0 -Reason ''

# Pass 2, the Rust client over the local transport, each ssh call through fake-ssh and its steps file.
$client = @('--test-remote-transport', "local:$builder", '--test-remote-ssh', $fakeSsh, '--test-remote-retry-pause-ms', '1')
$lost = $client + @('--test-remote-bound-ms', '600000')
$sshVariables = { param([string]$Name, [string[]]$Steps) @{ GATE_FAKE_SSH_STEPS = (Write-Step -Name "$Name.steps" -Steps $Steps); GATE_FAKE_SSH_GATE = $gate } }

Invoke-Gate -Name 'client-unreachable' -Seams $client -Command $tool -Expected 0 -Reason 'unreachable' `
    -Variables (& $sshVariables 'unreachable' @('fail-banner', 'fail-banner'))

$rebootedSteps = @('boot 2026-10-10T08:00:00.0000000Z', 'drop', 'ok', 'boot 2026-10-10T08:01:00.0000000Z')
Invoke-Gate -Name 'client-rebooted' -Seams $lost -Command $tool -Expected 0 -Reason 'rebooted' `
    -Variables (& $sshVariables 'rebooted' $rebootedSteps)

# The builder's dotnet fails the restore of the snapshot's tool manifest, so the box refuses the run.
$manifest = Join-Path $repo '.config\dotnet-tools.json'
New-Item -ItemType Directory -Force (Split-Path $manifest), (Join-Path $ssh 'stub-tools') | Out-Null
Write-Text -Path $manifest -Text "{`n  `"version`": 1`n}`n"
Write-Text -Path (Join-Path $ssh 'stub-tools\dotnet.cmd') -Text "@exit /b 1`r`n"
$toolsVariables = & $sshVariables 'tools' @('ok', 'ok')
$toolsVariables['PATH'] = Get-PathWith -First (Join-Path $ssh 'stub-tools')
Invoke-Gate -Name 'client-tools' -Seams $client -Command $tool -Expected 0 -Reason 'tools' -Variables $toolsVariables
Remove-Item -LiteralPath $manifest
Remove-Item -LiteralPath (Split-Path $manifest)

# A global.json pin the builder's only SDK, as its stub dotnet lists it, does not serve.
$pin = Join-Path $repo 'global.json'
New-Item -ItemType Directory -Force (Join-Path $ssh 'stub-dotnet') | Out-Null
Write-Text -Path $pin -Text '{"sdk": {"version": "999.0.0"}}'
Write-Text -Path (Join-Path $ssh 'stub-dotnet\dotnet.cmd') -Text "@echo off`r`necho 8.0.404 [C:\stub]`r`n"
$parityVariables = & $sshVariables 'parity' @('ok', 'ok')
$parityVariables['PATH'] = Get-PathWith -First (Join-Path $ssh 'stub-dotnet') -NoDotnet
Invoke-Gate -Name 'client-parity' -Seams $client -Command $tool -Expected 0 -Reason 'parity' -Variables $parityVariables
Remove-Item -LiteralPath $pin

# The builder's lock of the checkout's work folder, held here as another run on the builder holds it.
$queued = Read-Journal | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { Get-Field -Line $_ -Name 'key' } | Select-Object -First 1
$key = $queued.key
New-Item -ItemType Directory -Force (Join-Path $builder 'work') | Out-Null
$lock = [System.IO.File]::Open((Join-Path $builder "work\$key.lock"), 'OpenOrCreate', 'ReadWrite', 'None')
try {
    Invoke-Gate -Name 'client-busy' -Seams $client -Command $tool -Expected 0 -Reason 'busy' -Variables (& $sshVariables 'busy' @('ok', 'ok'))
}
finally { $lock.Dispose() }

Invoke-Gate -Name 'client-version' -Seams $client -Command $tool -Expected 0 -Reason 'version' `
    -Variables (& $sshVariables 'version' @('version 0.0.0-other'))

$lines = Read-Journal
$fixtureLines = @(ConvertTo-FixtureLine -Lines $lines -ParentPid $PID -RootHash $rootHash -Key $key)
Write-Text -Path $fixture -Text (($fixtureLines -join "`n") + "`n")
Write-Information "captured $($lines.Count) lines from $journal into $fixture" -InformationAction Continue
