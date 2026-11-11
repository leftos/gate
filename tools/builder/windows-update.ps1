# windows-update.ps1: the builder's drained Windows Update install. The task \build-box\gate-windows-update runs it daily
# as the builder user (S4U, Highest): it searches for the software updates Windows offers, and only when it finds one
# raises the box drain, waits until this box says the builder is drained, installs the updates Windows already
# downloaded, reports each result to the Application log under gate-windows-update, lifts the drain, and never restarts
# anything. A pending restart is a Warning that waits for the owner's yes.
#
# Windows Update itself is set to download only by the installer's Windows Update drained install step, so this script is
# the only thing that installs an update.
#
# The work root is read from windows-update.root beside this script: it is never on the task's command line, which the
# box's drain judge counts as live work. The task runs under Windows PowerShell 5.1, so nothing here uses newer syntax.
# Module auto-loading is off and every value comes from a .NET call: the builder user can write the user module path, and
# this task runs elevated. The update COM objects stay in the thread that makes them: the install runs in a runspace of
# its own and is given update identity strings, never an object.
$PSModuleAutoLoadingPreference = 'None'
$ErrorActionPreference = 'Stop'

$drainExe = 'C:\Program Files\gate\bin\gate.exe'
$eventSource = 'gate-windows-update'
$drainBoundMs = 120000
$pollSeconds = 10
$raiseEverySeconds = 300
$giveUpSeconds = 7200
# The Upgrades classification, the category a feature update belongs to. Its name is translated, so the ID decides.
$upgradeCategoryId = '3689BDC8-B205-4AF4-8D4B-A63924C5E9D5'

function Get-Now { return [DateTime]::UtcNow }

function Wait-Seconds([int]$Seconds) { [Threading.Thread]::Sleep($Seconds * 1000) }

# The one file the installer writes beside this script: the gate's work root on this builder.
function Read-GateRoot {
    $path = [IO.Path]::Combine($PSScriptRoot, 'windows-update.root')
    $root = [IO.File]::ReadAllText($path).Trim()
    if (-not $root) { throw "windows-update: $path is empty" }
    return $root
}

# The line `gate box drain --stdin` reads on its standard input: the payload word, base64 of the compact UTF-8 JSON
# {"Root": <root>, "Action": <action>}.
function Get-DrainRequestLine([string]$Root, [string]$Action) {
    $escaped = $Root.Replace('\', '\\').Replace('"', '\"')
    $json = '{"Root":"' + $escaped + '","Action":"' + $Action + '"}'
    return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
}

# The non-empty lines of a drain call's standard output and standard error.
function Split-OutputLines([string]$Out, [string]$Err) {
    $lines = @()
    foreach ($text in @($Out, $Err)) {
        foreach ($line in ($text -split "`r?`n")) {
            if ($line) { $lines += $line }
        }
    }
    return , $lines
}

# The state a drain call's status line names: drained, waiting, open, or $null when no line names one.
function Get-DrainState([string[]]$Lines) {
    foreach ($line in $Lines) {
        if ($line -match '^gate: drain state (drained|waiting|open)$') { return $Matches[1] }
    }
    return $null
}

# Runs one drain call on this box, bounded at two minutes, and returns its exit code, its state and its output lines. A
# box that exits before it reads the request is no failure here: its own lines and exit code are what the caller reports.
function Invoke-BoxDrain([string]$Root, [string]$Action) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $drainExe
    $info.Arguments = 'box drain --stdin'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $outTask = $process.StandardOutput.ReadToEndAsync()
    $errTask = $process.StandardError.ReadToEndAsync()
    $bytes = [Text.Encoding]::ASCII.GetBytes((Get-DrainRequestLine -Root $Root -Action $Action) + "`n")
    try {
        $process.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
        $process.StandardInput.BaseStream.Flush()
        $process.StandardInput.Close()
    }
    catch {
        if ($_.Exception -isnot [IO.IOException] -and $_.Exception.InnerException -isnot [IO.IOException]) { throw }
    }
    if (-not $process.WaitForExit($drainBoundMs)) {
        $process.Kill()
        throw "the drain call '$Action' did not exit within $($drainBoundMs / 1000) s"
    }
    $lines = Split-OutputLines -Out $outTask.Result -Err $errTask.Result
    return @{ ExitCode = $process.ExitCode; State = (Get-DrainState -Lines $lines); Lines = $lines }
}

