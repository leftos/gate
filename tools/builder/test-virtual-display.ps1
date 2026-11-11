#Requires -Version 7
# Case table for virtual-display.ps1, the Windows builder's virtual display. The script is dot-sourced once and its
# seam functions are replaced with recording stand-ins: a fake clock whose Wait-Seconds advances it, an adapter that
# appears at a fake time, a monitor count the cases script, a plug that records and may throw, a display that attaches
# at a fake time, a mode set that records its arguments and returns a scripted code, and event recording. Each case
# reads the calls back; no case sleeps or reads the wall clock. Prints ok/FAIL per case and exits with the failure count.
#
#   pwsh -NoProfile -File tools/builder/test-virtual-display.ps1
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceScript = Join-Path $PSScriptRoot 'virtual-display.ps1'
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

# The script's seams, replaced for every case: the clock, the adapter, the monitor count, the plug, the display, the
# mode set and the event log.
function Get-Now { return $script:fakeNow }
function Wait-Seconds([double]$Seconds) { $script:fakeNow = $script:fakeNow.AddSeconds($Seconds) }

function Get-VirtualAdapter {
    if ($script:fakeNow -lt $script:adapterAt) { return $null }
    return [pscustomobject]@{ InstanceId = 'USB\VID_1D6B&PID_0104\6&1a2b3c4d&0&1' }
}

function Test-VirtualAdapter {
    $script:interfaceChecks++
    return [bool](Get-VirtualAdapter) -and ($script:fakeNow -ge $script:interfaceAt)
}

function Get-VirtualMonitorCount([string]$AdapterId) {
    $script:countAdapter = $AdapterId
    return $script:monitorCount
}

function Invoke-MonitorPlug {
    $script:plugged++
    if ($script:plugThrows) { throw 'device ioctl failed (0x1F)' }
    $script:monitorCount = $script:countAfterPlug
}

function Get-VirtualDisplay {
    if ($script:fakeNow -lt $script:displayAt) { return $null }
    return $script:display
}

function Set-VirtualDisplayMode([string]$Device, [int]$Width, [int]$Height) {
    $script:setArgs = [pscustomobject]@{ Device = $Device; Width = $Width; Height = $Height }
    if ($script:modeCode -eq 0) { $script:display = [pscustomobject]@{ Device = $Device; Width = $Width; Height = $Height } }
    return $script:modeCode
}

function Write-DisplayEvent([string]$Message, [string]$Type) {
    $script:events.Add([pscustomobject]@{ Message = $Message; Type = $Type })
}

# Every recording stand-in's state, back to its start, before each case.
function Reset-Fakes {
    $script:startTime = [DateTime]::new(2026, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
    $script:fakeNow = $script:startTime
    $script:adapterAt = $script:startTime
    $script:interfaceAt = $script:startTime
    $script:interfaceChecks = 0
    $script:countAdapter = $null
    $script:monitorCount = 0
    $script:countAfterPlug = 1
    $script:plugThrows = $false
    $script:plugged = 0
    $script:displayAt = $script:startTime
    $script:display = [pscustomobject]@{ Device = '\\.\DISPLAY10'; Width = 1024; Height = 768 }
    $script:modeCode = 0
    $script:setArgs = $null
    $script:events = [System.Collections.Generic.List[object]]::new()
}

# The fake seconds that passed since the case started.
function Get-ElapsedSeconds { return [int](($script:fakeNow - $script:startTime).TotalSeconds) }

# The one event of $Type the script wrote, or a problem naming what it wrote instead.
function Get-SingleEvent([string]$Type, [ref]$Event) {
    $found = @($script:events | Where-Object { $_.Type -eq $Type })
    if ($found.Count -ne 1) {
        $shown = @($script:events | ForEach-Object { "($($_.Type)) $($_.Message)" }) -join ' | '
        return "it wrote $($found.Count) $Type events, expected 1: $shown"
    }
    $Event.Value = $found[0]
    return ''
}

# The line the status action prints and the code it returns. The line goes to the host, so 6>&1 turns it into an
# information record beside the code.
function Invoke-Status {
    $out = @(Invoke-VirtualDisplay -Action status 6>&1)
    $line = @($out | Where-Object { $_ -is [System.Management.Automation.InformationRecord] } |
        ForEach-Object { "$($_.MessageData)" }) -join ''
    $code = @($out | Where-Object { $_ -isnot [System.Management.Automation.InformationRecord] }) -join ','
    return [pscustomobject]@{ Line = $line; Code = $code }
}

Test-Case 'ensure with a monitor plugged in plugs nothing and writes no event' {
    Reset-Fakes
    $script:monitorCount = 1
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:plugged -ne 0) { return "it plugged $($script:plugged) time(s)" }
    if ($script:events.Count) { return "it wrote $($script:events.Count) event(s): $($script:events[0].Message)" }
    return ''
}

