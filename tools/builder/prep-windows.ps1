#Requires -Version 7.3
# Prepares a Windows machine to be a gate builder: runs the step scripts in prep\windows, listed in
# prep\windows\steps.psd1, in manifest order. Run it elevated, at the console or over ssh -t for the menu.
# -List prints the steps and what skipping each one costs. With neither -Defaults nor -Steps it shows a numbered toggle
# menu; -Defaults runs the ticked defaults and -Steps id,... the named steps, both with no menu. -DryRun has each step
# print what it would change and change nothing. -Yes skips the "run these steps?" question -Defaults and -Steps ask
# at a console; it never skips dev-drive's own FORMAT question.
# -User names the builder account, -PublicKeyFile the PC's public key for sshd, -GateRoot the gate's work root and
# -DiskNumber the empty disk for the Dev Drive. -TestInputFile feeds the menu's typed lines from a file, and only the
# test suite may use it (GATE_PREP_TEST=1).
# A step talks back on stdout with whole lines: REBOOT asks for a reboot, REASON <text> says why it failed and
# NOTE <text> is logged as it is. Every step is logged to C:\build-box\prep.log and each finished one recorded in
# C:\build-box\prep-state.json; a step that needs a reboot stops the run, and the next run shows the finished steps
# as done. Exits 0 when nothing failed, 1 when a step failed or the session is not elevated, 2 on a bad argument.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '', Justification = 'the menu is drawn on the console')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseDeclaredVarsMoreThanAssignments', '',
    Justification = 'Invoke-PrepStep reads the pipeline''s flags after it')]