# True when any category is the Upgrades classification: Windows names a category in its own language, so its ID decides.
function Test-IsUpgrade($Categories) {
    foreach ($category in $Categories) {
        if ([string]$category.CategoryID -eq $upgradeCategoryId) { return $true }
    }
    return $false
}

# The software updates Windows offers this machine, a feature update left out, as plain data: an identity and a title.
# No COM object leaves this function.
function Get-PendingUpdate {
    $session = [Activator]::CreateInstance([Type]::GetTypeFromProgID('Microsoft.Update.Session'))
    $searcher = $session.CreateUpdateSearcher()
    $found = $searcher.Search("IsInstalled=0 and IsHidden=0 and Type='Software'")
    $pending = @()
    foreach ($update in $found.Updates) {
        if (Test-IsUpgrade -Categories $update.Categories) { continue }
        $pending += @{ Id = [string]$update.Identity.UpdateID; Title = [string]$update.Title }
    }
    return , $pending
}

# The script the install runspace runs: it searches again, keeps the updates whose identity is in $Ids, accepts each
# EULA, downloads what is not downloaded and installs the rest. A failed download leaves that update out and says so in
# its title; nothing downloaded is an overall failure (result 4). It makes its own COM objects, since none may cross.
function Get-InstallScriptText {
    return @'
param($Ids)
$ErrorActionPreference = 'Stop'
$PSModuleAutoLoadingPreference = 'None'
$session = [Activator]::CreateInstance([Type]::GetTypeFromProgID('Microsoft.Update.Session'))
$searcher = $session.CreateUpdateSearcher()
$found = $searcher.Search("IsInstalled=0 and IsHidden=0 and Type='Software'")
$collection = [Activator]::CreateInstance([Type]::GetTypeFromProgID('Microsoft.Update.UpdateColl'))
$titles = @()
foreach ($update in $found.Updates) {
    if ($Ids -notcontains [string]$update.Identity.UpdateID) { continue }
    $isUpgrade = $false
    foreach ($category in $update.Categories) {
        if ([string]$category.CategoryID -eq '3689BDC8-B205-4AF4-8D4B-A63924C5E9D5') { $isUpgrade = $true }
    }
    if ($isUpgrade) { continue }
    if (-not $update.EulaAccepted) { $update.AcceptEula() }
    $downloaded = $true
    if (-not $update.IsDownloaded) {
        try {
            $one = [Activator]::CreateInstance([Type]::GetTypeFromProgID('Microsoft.Update.UpdateColl'))
            [void]$one.Add($update)
            $downloader = $session.CreateUpdateDownloader()
            $downloader.Updates = $one
            [void]$downloader.Download()
        }
        catch {
            $downloaded = $false
            $titles += $update.Title + ' (download failed: ' + $_.Exception.Message + ')'
        }
    }
    if ($downloaded) { [void]$collection.Add($update) }
}
if ($collection.Count -eq 0) {
    return @{ ResultCode = 4; RebootRequired = $false; Titles = $titles }
}
$installer = $session.CreateUpdateInstaller()
$installer.Updates = $collection
$result = $installer.Install()
for ($i = 0; $i -lt $collection.Count; $i++) {
    $titles += $collection.Item($i).Title + ' (result ' + $result.GetUpdateResult($i).ResultCode + ')'
}
return @{ ResultCode = $result.ResultCode; RebootRequired = $result.RebootRequired; Titles = $titles }
'@
}

# Starts the download and install of $Updates in a runspace of its own, so this thread can keep raising the barrier, and
# returns the handle the three functions below read. Only the updates' identity strings cross into it.
function Start-UpdateInstall($Updates) {
    $ids = @()
    foreach ($update in @($Updates)) { $ids += [string]$update.Id }
    $powershell = [powershell]::Create()
    [void]$powershell.AddScript((Get-InstallScriptText)).AddArgument($ids)
    return @{ PowerShell = $powershell; Async = $powershell.BeginInvoke() }
}

function Test-UpdateInstallDone($Handle) { return $Handle.Async.IsCompleted }

# The deepest message of an exception chain, which is where the fault itself is named.
function Get-InnermostMessage($Failure) {
    $inner = $Failure
    while ($null -ne $inner.InnerException) { $inner = $inner.InnerException }
    return $inner.Message
}

