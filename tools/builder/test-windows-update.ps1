#Requires -Version 7
# Case table for windows-update.ps1, the builder's drained Windows Update install. The script is dot-sourced once and
# its seam functions are replaced with recording stand-ins: a fake clock whose Wait-Seconds advances it, a drain verb
# the case scripts by action, a fake install handle whose PowerShell answers EndInvoke from the case's scripted result
# and error records, and event recording. Each case records the order of the calls and reads it back; no case sleeps or
# reads the wall clock. Prints ok/FAIL per case and exits with the failure count.
#
#   pwsh -NoProfile -File tools/builder/test-windows-update.ps1
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceScript = Join-Path $PSScriptRoot 'windows-update.ps1'
$script:fails = 0

function Test-Case {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Check)
    try {
        $problem = & $Check
    }
    catch {
        $problem = "threw: $($_.Exception.Message)"
    }
    if ("$problem" -like 'skipped: *') {
        "  skip {0}: {1}" -f $Name, $problem
    }
    elseif ($problem) {
        "  FAIL {0}: {1}" -f $Name, $problem
        $script:fails++
    }
    else { "  ok   {0}" -f $Name }
}

if (-not (Test-Path -LiteralPath $sourceScript)) { throw "$sourceScript is missing" }
. $sourceScript
# The script turns module auto-loading off for its own run, under which a module this test has not loaded yet would not
# load; the test runs with the default.
$PSModuleAutoLoadingPreference = 'All'

# A real error record for the install handle cases.
$oneError = $null
try { throw 'an error record' } catch { $oneError = $_ }

# The script's seams, replaced for every case: the clock, the work root, the drain verb, the update search, the install
# handle and the event log.
function Get-Now { return $script:fakeNow }
function Wait-Seconds([int]$Seconds) { $script:fakeNow = $script:fakeNow.AddSeconds($Seconds) }
function Read-GateRoot { return $script:fakeRoot }

# Answers from the scriptblock the case sets as $script:onDrain, which takes the action and returns the result.
function Invoke-BoxDrain([string]$Root, [string]$Action) {
    if (-not $script:onDrain) { throw "the case scripted no drain answer for '$Action'" }
    $result = & $script:onDrain $Action
    if ($Action -eq 'status') {
        $script:events.Add("status:$($result.State)")
    }
    else {
        $script:events.Add($Action)
        if ($Action -eq 'raise') {
            $script:raiseTimes.Add($script:fakeNow)
            if ($script:installed) { $script:installRaises++ }
        }
    }
    return $result
}

# Plain update data, as the script's own search returns it: an identity and a title, never a COM object.
function Get-PendingUpdate {
    $script:events.Add('find')
    return , @($script:updates)
}

# A handle holding the case's result, or none when the case asks for that, and its error records.
function Start-UpdateInstall($Updates) {
    $script:events.Add('install')
    $script:installed = $true
    $script:installIds = @($Updates | ForEach-Object { $_.Id })
    $script:installOutput = @()
    if (-not $script:installNoResult) {
        $script:installOutput = @(@{
                ResultCode = $script:resultCode
                RebootRequired = $script:rebootRequired
                Titles = $script:resultTitles
            })
    }
    $powerShell = [pscustomobject]@{ Streams = [pscustomobject]@{ Error = @($script:installErrors) } }
    $powerShell | Add-Member -MemberType ScriptMethod -Name EndInvoke -Value {
        param($Async)
        if ($script:installThrowsOnEnd) { throw [Exception]::new('WU_E_FAKE 0x80240001') }
        return $script:installOutput
    }
    return @{ PowerShell = $powerShell; Async = $null; Polls = 0; DoneAfter = $script:installDoneAfter }
}

function Test-UpdateInstallDone($Handle) {
    $Handle.Polls = $Handle.Polls + 1
    $script:installPolls = $Handle.Polls
    return ($Handle.Polls -gt $Handle.DoneAfter)
}

function Test-RebootPending { return $script:rebootPending }

function Write-UpdateEvent([string]$Message, [string]$Type) {
    $script:updateEvents.Add(@{ Message = $Message; Type = $Type })
}