[CmdletBinding()]
param(
    [string]$User,
    [string]$PublicKeyFile,
    [string]$GateRoot = 'D:\gate-remote',
    [int]$DiskNumber,
    [switch]$List,
    [switch]$Defaults,
    [string[]]$Steps,
    [switch]$DryRun,
    [switch]$Yes,
    [string]$TestInputFile
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$bound = $PSBoundParameters
$stepDir = Join-Path $PSScriptRoot 'prep\windows'
$buildBox = 'C:\build-box'
$logPath = Join-Path $buildBox 'prep.log'
$statePath = Join-Path $buildBox 'prep-state.json'
$rebootLine = 'prep: reboot, then run prep again; finished steps show as done'
$manifest = @((Import-PowerShellDataFile -LiteralPath (Join-Path $stepDir 'steps.psd1')).Steps)
# powershell -File hands a comma list over as one word.
$Steps = @($Steps | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$testLines = $null

function Exit-Prep([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("prep: $Message")
    exit $Code
}

function Show-StepList {
    '{0,3}  {1,-19}{2,-38}{3,-9}{4,-24}{5}' -f '#', 'id', 'name', 'default', 'flags', 'caveat if skipped'
    for ($i = 0; $i -lt $manifest.Count; $i++) {
        $step = $manifest[$i]
        $flags = @(foreach ($flag in 'Reboot', 'Destructive', 'Interactive') { if ($step[$flag]) { $flag.ToLowerInvariant() } }) -join ','
        $default = if ($step.Default) { 'yes' } else { 'no' }
        '{0,3}  {1,-19}{2,-38}{3,-9}{4,-24}{5}' -f ($i + 1), $step.Id, $step.Name, $default, $flags, $step.Caveat
    }
}

# A typed line from the console, or from -TestInputFile; $null when the input has ended.
function Read-PrepLine {
    if ($null -ne $testLines) {
        if ($testLines.Count) { return $testLines.Dequeue() }
        return $null
    }
    return [Console]::ReadLine()
}

function Show-Menu([bool[]]$Ticked, [System.Collections.IDictionary]$Done) {
    $width = [Math]::Max(26, (@($manifest | ForEach-Object { $_.Name.Length }) | Measure-Object -Maximum).Maximum + 2)
    Write-Host "gate builder prep  (elevated: yes, user: $User)"
    Write-Host ''
    Write-Host ('  #  [x] {0}caveat if skipped' -f 'step'.PadRight($width))
    for ($i = 0; $i -lt $manifest.Count; $i++) {
        $step = $manifest[$i]
        if ($Done.Contains($step.Id) -and -not $Ticked[$i]) {
            Write-Host ('{0,3}  [done] {1}' -f ($i + 1), $step.Name)
            continue
        }
        $mark = if ($Ticked[$i]) { '[x]' } else { '[ ]' }
        Write-Host ('{0,3}  {1} {2}{3}' -f ($i + 1), $mark, $step.Name.PadRight($width), $step.Caveat)
    }
    Write-Host 'toggle: numbers (e.g. 4 11), a=all, d=defaults, Enter=run, q=quit'
}

# The ticks for 'a' (every step) or 'd' (the defaults), leaving done steps unticked.
function Get-Tick([System.Collections.IDictionary]$Done, [switch]$All) {
    return , [bool[]]@($manifest | ForEach-Object { ($All -or $_.Default) -and -not $Done.Contains($_.Id) })
}

# The first token of $Tokens that is not a step number, or $null.
function Get-BadToken([string[]]$Tokens) {
    foreach ($token in $Tokens) {
        $number = 0
        if (-not [int]::TryParse($token, [ref]$number) -or $number -lt 1 -or $number -gt $manifest.Count) { return $token }
    }
    return $null
}

# The ticks after one typed line that is not Enter or q: a, d, or step numbers to toggle. A bad token prints its line
# and changes nothing.
function Get-ToggledTick([bool[]]$Ticked, [string]$Line, [System.Collections.IDictionary]$Done) {
    if ($Line -eq 'a') { return , (Get-Tick $Done -All) }
    if ($Line -eq 'd') { return , (Get-Tick $Done) }
    $tokens = @($Line -split '[\s,]+' | Where-Object { $_ })
    $bad = Get-BadToken $tokens
    if ($null -ne $bad) {
        Write-Host "prep: '$bad' is not a step number, a, d or q"
        return , $Ticked
    }
    foreach ($token in $tokens) { $Ticked[[int]$token - 1] = -not $Ticked[[int]$token - 1] }
    return , $Ticked
}

# Shows the menu until Enter and returns the ticked ids; q exits 0.
function Select-FromMenu([System.Collections.IDictionary]$Done) {
    $ticked = Get-Tick $Done
    while ($true) {
        Show-Menu $ticked $Done
        Write-Host -NoNewline '> '
        $line = Read-PrepLine
        if ($null -eq $line) { Exit-Prep 2 'the input ended before Enter; nothing ran' }
        $line = $line.Trim()
        if ($line -eq '') { break }
        if ($line -eq 'q') { exit 0 }
        $ticked = Get-ToggledTick -Ticked $ticked -Line $line -Done $Done
    }
    return @(for ($i = 0; $i -lt $manifest.Count; $i++) { if ($ticked[$i]) { $manifest[$i].Id } })
}

# The finished steps recorded in the state file, id to time.
function Read-PrepState {
    if (-not (Test-Path -LiteralPath $statePath)) { return [ordered]@{} }
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json -AsHashtable
    if ($state -isnot [System.Collections.IDictionary] -or $state['done'] -isnot [System.Collections.IDictionary]) {
        Exit-Prep 1 "$statePath is not { ""done"": { ""<id>"": ""<time>"" } }; fix it or delete it to start over"
    }
    return $state['done']
}

function Write-PrepLog([string]$Message) {
    $line = '{0:s} {1}' -f (Get-Date), $Message
    if (-not $DryRun) { Add-Content -LiteralPath $logPath -Value $line }
    Write-Host $line
}

# Runs one step script in this session with the parameters it declares, and reads its REBOOT, REASON and NOTE lines.
function Invoke-PrepStep([System.Collections.IDictionary]$Step) {
    $path = Join-Path $stepDir "$($Step.Id).ps1"
    $declared = @((Get-Command -Name $path).Parameters.Keys)
    $arguments = @{}
    foreach ($name in 'User', 'PublicKeyFile', 'GateRoot', 'DiskNumber') {
        $given = $bound.ContainsKey($name) -or $name -eq 'GateRoot'
        if ($name -in $declared -and $given) { $arguments[$name] = Get-Variable -Name $name -ValueOnly }
    }
    if ($DryRun -and 'DryRun' -in $declared) { $arguments['DryRun'] = $true }
    $reboot = $false
    $reason = $null
    $global:LASTEXITCODE = 0
    & $path @arguments | ForEach-Object {
        $line = "$_"
        if ($line -ceq 'REBOOT') { $reboot = $true }
        elseif ($line -clike 'REASON *') { $reason = $line.Substring(7) }
        elseif ($line -clike 'NOTE *') { Write-PrepLog "NOTE $($Step.Id): $($line.Substring(5))" }
        else { Write-Host $line }
    }
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Reboot = $reboot; Reason = $reason }
}

if ($List) {
    Show-StepList
    exit 0
}
if ($TestInputFile -and $env:GATE_PREP_TEST -ne '1') { Exit-Prep 2 '-TestInputFile is for the test suite only; type at the menu instead' }
if ($TestInputFile) { $testLines = [System.Collections.Generic.Queue[string]]::new([string[]]@(Get-Content -LiteralPath $TestInputFile)) }
$hasConsole = [bool]$TestInputFile -or -not [Console]::IsInputRedirected
if (-not $User) { Exit-Prep 2 '-User is required: the builder account the steps set up, e.g. -User builder' }
if ($Defaults -and $Steps) { Exit-Prep 2 '-Steps and -Defaults do not go together; give one of them' }
$ids = @($manifest | ForEach-Object { $_.Id })
foreach ($id in $Steps) {
    if ($id -notin $ids) { Exit-Prep 2 "unknown step '$id'; the steps are: $($ids -join ', ')" }
}
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Prep 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }
$menu = -not $Defaults -and -not $Steps
if ($menu -and -not $hasConsole) {
    Exit-Prep 2 'no console for the menu; run it at the console, over ssh -t, or with -Defaults or -Steps'
}
$done = Read-PrepState
$selected = if ($Steps) { $Steps }
elseif ($Defaults) { @($manifest | Where-Object { $_.Default } | ForEach-Object { $_.Id }) }
else { Select-FromMenu $done }
$run = @($manifest | Where-Object { $_.Id -in $selected })
foreach ($step in $run) {
    foreach ($need in $step.Needs) {
        if (-not $bound.ContainsKey($need)) { Exit-Prep 2 "step $($step.Id) needs -$need; give it or leave the step out" }
    }
    if ($step.Destructive -and -not $DryRun -and -not $hasConsole) {
        Exit-Prep 2 "step $($step.Id) asks you to type FORMAT first; run it at the console or over ssh -t"
    }
}
if (-not $menu -and $hasConsole -and -not $Yes -and $run) {
    Write-Host "prep: run $(($run | ForEach-Object { $_.Id }) -join ', ')? [y/N]"
    if ((Read-PrepLine) -notmatch '^\s*y(es)?\s*$') { Exit-Prep 1 'not confirmed; nothing ran' }
}
if (-not $DryRun -and -not (Test-Path -LiteralPath $buildBox)) { New-Item -ItemType Directory -Path $buildBox | Out-Null }
$failed = @()
foreach ($step in $run) {
    Write-PrepLog "BEGIN $($step.Id)"
    if ($Defaults -and $done.Contains($step.Id)) { Write-PrepLog "SKIP $($step.Id): done at $($done[$step.Id])"; continue }
    try { $result = Invoke-PrepStep $step }
    catch { $result = [pscustomobject]@{ ExitCode = -1; Reboot = $false; Reason = $_.Exception.Message } }
    if ($result.ExitCode -ne 0) {
        $why = if ($result.Reason) { $result.Reason } else { "exited $($result.ExitCode)" }
        Write-PrepLog "FAIL $($step.Id): $why"
        $failed += $step.Id
        continue
    }
    Write-PrepLog "OK $($step.Id)"
    if (-not $DryRun) {
        $done[$step.Id] = (Get-Date).ToString('s')
        Set-Content -LiteralPath $statePath -Value (@{ done = $done } | ConvertTo-Json)
    }
    if ($result.Reboot -and -not $DryRun) {
        Write-Output $rebootLine
        break
    }
}
Write-PrepLog "COMPLETE failed=$($failed.Count)"
if ($failed) { exit 1 }
exit 0