# Ends the install runspace and returns the overall result code, whether a restart is needed, and each update's title
# with its own result or download result. Result codes are the Windows Update operation's: 2 succeeded, 3 succeeded with
# errors, 4 failed, 5 aborted. A result that came back is reported even beside error records; no result is a failure,
# and its message names the runspace's own exception, else its first error record.
function Complete-UpdateInstall($Handle) {
    $output = $null
    $failure = $null
    try {
        $output = $Handle.PowerShell.EndInvoke($Handle.Async)
    }
    catch {
        $failure = $_.Exception
    }
    $result = $null
    foreach ($item in $output) {
        if ($null -ne $item -and $null -ne $item.ResultCode) { $result = $item }
    }
    if ($null -eq $result) {
        if ($null -ne $failure) { throw "the install failed: $(Get-InnermostMessage -Failure $failure)" }
        $why = 'no result came back'
        $errors = $Handle.PowerShell.Streams.Error
        if ($errors.Count -gt 0) { $why = "no result came back: $($errors[0].ToString())" }
        throw "the install failed: $why"
    }
    return @{ ResultCode = $result.ResultCode; RebootRequired = $result.RebootRequired; Titles = @($result.Titles) }
}

# True when a restart is pending: either key's existence says one is.
function Test-RebootPending {
    foreach ($path in @(
            'SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending'
            'SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired')) {
        $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($path)
        if ($null -ne $key) {
            $key.Close()
            return $true
        }
    }
    return $false
}

# The Application log line, under gate-windows-update, event ID 1.
function Write-UpdateEvent([string]$Message, [string]$Type) {
    [Diagnostics.EventLog]::WriteEntry($eventSource, $Message, [Diagnostics.EventLogEntryType]::$Type, 1)
}

# The event type the overall install result is reported as.
function Get-ResultEventType([int]$Code) {
    if ($Code -eq 3) { return 'Warning' }
    if ($Code -eq 4 -or $Code -eq 5) { return 'Error' }
    return 'Information'
}

# The one event the install writes: every title with its result, and a restart that is pending.
function Complete-UpdateReport($Result) {
    $message = "windows-update: install finished: result $($Result.ResultCode); " + (@($Result.Titles) -join '; ')
    Write-UpdateEvent -Message $message -Type (Get-ResultEventType -Code $Result.ResultCode)
    if (-not $Result.RebootRequired -and -not (Test-RebootPending)) { return }
    Write-UpdateEvent -Type 'Warning' -Message (
        'windows-update: a reboot is pending; it waits for the owner: a session drains the builder and asks the owner before any reboot')
}

# Writes one Warning per failure streak: the first failure of a kind writes, and the kind stays quiet until it succeeds.
function Write-StreakWarning($State, [string]$Name, [string]$Message) {
    if ($State["$($Name)Failing"]) { return }
    $State["$($Name)Failing"] = $true
    Write-UpdateEvent -Type 'Warning' -Message $Message
}

function Clear-Streak($State, [string]$Name) { $State["$($Name)Failing"] = $false }

# Raises the barrier again, which also pushes its lapse out by another 30 minutes. A success ends the raise failure
# streak, wherever this is called from, so a later failure warns again.
function Invoke-RaiseBarrier($State) {
    $result = Invoke-BoxDrain -Root $State.Root -Action 'raise'
    if ($result.ExitCode -ne 0 -or $null -eq $result.State) {
        throw "the drain raise failed: exit $($result.ExitCode): $($result.Lines -join ' ')"
    }
    $State.LastRaise = Get-Now
    Clear-Streak -State $State -Name 'Raise'
}

# Raises again and keeps the run going when that fails: one Warning per failure streak, and the next poll tries again.
function Invoke-RaiseRetried($State, [string]$When) {
    try {
        Invoke-RaiseBarrier -State $State
        return $true
    }
    catch {
        Write-StreakWarning -State $State -Name 'Raise' -Message (
            "windows-update: re-raising the drain $When failed: $($_.Exception.Message); retrying")
        return $false
    }
}

# Raises again once a raise is 300 s old, so the barrier never lapses while this script is working.
function Update-RaiseIfDue($State, [string]$When) {
    if ((Get-Now) -lt $State.LastRaise.AddSeconds($raiseEverySeconds)) { return }
    [void](Invoke-RaiseRetried -State $State -When $When)
}

# One status poll: the box's answer, or $null after reporting that the call failed. A failed call is retried on the next
# poll and never ends the run.
function Get-DrainStatus($State) {
    try {
        $status = Invoke-BoxDrain -Root $State.Root -Action 'status'
        if ($status.ExitCode -ne 0 -or $null -eq $status.State) {
            throw "exit $($status.ExitCode): $($status.Lines -join ' ')"
        }
        Clear-Streak -State $State -Name 'Status'
        return $status
    }
    catch {
        Write-StreakWarning -State $State -Name 'Status' -Message (
            "windows-update: a drain status call failed: $($_.Exception.Message); retrying")
        return $null
    }
}

