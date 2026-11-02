#Requires -Version 7.3

<#
.SYNOPSIS
    Smoke-tests an installed gate.exe by running the gate from a Git Bash script and from pwsh.

.DESCRIPTION
    A gate started from a Git Bash script reaches it through MSYS's fork stub, a path the test suite never takes: its
    shells are real Windows children. An install can therefore pass the whole suite and still refuse every gate a Git
    Bash script starts. This script installs nothing. It runs the repo's own tools/gate.ps1 launcher on `git --version`
    twice, once from a script run the way a caller's script runs - Git's own bash forking another bash, both through the
    stub - and once from pwsh directly, and checks that each run exits 0, says its command's output and ends with the
    gate's own passed line, so a launcher that ran the command without a gate is never a pass. Both runs use `-Slot
    none`, so a busy machine's queue never fails the smoke, and each child runs without the caller's `GATE_` variables,
    as a top-level caller's gate would.

    Git's bash is `usr\bin\bash.exe` in the first folder at or above the folder of the `git.exe` on PATH that holds one;
    with none, the script fails before either run. The gate is `%LOCALAPPDATA%\gate\bin\gate.exe`, which must answer
    --version before either run starts. Each of the three programs runs with its output redirected and is given 120 s,
    its whole tree killed past that. The runs write their script and logs under `<repo>\.tmp\smoke`. The script prints
    one verdict line and exits 0 or 1:

        smoke: passed (git bash, pwsh) with gate <version>
        smoke: FAILED (git bash|pwsh|both): <the failed runs' last three output lines, joined by " | ">

    It installs nothing and changes nothing outside `.tmp\smoke`.

.EXAMPLE
    pwsh -NoProfile -File tools/smoke-install.ps1
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$smokeDir = Join-Path $repoRoot '.tmp\smoke'
$ceilingSeconds = 120

# `$Text` as one word of a bash script or a bash -c line: in single quotes, a `'` in it written as `'\''`.
function ConvertTo-BashWord {
    param([string]$Text)
    return "'" + $Text.Replace("'", "'\''") + "'"
}

# The first folder at or above `$From` that holds `usr\bin\bash.exe`, or $null when the walk reaches the root without
# one.
function Find-BashAbove {
    param([string]$From)
    $folder = $From
    while ($folder) {
        $bash = Join-Path $folder 'usr\bin\bash.exe'
        if (Test-Path -LiteralPath $bash -PathType Leaf) { return $bash }
        $above = Split-Path -Parent $folder
        if (-not $above -or $above -eq $folder) { return $null }
        $folder = $above
    }
    return $null
}

# A program started with this process's environment minus every `GATE_` variable, its stdout and stderr read as they come
# and its whole tree killed when it outlives `$ceilingSeconds`: Status, its exit code, $null for a run that timed out;
# Output, its standard output's lines and then its standard error's; TimedOut, whether it had to be killed.
function Invoke-Clean {
    param([string]$File, [string[]]$Arguments)
    $start = [System.Diagnostics.ProcessStartInfo]::new($File)
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.Environment.Clear()
    foreach ($entry in [System.Environment]::GetEnvironmentVariables().GetEnumerator()) {
        if ([string]$entry.Key -like 'GATE_*') { continue }
        $start.Environment[[string]$entry.Key] = [string]$entry.Value
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    $null = $process.Start()
    $outTask = $process.StandardOutput.ReadToEndAsync()
    $errTask = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit($ceilingSeconds * 1000)) {
        $process.Kill($true)
        $process.WaitForExit()
        $null = $outTask.Wait(5000)
        $null = $errTask.Wait(5000)
        $process.Dispose()
        return @{ Status = $null; Output = @(); TimedOut = $true }
    }
    $lines = [System.Collections.Generic.List[string]]::new()
    foreach ($line in ($outTask.GetAwaiter().GetResult() -split "`r?`n")) { $lines.Add($line) }
    foreach ($line in ($errTask.GetAwaiter().GetResult() -split "`r?`n")) { $lines.Add($line) }
    $status = $process.ExitCode
    $process.Dispose()
    return @{ Status = $status; Output = $lines.ToArray(); TimedOut = $false }
}

# A run's output as a verdict reads it: Passed when it exited 0, said `git version` and ended with the gate's own passed
# line, and Tail, its last three lines that say something, joined by " | ", for a failed run's verdict. A run that
# outlived the ceiling failed, with the ceiling as its tail.
function Get-RunResult {
    param([string[]]$Output, [int]$Status, [bool]$TimedOut)
    if ($TimedOut) { return @{ Passed = $false; Tail = "timed out after $ceilingSeconds s" } }
    $lines = @($Output | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    $text = $lines -join "`n"
    $from = [Math]::Max(0, $lines.Count - 3)
    $tail = if ($lines.Count -gt 0) { ($lines[$from..($lines.Count - 1)]) -join ' | ' } else { '(no output)' }
    $passed = ($Status -eq 0) -and $text.Contains('git version') -and [bool]($lines | Where-Object { $_ -match '^gate: passed' })
    return @{ Passed = $passed; Tail = $tail }
}