Test-Case 'ensure with a monitor plugged in never compiles the interop' {
    Reset-Fakes
    $script:monitorCount = 1
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:interfaceChecks -ne 0) { return "it checked the device interface $($script:interfaceChecks) time(s)" }
    return ''
}

Test-Case 'ensure with no monitor plugs once and writes an Information event' {
    Reset-Fakes
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:plugged -ne 1) { return "it plugged $($script:plugged) time(s), expected 1" }
    $event = $null
    $problem = Get-SingleEvent 'Information' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -ne 'virtual-display: plugged in a virtual monitor (0 -> 1)') { return "it wrote '$($event.Message)'" }
    return ''
}

Test-Case 'ensure waits for an adapter that shows after 30 s, then plugs' {
    Reset-Fakes
    $script:adapterAt = $script:startTime.AddSeconds(30)
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:plugged -ne 1) { return "it plugged $($script:plugged) time(s), expected 1" }
    return ''
}

Test-Case 'ensure with no adapter for 180 s writes an Error event and returns 2 without plugging' {
    Reset-Fakes
    $script:adapterAt = $script:startTime.AddYears(1)
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 2) { return "returned $code, expected 2" }
    if ((Get-ElapsedSeconds) -lt 180) { return "it waited $(Get-ElapsedSeconds) s, expected at least 180" }
    if ($script:plugged -ne 0) { return "it plugged $($script:plugged) time(s)" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -notmatch 'usbmmidd') { return "it wrote '$($event.Message)', which does not name the driver" }
    return ''
}

Test-Case 'ensure with no monitor and no interface for 180 s writes the driver Error and returns 2' {
    Reset-Fakes
    $script:interfaceAt = $script:startTime.AddYears(1)
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 2) { return "returned $code, expected 2" }
    if ((Get-ElapsedSeconds) -lt 180) { return "it waited $(Get-ElapsedSeconds) s, expected at least 180" }
    if ($script:plugged -ne 0) { return "it plugged $($script:plugged) time(s)" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -notmatch 'usbmmidd') { return "it wrote '$($event.Message)', which does not name the driver" }
    return ''
}

Test-Case 'ensure whose plug leaves no monitor writes an Error event and returns 1' {
    Reset-Fakes
    $script:countAfterPlug = 0
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 1) { return "returned $code, expected 1" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -notmatch 'count 0') { return "it wrote '$($event.Message)', which names no count" }
    return ''
}

Test-Case 'ensure whose plug throws writes an Error event with the message and returns 1' {
    Reset-Fakes
    $script:plugThrows = $true
    $code = Invoke-VirtualDisplay -Action ensure
    if ($code -ne 1) { return "returned $code, expected 1" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -notmatch '0x1F') { return "it wrote '$($event.Message)', which does not hold the failure" }
    return ''
}

Test-Case 'mode at 1920x1080 already changes nothing and writes no event' {
    Reset-Fakes
    $script:display = [pscustomobject]@{ Device = '\\.\DISPLAY10'; Width = 1920; Height = 1080 }
    $code = Invoke-VirtualDisplay -Action mode
    if ($code -ne 0) { return "returned $code, expected 0" }
    if ($script:setArgs) { return "it set the display to $($script:setArgs.Width)x$($script:setArgs.Height)" }
    if ($script:events.Count) { return "it wrote $($script:events.Count) event(s): $($script:events[0].Message)" }
    return ''
}