# Every recording stand-in's state, back to its start, before each case.
function Reset-Fakes {
    $script:startTime = [DateTime]::new(2026, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
    $script:fakeNow = $script:startTime
    $script:fakeRoot = 'D:\gate-remote'
    $script:events = [System.Collections.Generic.List[string]]::new()
    $script:raiseTimes = [System.Collections.Generic.List[object]]::new()
    $script:updateEvents = [System.Collections.Generic.List[object]]::new()
    $script:onDrain = $null
    $script:statusCount = 0
    $script:raiseCount = 0
    $script:raiseFailures = 0
    $script:installFailCount = 0
    $script:drainedAt = 0
    $script:installed = $false
    $script:installRaises = 0
    $script:installPolls = 0
    $script:installIds = @()
    $script:updates = @(@{ Id = 'id-1'; Title = 'Security Update' })
    $script:installDoneAfter = 0
    $script:installNoResult = $false
    $script:installThrowsOnEnd = $false
    $script:installErrors = @()
    $script:resultCode = 2
    $script:rebootRequired = $false
    $script:resultTitles = @('Security Update (result 2)')
    $script:rebootPending = $false
}

# The fake seconds at which the drain verb was raised.
function Get-RaiseSeconds {
    return @($script:raiseTimes | ForEach-Object { [int](($_ - $script:startTime).TotalSeconds) })
}

# '' when the calls ran in exactly $Expected order, else what they were.
function Get-EventOrderProblem([string[]]$Expected) {
    $got = @($script:events)
    if (($got -join ' | ') -ne ($Expected -join ' | ')) { return "the calls were: $($got -join ' | ')" }
    return ''
}

# '' when the last call recorded was a lift, else the calls.
function Get-LiftProblem {
    if ($script:events.Count -eq 0 -or $script:events[$script:events.Count - 1] -ne 'lift') {
        return "the last call was not a lift: $($script:events -join ' | ')"
    }
    return ''
}

# The one event of $Type the script wrote, or a problem naming what it wrote instead.
function Get-SingleEvent([string]$Type, [ref]$Event) {
    $found = @($script:updateEvents | Where-Object { $_.Type -eq $Type })
    if ($found.Count -ne 1) {
        $shown = @($script:updateEvents | ForEach-Object { "($($_.Type)) $($_.Message)" }) -join ' | '
        return "it wrote $($found.Count) $Type events, expected 1: $shown"
    }
    $Event.Value = $found[0]
    return ''
}

# The drain verb of a builder that says drained at once; every lift succeeds.
function Set-DrainedAtOnceDrain {
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'status') { return @{ ExitCode = 0; State = 'drained'; Lines = @() } }
        return @{ ExitCode = 0; State = $null; Lines = @() }
    }
}

Test-Case 'it raises, waits until drained, installs, then lifts' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'status') {
            $script:statusCount++
            if ($script:statusCount -lt 2) { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
            return @{ ExitCode = 0; State = 'drained'; Lines = @() }
        }
        return @{ ExitCode = 0; State = $null; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $problem = Get-EventOrderProblem @('find', 'raise', 'status:waiting', 'status:drained', 'raise', 'install', 'lift')
    if ($problem) { return $problem }
    if (($script:installIds -join ',') -ne 'id-1') { return "the install got '$($script:installIds -join ',')', expected the update's id" }
    return ''
}

Test-Case 'the script holds no reboot and calls no module cmdlet' {
    $text = Get-Content -LiteralPath $sourceScript -Raw
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($sourceScript, [ref]$null, [ref]$parseErrors)
    if ($null -ne $parseErrors -and $parseErrors.Count) { return "the script does not parse: $($parseErrors[0].Message)" }
    $defined = @()
    foreach ($function in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
        $defined += $function.Name
    }
    $called = @()
    foreach ($command in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true)) {
        $called += $command.GetCommandName()
    }
    $others = @($called | Where-Object { $_ -notin $defined -and $_ -notin 'exit', 'return' })
    if ($others.Count) { return "the script calls $($others -join ', '), which may auto-load a module" }
    $runspaceText = Get-InstallScriptText
    $runspaceErrors = $null
    $runspaceAst = [System.Management.Automation.Language.Parser]::ParseInput($runspaceText, [ref]$null, [ref]$runspaceErrors)
    if ($null -ne $runspaceErrors -and $runspaceErrors.Count) { return "the runspace text does not parse: $($runspaceErrors[0].Message)" }
    $runspaceCalled = @()
    foreach ($command in $runspaceAst.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true)) {
        $runspaceCalled += $command.GetCommandName()
    }
    if ($runspaceCalled.Count) { return "the runspace text calls $($runspaceCalled -join ', '), which may auto-load a module" }
    foreach ($banned in 'Restart-Computer', 'Stop-Computer', 'shutdown', 'InitiateSystemShutdown', 'ExitWindowsEx', 'CommitInstall') {
        if ($text.IndexOf($banned, [StringComparison]::OrdinalIgnoreCase) -ge 0) { return "the script holds '$banned'" }
    }
    return ''
}