try {
    $command = Get-Command git.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    $git = if ($command) { $command.Source } else { $null }
    if (-not $git) {
        Write-Output 'smoke: FAILED (git bash): no usr\bin\bash.exe beside git.exe on PATH'
        exit 1
    }
    $bash = Find-BashAbove -From (Split-Path -Parent $git)
    if (-not $bash) {
        Write-Output "smoke: FAILED (git bash): no usr\bin\bash.exe above $git"
        exit 1
    }

    if (-not $env:LOCALAPPDATA) {
        Write-Output 'smoke: FAILED (both): LOCALAPPDATA is not set'
        exit 1
    }
    $gateExe = Join-Path $env:LOCALAPPDATA 'gate\bin\gate.exe'
    if (-not (Test-Path -LiteralPath $gateExe -PathType Leaf)) {
        Write-Output "smoke: FAILED (both): no working gate.exe at $gateExe"
        exit 1
    }
    $answered = Invoke-Clean -File $gateExe -Arguments @('--version')
    $answeredLines = @($answered.Output | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($answered.TimedOut -or $answered.Status -ne 0 -or $answeredLines.Count -eq 0) {
        $reason = "no working gate.exe at $gateExe"
        if ($answered.TimedOut) { $reason += ": timed out after $ceilingSeconds s" }
        elseif ($answeredLines.Count -gt 0) { $reason += ": $($answeredLines[-1])" }
        Write-Output "smoke: FAILED (both): $reason"
        exit 1
    }
    $version = $answeredLines[0]
    if ($version.StartsWith('gate ')) { $version = $version.Substring(5) }

    $null = New-Item -ItemType Directory -Force -Path $smokeDir
    $launcher = Join-Path $repoRoot 'tools\gate.ps1'
    $smokeSh = Join-Path $smokeDir 'smoke.sh'
    $bashLog = Join-Path $smokeDir 'bash.log'
    $runLine = "pwsh -NoProfile -File $(ConvertTo-BashWord $launcher.Replace('\', '/')) -Log " +
        "$(ConvertTo-BashWord $bashLog.Replace('\', '/')) -TimeoutSeconds 60 -Slot none -- git --version"
    [System.IO.File]::WriteAllText($smokeSh, (@($runLine, 'exit $?') -join "`n") + "`n",
        [System.Text.UTF8Encoding]::new($false))

    # Git's bash runs the script with a bash of its own, as a caller's script does: two commands, so the outer bash forks
    # the inner one through MSYS's stub rather than executing it in its own place, and the outer's status is kept.
    $bashLine = "/usr/bin/bash $(ConvertTo-BashWord $smokeSh.Replace('\', '/'))" + '; exit $?'
    $bashRun = Invoke-Clean -File $bash -Arguments @('-c', $bashLine)

    $pwshExe = (Get-Command pwsh -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $pwshLog = Join-Path $smokeDir 'pwsh.log'
    $pwshRun = Invoke-Clean -File $pwshExe -Arguments @('-NoProfile', '-File', $launcher, '-Log', $pwshLog,
        '-TimeoutSeconds', '60', '-Slot', 'none', '--', 'git', '--version')

    $bashResult = Get-RunResult -Output $bashRun.Output -Status $bashRun.Status -TimedOut $bashRun.TimedOut
    $pwshResult = Get-RunResult -Output $pwshRun.Output -Status $pwshRun.Status -TimedOut $pwshRun.TimedOut

    if ($bashResult.Passed -and $pwshResult.Passed) {
        Write-Output "smoke: passed (git bash, pwsh) with gate $version"
        exit 0
    }
    $failed = [System.Collections.Generic.List[string]]::new()
    $tails = [System.Collections.Generic.List[string]]::new()
    if (-not $bashResult.Passed) {
        $failed.Add('git bash')
        $tails.Add($bashResult.Tail)
    }
    if (-not $pwshResult.Passed) {
        $failed.Add('pwsh')
        $tails.Add($pwshResult.Tail)
    }
    $label = if ($failed.Count -eq 2) { 'both' } else { $failed[0] }
    Write-Output "smoke: FAILED ($label): $($tails -join ' | ')"
    exit 1
}
catch {
    Write-Output "smoke: FAILED (both): $($_.Exception.Message)"
    exit 1
}