Test-Case 'mode at 1024x768 sets 1920x1080 once and writes an Information event' {
    Reset-Fakes
    $code = Invoke-VirtualDisplay -Action mode
    if ($code -ne 0) { return "returned $code, expected 0" }
    if (-not $script:setArgs) { return 'it set no mode' }
    if ($script:setArgs.Width -ne 1920 -or $script:setArgs.Height -ne 1080) {
        return "it set $($script:setArgs.Width)x$($script:setArgs.Height), expected 1920x1080"
    }
    $event = $null
    $problem = Get-SingleEvent 'Information' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -ne 'virtual-display: set \\.\DISPLAY10 to 1920x1080 (was 1024x768)') { return "it wrote '$($event.Message)'" }
    return ''
}

Test-Case 'mode waits for the display to attach, then sets it' {
    Reset-Fakes
    $script:displayAt = $script:startTime.AddSeconds(40)
    $code = Invoke-VirtualDisplay -Action mode
    if ($code -ne 0) { return "returned $code, expected 0" }
    if (-not $script:setArgs) { return 'it set no mode' }
    return ''
}

Test-Case 'mode with no display for 300 s writes an Error event and returns 1' {
    Reset-Fakes
    $script:displayAt = $script:startTime.AddYears(1)
    $code = Invoke-VirtualDisplay -Action mode
    if ($code -ne 1) { return "returned $code, expected 1" }
    if ((Get-ElapsedSeconds) -lt 300) { return "it waited $(Get-ElapsedSeconds) s, expected at least 300" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    $want = 'virtual-display: no virtual monitor is attached in this session after 300 s'
    if ($event.Message -ne $want) { return "it wrote '$($event.Message)'" }
    return ''
}

Test-Case 'mode whose change returns a non-zero code writes an Error event and returns 1' {
    Reset-Fakes
    $script:modeCode = -2
    $code = Invoke-VirtualDisplay -Action mode
    if ($code -ne 1) { return "returned $code, expected 1" }
    $event = $null
    $problem = Get-SingleEvent 'Error' ([ref]$event)
    if ($problem) { return $problem }
    if ($event.Message -notmatch '\-2') { return "it wrote '$($event.Message)', which names no return code" }
    return ''
}

Test-Case 'status prints the monitor count and writes no event' {
    Reset-Fakes
    $script:monitorCount = 2
    $status = Invoke-Status
    if ($status.Code -ne '0') { return "returned '$($status.Code)', expected 0" }
    if ($status.Line -ne 'virtual-display: adapter present, 2 virtual monitor(s)') { return "it printed '$($status.Line)'" }
    if ($script:events.Count) { return "it wrote $($script:events.Count) event(s)" }
    if ($script:interfaceChecks -ne 0) { return "it checked the device interface $($script:interfaceChecks) time(s)" }
    return ''
}

Test-Case 'the script never restarts the machine or a device' {
    $text = Get-Content -LiteralPath $sourceScript -Raw
    $banned = 'Restart-Computer', 'Stop-Computer', 'shutdown', 'ExitWindowsEx', 'InitiateSystemShutdown',
        'Disable-PnpDevice', 'Enable-PnpDevice', 'pnputil', 'devcon', 'Restart-Service'
    $found = @($banned | Where-Object { $text.IndexOf($_, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
    if ($found.Count) { return "the script holds $($found -join ', ')" }
    return ''
}

Test-Case 'dot-sourcing the script compiles nothing and calls no device' {
    foreach ($name in 'UsbmmIdd', 'DisplayApi') {
        if ($name -as [type]) { return "dot-sourcing the script loaded the type $name" }
    }
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

"`n$($script:fails) failure(s)"
exit $script:fails