Test-Case 'the drain request is the box''s JSON in base64' {
    $line = Get-DrainRequestLine -Root 'D:\gate-remote' -Action 'raise'
    $decoded = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($line))
    $want = '{"Root":"D:\\gate-remote","Action":"raise"}'
    if ($decoded -ne $want) { return "the line decodes to $decoded" }
    return ''
}

Test-Case 'the drain request line is pure ASCII' {
    $line = Get-DrainRequestLine -Root 'D:\gate-remote' -Action 'raise'
    if (-not $line) { return 'the line is empty' }
    $decoded = [Text.Encoding]::ASCII.GetString([Text.Encoding]::ASCII.GetBytes($line))
    if ($decoded -ne $line) { return "the line does not survive an ASCII round trip: $line" }
    return ''
}

Test-Case 'while waiting it raises again every 300 s' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        if ($script:fakeNow -lt $script:startTime.AddSeconds(700)) { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        $script:drainedAt = [int](($script:fakeNow - $script:startTime).TotalSeconds)
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $seconds = @(Get-RaiseSeconds)
    $expected = @(0, 300, 600, $script:drainedAt)
    if (($seconds -join ',') -ne ($expected -join ',')) {
        return "it raised at $($seconds -join ',') fake seconds, expected $($expected -join ','); the last is the fresh raise before the install"
    }
    return ''
}

Test-Case 'a status that says open raises again at once' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        if ($script:statusCount -eq 0) {
            $script:statusCount++
            return @{ ExitCode = 0; State = 'open'; Lines = @() }
        }
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $problem = Get-EventOrderProblem @('find', 'raise', 'status:open', 'raise', 'status:drained', 'raise', 'install', 'lift')
    if ($problem) { return $problem }
    $seconds = @(Get-RaiseSeconds)
    if ($seconds.Count -lt 2) { return "it raised $($seconds.Count) times" }
    if ($seconds[1] -ne $seconds[0]) {
        return "the raise after open came $($seconds[1] - $seconds[0]) s later, expected at once"
    }
    return ''
}

Test-Case 'a status call that fails is retried and the run still installs' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        $script:statusCount++
        if ($script:statusCount -eq 1) { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($script:statusCount -eq 2) { return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') } }
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:events -notcontains 'install') { return "it never installed: $($script:events -join ' | ')" }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    if ($warning.Message -notlike 'windows-update: a drain status call failed: *; retrying') {
        return "it wrote '$($warning.Message)'"
    }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    return ''
}