# Polls the box until it judges the builder drained; false once 2 h have passed since the first raise. The 300 s re-raise
# runs on every poll, whatever the status call returned; only the immediate raise on an open barrier needs its state.
function Wait-UntilDrained($State) {
    $deadline = $State.FirstRaise.AddSeconds($giveUpSeconds)
    while ($true) {
        $status = Get-DrainStatus -State $State
        if ($null -ne $status -and $status.State -eq 'drained') { return $true }
        if ((Get-Now) -ge $deadline) { return $false }
        if ($null -ne $status -and $status.State -eq 'open') {
            [void](Invoke-RaiseRetried -State $State -When 'while waiting')
        }
        else { Update-RaiseIfDue -State $State -When 'while waiting' }
        Wait-Seconds $pollSeconds
    }
}

# Lifts the barrier; a lift that fails is reported and never throws, since it runs on the way out.
function Invoke-LiftBarrier($State) {
    try {
        $result = Invoke-BoxDrain -Root $State.Root -Action 'lift'
        if ($result.ExitCode -eq 0) { return $true }
        $why = "exit $($result.ExitCode): $($result.Lines -join ' ')"
    }
    catch {
        $why = $_.Exception.Message
    }
    Write-UpdateEvent -Type 'Error' -Message (
        "windows-update: lift failed: $why; the barrier lapses by itself within 30 minutes")
    return $false
}

# Raises the barrier and waits for the box to call the builder drained, saying why when it does not. $false means the
# caller returns 1: a raise that failed (no barrier stands, so nothing to lift), or the 2 h give-up.
function Start-DrainWait($State) {
    $first = Invoke-BoxDrain -Root $State.Root -Action 'raise'
    if ($first.ExitCode -ne 0 -or $null -eq $first.State) {
        Write-UpdateEvent -Type 'Error' -Message (
            "windows-update: drain raise failed: exit $($first.ExitCode): $($first.Lines -join ' ')")
        return $false
    }
    $State.Raised = $true
    $State.LastRaise = Get-Now
    $State.FirstRaise = $State.LastRaise
    if (Wait-UntilDrained -State $State) { return $true }
    Write-UpdateEvent -Type 'Warning' -Message (
        'windows-update: gave up after 2 h waiting for the builder to drain; nothing installed')
    return $false
}

# Installs $Updates under a fresh raise, which gives the install a new 30-minute lapse, and reports the result.
function Invoke-UpdateInstall($State, $Updates) {
    Invoke-RaiseBarrier -State $State
    $handle = Start-UpdateInstall -Updates $Updates
    while (-not (Test-UpdateInstallDone -Handle $handle)) {
        Wait-Seconds $pollSeconds
        Update-RaiseIfDue -State $State -When 'during the install'
    }
    $result = Complete-UpdateInstall -Handle $handle
    Complete-UpdateReport -Result $result
    if ($result.ResultCode -eq 4 -or $result.ResultCode -eq 5) { return 1 }
    return 0
}

# Searches first and raises only when an update is pending, so a night with nothing to install never touches the drain.
function Invoke-DrainedInstall($State) {
    $updates = Get-PendingUpdate
    if ($null -eq $updates -or @($updates).Count -eq 0) {
        Write-UpdateEvent -Type 'Information' -Message 'windows-update: nothing to install'
        return 0
    }
    if (-not (Start-DrainWait -State $State)) { return 1 }
    return Invoke-UpdateInstall -State $State -Updates $updates
}

# The task's entry: a barrier that was raised is always lifted, whatever happened after the raise.
function Invoke-DrainedUpdate {
    $state = @{ Root = $null; Started = $null; FirstRaise = $null; LastRaise = $null; Raised = $false }
    $code = 1
    try {
        try {
            $state.Root = Read-GateRoot
            $state.Started = Get-Now
            $state.LastRaise = $state.Started
            $code = Invoke-DrainedInstall -State $state
        }
        catch {
            $message = $_.Exception.Message
            if ($message -notlike 'windows-update:*') { $message = "windows-update: $message" }
            Write-UpdateEvent -Type 'Error' -Message $message
            $code = 1
        }
    }
    finally {
        if ($state.Raised -and -not (Invoke-LiftBarrier -State $state)) { $code = 1 }
    }
    return $code
}

if ($MyInvocation.InvocationName -ne '.') { exit (Invoke-DrainedUpdate) }
