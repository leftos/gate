<#
.SYNOPSIS
Re-captures journal.jsonl from three gate.exe runs.

.DESCRIPTION
Builds gate.exe through the repo's gate, then runs three light gates in a scratch state root, their commands
`cmd /c exit 0`, `cmd /c exit 3` and `cmd /c exit 0`, so no live journal or mutex is touched. The journal.jsonl in the
repo came from an early gate.exe that launched no command and took no slot; a re-capture with this script gives takes
naming real slots and non-zero held and wall times, so the figures captured_journal_is_read_into_runs in fixtures.rs
expects change with it. Writes journal.jsonl beside
this script with LF line ends, the scratch folder's path as C:\work\repo, and the hand-written queue and abandon lines of
pid 58200 after the captured ones.

.PARAMETER Scratch
The folder the gates run in; default .tmp\cap-journal\run-<stamp> under the repo, left for you to delete.
#>
param([string]$Scratch)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..\..')).Path
if (-not $Scratch) {
    $stamp = [DateTime]::UtcNow.ToString("yyyyMMdd'T'HHmmss'Z'", [cultureinfo]::InvariantCulture)
    $Scratch = Join-Path $repo ".tmp\cap-journal\run-$stamp"
}
$null = New-Item -ItemType Directory -Force $Scratch
$Scratch = (Resolve-Path $Scratch).Path

& pwsh -NoProfile -WorkingDirectory $repo -File (Join-Path $repo 'tools\gate.ps1') -Log '.tmp/cap-journal-build.log' -TimeoutSeconds 900 -Slot heavy -- cargo build -p gate --profile test-release
if ($LASTEXITCODE -ne 0) { throw "the build failed (status $LASTEXITCODE); see $repo\.tmp\cap-journal-build.log" }

$gate = Join-Path $repo 'target\test-release\gate.exe'
$root = Join-Path $Scratch 'state'
$runs = @(
    @{ Log = '.tmp\f1.log'; Status = 0 },
    @{ Log = '.tmp\f2.log'; Status = 3 },
    @{ Log = '.tmp\f3.log'; Status = 0 }
)
$env:GATE_SLOT_HELD = $null
Push-Location $Scratch
try {
    foreach ($run in $runs) {
        & $gate run --slot light --timeout 60 --log $run.Log --state-dir $root -- cmd /c exit $run.Status
        if ($LASTEXITCODE -ne $run.Status) { throw "the gate for status $($run.Status) exited $LASTEXITCODE" }
    }
}
finally {
    Pop-Location
}

$escaped = ($Scratch | ConvertTo-Json).Trim('"')
$captured = [IO.File]::ReadAllText((Join-Path $root 'events.jsonl')).Replace($escaped, 'C:\\work\\repo')
$handWritten = @(
    '{"v":1,"time":"2026-10-03T05:04:44.3000000Z","event":"queue","pid":58200,"processStart":"2026-10-03T05:04:43.9000000Z","kind":"light","cwd":"C:\\work\\repo","command":"cmd /c exit 0","log":"C:\\work\\repo\\.tmp\\f5.log"}',
    '{"v":1,"time":"2026-10-03T05:04:46.0000000Z","event":"abandon","pid":58200,"processStart":"2026-10-03T05:04:43.9000000Z","kind":"light","cwd":"C:\\work\\repo","command":"cmd /c exit 0","waitedSeconds":1.7}'
)
$text = $captured + (($handWritten | ForEach-Object { "$_`n" }) -join '')
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'journal.jsonl'), $text, [Text.UTF8Encoding]::new($false))
Write-Output "wrote $(Join-Path $PSScriptRoot 'journal.jsonl') from $root"