Test-Case 'with nothing to install it never raises the drain and returns 0' {
    Reset-Fakes
    $script:updates = @()
    $script:onDrain = {
        param([string]$Action)
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $problem = Get-EventOrderProblem @('find')
    if ($problem) { return $problem }
    $information = $null
    $problem = Get-SingleEvent 'Information' ([ref]$information)
    if ($problem) { return $problem }
    if ($information.Message -ne 'windows-update: nothing to install') { return "it wrote '$($information.Message)'" }
    return ''
}

Test-Case 'while the install runs it raises again every 300 s' {
    Reset-Fakes
    $script:installDoneAfter = 65
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $seconds = @(Get-RaiseSeconds)
    if ($script:installRaises -ne 2) {
        return "the install raised $($script:installRaises) times, expected two: $($seconds -join ',')"
    }
    $installSeconds = @($seconds | Select-Object -Last 2)
    if ($installSeconds[1] - $installSeconds[0] -lt 300) {
        return "two raises are $($installSeconds[1] - $installSeconds[0]) s apart: $($seconds -join ',')"
    }
    return ''
}

Test-Case 'a re-raise that fails during the install keeps the drain and lifts only after the install is done' {
    Reset-Fakes
    $script:installDoneAfter = 65
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') {
            if ($script:installed -and $script:raiseFailures -eq 0) {
                $script:raiseFailures++
                return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') }
            }
            return @{ ExitCode = 0; State = 'waiting'; Lines = @() }
        }
        if ($Action -eq 'status') { return @{ ExitCode = 0; State = 'drained'; Lines = @() } }
        return @{ ExitCode = 0; State = $null; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    if (-not $script:installed) { return 'it never installed' }
    if ($script:installPolls -le $script:installDoneAfter) {
        return "the install was polled $($script:installPolls) times, so it never finished"
    }
    if ($script:installRaises -lt 2) {
        return "the install raised $($script:installRaises) times, so the failed raise was never retried"
    }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    if ($warning.Message -notlike 'windows-update: re-raising the drain during the install failed: *; retrying') {
        return "it wrote '$($warning.Message)'"
    }
    return ''
}

Test-Case 'an install that returns no result still lifts and returns 1' {
    Reset-Fakes
    $script:installNoResult = $true
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    $errorEvent = $null
    $problem = Get-SingleEvent 'Error' ([ref]$errorEvent)
    if ($problem) { return $problem }
    if ($errorEvent.Message -notlike 'windows-update: the install failed: no result came back*') {
        return "it wrote '$($errorEvent.Message)'"
    }
    return ''
}

Test-Case 'a result with an error record still reports the result and the reboot Warning' {
    Reset-Fakes
    $script:installErrors = @($oneError)
    $script:rebootRequired = $true
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $information = $null
    $problem = Get-SingleEvent 'Information' ([ref]$information)
    if ($problem) { return $problem }
    if ($information.Message -notlike 'windows-update: install finished: result 2*') { return "it wrote '$($information.Message)'" }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    $want = 'windows-update: a reboot is pending; it waits for the owner: a session drains the builder and asks the owner before any reboot'
    if ($warning.Message -ne $want) { return "the Warning is '$($warning.Message)'" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    return ''
}

Test-Case 'a handle with no result throws naming the first error record' {
    $powerShell = [pscustomobject]@{ Streams = [pscustomobject]@{ Error = @($oneError) } }
    $powerShell | Add-Member -MemberType ScriptMethod -Name EndInvoke -Value { param($Async) }
    $handle = @{ PowerShell = $powerShell; Async = $null }
    $message = $null
    try { $null = Complete-UpdateInstall -Handle $handle } catch { $message = $_.Exception.Message }
    if (-not $message) { return 'it returned instead of throwing' }
    if ($message -notmatch 'no result') { return "it threw '$message'" }
    if ($message -notmatch 'an error record') { return "the message does not name the error record: $message" }
    return ''
}

Test-Case 'a reboot-required result writes the pending-reboot Warning, lifts and returns 0' {
    Reset-Fakes
    $script:rebootRequired = $true
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    $want = 'windows-update: a reboot is pending; it waits for the owner: a session drains the builder and asks the owner before any reboot'
    if ($warning.Message -ne $want) { return "the Warning is '$($warning.Message)'" }
    return ''
}

Test-Case 'waiting past 2 h gives up, lifts and returns 1' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        return @{ ExitCode = 0; State = 'waiting'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    $want = 'windows-update: gave up after 2 h waiting for the builder to drain; nothing installed'
    if ($warning.Message -ne $want) { return "the Warning is '$($warning.Message)'" }
    if ($script:fakeNow -lt $script:startTime.AddHours(2)) {
        return "it gave up after $([int](($script:fakeNow - $script:startTime).TotalSeconds)) fake seconds, before 2 h"
    }
    if ($script:events -contains 'install') { return 'it installed an update after giving up' }
    return ''
}

Test-Case 'a raise that fails returns 1 with no lift' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: the builder is busy') }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    if ($script:events -contains 'lift') { return "it lifted: $($script:events -join ' | ')" }
    $errorEvent = $null
    $problem = Get-SingleEvent 'Error' ([ref]$errorEvent)
    if ($problem) { return $problem }
    if ($errorEvent.Message -notmatch 'exit 1') { return "the Error does not name the exit code: $($errorEvent.Message)" }
    if ($errorEvent.Message -notmatch 'the builder is busy') { return "the Error does not name the lines: $($errorEvent.Message)" }
    return ''
}

Test-Case 'a lift that fails writes the lift Error and returns 1' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'lift') { return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') } }
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    $errorEvent = $null
    $problem = Get-SingleEvent 'Error' ([ref]$errorEvent)
    if ($problem) { return $problem }
    $want = 'windows-update: lift failed: exit 1: gate: drain failed: busy; the barrier lapses by itself within 30 minutes'
    if ($errorEvent.Message -ne $want) { return "it wrote '$($errorEvent.Message)'" }
    return ''
}

Test-Case 'succeeded with errors writes a Warning and returns 0' {
    Reset-Fakes
    $script:resultCode = 3
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $warning = $null
    $problem = Get-SingleEvent 'Warning' ([ref]$warning)
    if ($problem) { return $problem }
    if ($warning.Message -notlike 'windows-update: install finished: result 3*') { return "it wrote '$($warning.Message)'" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    return ''
}

Test-Case 'a failed install writes an Error and returns 1' {
    Reset-Fakes
    $script:resultCode = 4
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    $errorEvent = $null
    $problem = Get-SingleEvent 'Error' ([ref]$errorEvent)
    if ($problem) { return $problem }
    if ($errorEvent.Message -notlike 'windows-update: install finished: result 4*') { return "it wrote '$($errorEvent.Message)'" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    return ''
}

Test-Case 'the script parses under Windows PowerShell 5.1' {
    $exe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $exe)) { return 'skipped: powershell.exe is not on this machine' }
    $command = "`$e = `$null; " +
        "[System.Management.Automation.Language.Parser]::ParseFile('$sourceScript', [ref]`$null, [ref]`$e) | Out-Null; " +
        "`$e.Count"
    $output = (& $exe -NoProfile -NonInteractive -Command $command 2>&1) -join ' '
    if ("$output".Trim() -ne '0') { return "Windows PowerShell 5.1 reports: $output" }
    return ''
}

Test-Case 'an install whose runspace throws names the fault' {
    Reset-Fakes
    $script:installThrowsOnEnd = $true
    Set-DrainedAtOnceDrain
    $code = Invoke-DrainedUpdate
    if ($code -ne 1) { return "returned $code, expected 1" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    $errorEvent = $null
    $problem = Get-SingleEvent 'Error' ([ref]$errorEvent)
    if ($problem) { return $problem }
    if ($errorEvent.Message -notmatch 'WU_E_FAKE 0x80240001') { return "it wrote '$($errorEvent.Message)'" }
    return ''
}

Test-Case 'a successful raise ends the failure streak, so a later failure warns again' {
    Reset-Fakes
    $script:installDoneAfter = 65
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') {
            $script:raiseCount++
            if ($script:raiseCount -eq 2) { return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') } }
            if ($script:installed -and $script:installFailCount -eq 0) {
                $script:installFailCount++
                return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') }
            }
            return @{ ExitCode = 0; State = 'waiting'; Lines = @() }
        }
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        if ($script:fakeNow -lt $script:startTime.AddSeconds(700)) { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $warnings = @($script:updateEvents | Where-Object { $_.Type -eq 'Warning' })
    if ($warnings.Count -ne 2) { return "it wrote $($warnings.Count) Warnings: $($warnings.Message -join ' | ')" }
    if ($warnings[0].Message -notlike 'windows-update: re-raising the drain while waiting failed: *; retrying') {
        return "the first Warning is '$($warnings[0].Message)'"
    }
    if ($warnings[1].Message -notlike 'windows-update: re-raising the drain during the install failed: *; retrying') {
        return "the second Warning is '$($warnings[1].Message)'"
    }
    return ''
}

Test-Case 'a failed status does not stop the re-raise and the run still installs' {
    Reset-Fakes
    $script:onDrain = {
        param([string]$Action)
        if ($Action -eq 'raise') { return @{ ExitCode = 0; State = 'waiting'; Lines = @() } }
        if ($Action -eq 'lift') { return @{ ExitCode = 0; State = $null; Lines = @() } }
        if ($script:fakeNow -lt $script:startTime.AddSeconds(700)) {
            return @{ ExitCode = 1; State = $null; Lines = @('gate: drain failed: busy') }
        }
        $script:drainedAt = [int](($script:fakeNow - $script:startTime).TotalSeconds)
        return @{ ExitCode = 0; State = 'drained'; Lines = @() }
    }
    $code = Invoke-DrainedUpdate
    if ($code -ne 0) { return "returned $code, expected 0" }
    $seconds = @(Get-RaiseSeconds)
    $waitRaises = @($seconds | Where-Object { $_ -lt $script:drainedAt })
    if (($waitRaises -join ',') -ne '0,300,600') {
        return "the raises while waiting are at $($waitRaises -join ',') fake seconds, expected 0,300,600"
    }
    if ($script:events -notcontains 'install') { return "it never installed: $($script:events -join ' | ')" }
    $problem = Get-LiftProblem
    if ($problem) { return $problem }
    return ''
}

Test-Case 'a feature update is left out by its category ID, whatever its name' {
    $upgrades = @(@{ CategoryID = '3689bdc8-b205-4af4-8d4b-a63924c5e9d5'; Name = 'Mises à niveau' })
    if (-not (Test-IsUpgrade -Categories $upgrades)) { return 'it did not take the Upgrades category for one' }
    $security = @(@{ CategoryID = '0fa1201d-4330-4fa8-8ae9-b877473b6441'; Name = 'Mises à niveau' })
    if (Test-IsUpgrade -Categories $security) { return 'it took another category for Upgrades' }
    return ''
}

"`n$($script:fails) failure(s)"
exit $script:fails
