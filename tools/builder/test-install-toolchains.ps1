#Requires -Version 7
# Case table for install-toolchains.ps1's argument handling. Copies the script into a scratch folder under the repo's
# .tmp/ with C:\build-box and C:\Tools\bin swapped for scratch paths, runs the copy under Windows PowerShell 5.1 with
# -ExecutionPolicy Bypass as the box does, and checks an unknown parameter or a -Only prefix that matches no step exits
# non-zero before creating anything. Every copy carries an exit 0 guard before its elevation check, so no case needs
# admin or can ever run a step on this PC. It also pins the step list the script reads from its own AST against a plain
# regex over the file. The gate work root checks (its owner, DACL, mandatory label and link refusal) run on their
# function definitions taken from the AST, fed SDDL strings, icacls lines and a scratch junction, and the sccache server
# log's assignment is read from the AST. Prints ok/FAIL per case and exits with the failure count.
#
#   pwsh -NoProfile -File tools/builder/test-install-toolchains.ps1
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceScript = Join-Path $PSScriptRoot 'install-toolchains.ps1'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$root = Join-Path $repoRoot ".tmp/test-install-toolchains-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
$buildBox = Join-Path $root 'build-box'
$toolsParent = Join-Path $root 'Tools'
$tools = Join-Path $toolsParent 'bin'
$copy = Join-Path $root 'install-toolchains.ps1'
# The script refuses a -User that is not the account running it, so every case runs as this test's own account.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$userArguments = @('-User', $identity.Name)
$testElevated = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
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

# The step names from the script's own AST: every command named Invoke-Step whose first argument is a string literal
# or an expandable string, taken unexpanded. Any other name kind throws, as install-toolchains.ps1 refuses it.
function Get-StepName {
    param([Parameter(Mandatory)][string]$Path)
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    if ($null -ne $errors -and $errors.Count) { throw "$Path has parse errors: $($errors[0].Message)" }
    return @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true) |
        ForEach-Object {
            if ($_.GetCommandName() -ne 'Invoke-Step') { return }
            if ($_.CommandElements.Count -lt 2) { throw "Invoke-Step at line $($_.Extent.StartLineNumber) has no name" }
            $first = $_.CommandElements[1]
            if ($first -is [System.Management.Automation.Language.StringConstantExpressionAst]) { return $first.Value }
            if ($first -is [System.Management.Automation.Language.ExpandableStringExpressionAst]) { return $first.Value }
            throw "Invoke-Step at line $($_.Extent.StartLineNumber) has a name that is not a string"
        })
}

# Runs a scratch copy under Windows PowerShell 5.1 and returns its exit code and both streams.
function Invoke-Child {
    param([Parameter(Mandatory)][string]$Path, [string[]]$ScriptArguments)
    $tag = [guid]::NewGuid().ToString('N')
    $outFile = Join-Path $root "stdout-$tag.log"
    $errFile = Join-Path $root "stderr-$tag.log"
    $argumentList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Path) + @($ScriptArguments)
    $p = Start-Process -FilePath 'powershell.exe' -ArgumentList $argumentList -Wait -PassThru -NoNewWindow `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $stdOut = if (Test-Path -LiteralPath $outFile) { [IO.File]::ReadAllText($outFile) } else { '' }
    $stdErr = if (Test-Path -LiteralPath $errFile) { [IO.File]::ReadAllText($errFile) } else { '' }
    return [pscustomobject]@{ ExitCode = $p.ExitCode; StdOut = $stdOut; StdErr = $stdErr }
}

# What a refused run created that it must not have: the scratch build-box or the scratch Tools folder.
function Get-CreatedProblem {
    $present = @($buildBox, $toolsParent | Where-Object { Test-Path -LiteralPath $_ })
    if ($present.Count) { return "created $($present -join ', ')" }
    return ''
}

# The definitions of install-toolchains.ps1's functions $Name, taken from its AST, for the caller to dot-source; the whole
# script is never dot-sourced. A missing function throws, which fails the case.
function Get-InstallerFunctionText {
    param([Parameter(Mandatory)][string[]]$Name)
    $texts = foreach ($each in $Name) {
        $definition = $sourceAst.Find({
                param($node)
                $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $each
            }.GetNewClosure(), $true)
        if (-not $definition) { throw "install-toolchains.ps1 defines no $each" }
        $definition.Extent.Text
    }
    return $texts -join "`n"
}

# Runs install-toolchains.ps1's Assert-GateRootSecurity and its helpers and returns the message it threw, or '' when it
# passed.
function Invoke-GateRootSecurityCheck {
    param([Parameter(Mandatory)][string]$Sddl, [string[]]$IcaclsLines = @(), [string]$UserSid = $testUserSid)
    . ([scriptblock]::Create((Get-InstallerFunctionText 'Assert-GateRootSecurity', 'Get-GateRootAce', 'Assert-GateRootNoLabel')))
    try {
        Assert-GateRootSecurity -Sddl $Sddl -IcaclsLines $IcaclsLines -UserSid $UserSid
        return ''
    }
    catch {
        return $_.Exception.Message
    }
}

# A gate root check that must throw: returns '' when it threw a message matching $Expect, else what went wrong.
function Get-GateRootRefusalProblem {
    param([Parameter(Mandatory)][string]$Sddl, [string[]]$IcaclsLines = @(), [Parameter(Mandatory)][string]$Expect)
    $message = Invoke-GateRootSecurityCheck -Sddl $Sddl -IcaclsLines $IcaclsLines
    if (-not $message) { return "passed $Sddl, expected a refusal" }
    if ($message -notmatch $Expect) { return "the refusal does not match '$Expect': $message" }
    return ''
}

# Makes a scratch link to a scratch folder with mklink $MklinkSwitch, grants this user read through the link with
# icacls /L and then without it, and returns '' when the target's SDDL stayed unchanged under /L and changed without it
# exactly when $PlainChangesTarget. A link this account may not create (a symbolic link without Developer Mode or
# elevation) returns a skip line.
function Get-IcaclsLinkProblem {
    param([Parameter(Mandatory)][string]$MklinkSwitch, [Parameter(Mandatory)][bool]$PlainChangesTarget)
    $tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
    $target = Join-Path $root "icacls-target-$tag"
    $link = Join-Path $root "icacls-link-$tag"
    New-Item -ItemType Directory -Force $target | Out-Null
    $made = (& cmd.exe /c "mklink $MklinkSwitch `"$link`" `"$target`" 2>&1") -join ' '
    if ($LASTEXITCODE) {
        if ($MklinkSwitch -eq '/D') { return "skipped: this account cannot create a symbolic link (needs Developer Mode or elevation): $made" }
        return "mklink $MklinkSwitch exited $LASTEXITCODE`: $made"
    }
    try {
        $grant = "*$($identity.User.Value):(R)"
        $before = (Get-Acl -LiteralPath $target).Sddl
        & icacls.exe $link /L /grant $grant /q | Out-Null
        if ($LASTEXITCODE) { return "icacls /L /grant exited $LASTEXITCODE" }
        $afterL = (Get-Acl -LiteralPath $target).Sddl
        if ($afterL -ne $before) { return "icacls /L changed the link's target: $before -> $afterL" }
        & icacls.exe $link /grant $grant /q | Out-Null
        if ($LASTEXITCODE) { return "icacls /grant exited $LASTEXITCODE" }
        $changed = (Get-Acl -LiteralPath $target).Sddl -ne $before
        if ($changed -ne $PlainChangesTarget) { return "icacls without /L changed the target: $changed, expected $PlainChangesTarget" }
        return ''
    }
    finally {
        & cmd.exe /c "rmdir `"$link`"" | Out-Null
    }
}

# Runs install-toolchains.ps1's Assert-GateRootNotLink on $Path and returns the message it threw, or '' when it passed.
function Invoke-GateRootLinkCheck {
    param([Parameter(Mandatory)][string]$Path)
    . ([scriptblock]::Create((Get-InstallerFunctionText 'Assert-GateRootNotLink')))
    try {
        Assert-GateRootNotLink -Path $Path
        return ''
    }
    catch {
        return $_.Exception.Message
    }
}

# Stand-ins for the .NET types the prune task's command calls, swapped in by name: a static .NET call cannot be stubbed.
class FakeEventLog {
    static [System.Collections.Generic.List[object]] $Entries = [System.Collections.Generic.List[object]]::new()
    static [void] WriteEntry([string]$Source, [string]$Message, [System.Diagnostics.EventLogEntryType]$Type, [int]$EventId) {
        [FakeEventLog]::Entries.Add([pscustomobject]@{ Source = $Source; Message = $Message; Type = "$Type"; EventId = $EventId })
    }
}
class FakeDriveInfo {
    [long]$AvailableFreeSpace = 10GB
    FakeDriveInfo([string]$Name) {
        if ($Name -ne 'D:') { throw "the task asked for the free space of '$Name', expected D:" }
    }
}

# The prune task step's closing and whole command strings, built by the step's own assignments for a root at
# D:\gate-remote.
function Get-PruneCommandString {
    $block = Get-StepBlock 'Work-folder prune task'
    if (-not $block) { throw 'install-toolchains.ps1 has no Work-folder prune task step' }
    $names = 'eventSource', 'gateExe', 'drive', 'closingCommand', 'pruneCommand'
    $assignments = @($block.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
                $node.Left.VariablePath.UserPath -in $names
            }.GetNewClosure(), $true) | Sort-Object { $_.Extent.StartOffset })
    $GateRoot = 'D:\gate-remote'
    foreach ($assignment in $assignments) { Invoke-Expression $assignment.Extent.Text }
    foreach ($name in 'closingCommand', 'pruneCommand') {
        if (-not (Get-Variable -Name $name -Scope Local -ErrorAction SilentlyContinue)) { throw "the step assigns no `$$name" }
    }
    return [pscustomobject]@{ Closing = $closingCommand; Prune = $pruneCommand }
}

# The script block of install-toolchains.ps1's step whose name starts with $Prefix, or $null when it has none.
function Get-StepBlock {
    param([Parameter(Mandatory)][string]$Prefix)
    $step = $sourceAst.Find({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq 'Invoke-Step' -and
            $node.CommandElements.Count -ge 3 -and "$($node.CommandElements[1].Value)" -like "$Prefix*"
        }.GetNewClosure(), $true)
    if (-not $step) { return $null }
    return $step.CommandElements[2]
}

# The commands named $Name inside $Block.
function Get-BlockCommand {
    param([Parameter(Mandatory)]$Block, [Parameter(Mandatory)][string[]]$Name)
    return , @($Block.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -in $Name
            }.GetNewClosure(), $true))
}

# The argument of $Command's parameter -$Name, or $null when the parameter is absent or a bare switch.
function Get-ParameterArgument {
    param([Parameter(Mandatory)]$Command, [Parameter(Mandatory)][string]$Name)
    $elements = $Command.CommandElements
    for ($i = 0; $i -lt $elements.Count; $i++) {
        $element = $elements[$i]
        if ($element -isnot [System.Management.Automation.Language.CommandParameterAst] -or $element.ParameterName -ne $Name) { continue }
        if ($element.Argument) { return $element.Argument }
        if ($i + 1 -lt $elements.Count -and $elements[$i + 1] -isnot [System.Management.Automation.Language.CommandParameterAst]) {
            return $elements[$i + 1]
        }
        return $null
    }
    return $null
}

# The text of $Expression and of every assignment in $Block to a variable it reads, followed to $Depth levels, with the
# names of the variables read on the way.
function Resolve-BlockExpression {
    param([Parameter(Mandatory)]$Block, [Parameter(Mandatory)]$Expression, [int]$Depth = 4)
    $texts = @($Expression.Extent.Text)
    $variables = @($Expression.FindAll({ param($node) $node -is [System.Management.Automation.Language.VariableExpressionAst] }, $true) |
        ForEach-Object { $_.VariablePath.UserPath })
    if ($Depth -gt 0) {
        foreach ($variable in @($variables | Select-Object -Unique)) {
            $assignments = @($Block.FindAll({
                        param($node)
                        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                        $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
                        $node.Left.VariablePath.UserPath -eq $variable
                    }.GetNewClosure(), $true))
            foreach ($assignment in $assignments) {
                $inner = Resolve-BlockExpression -Block $Block -Expression $assignment.Right -Depth ($Depth - 1)
                $texts += $inner.Texts
                $variables += $inner.Variables
            }
        }
    }
    return [pscustomobject]@{ Texts = $texts; Variables = $variables }
}

New-Item -ItemType Directory -Force $root | Out-Null
$sourceText =Get-Content -LiteralPath $sourceScript -Raw
$sourceAst = [System.Management.Automation.Language.Parser]::ParseInput($sourceText, [ref]$null, [ref]$null)
# The gate root's three ACEs: SYSTEM, Administrators and a made-up builder user, full control, inherited by both.
$testUserSid = 'S-1-5-21-1-2-3-1001'
$aceSystem = '(A;OICI;FA;;;SY)'
$aceAdmins = '(A;OICI;FA;;;BA)'
$aceUser = "(A;OICI;FA;;;$testUserSid)"
$protectedTriple = "O:BAG:SYD:PAI$aceSystem$aceAdmins$aceUser"
$scratchText = $sourceText.Replace('C:\build-box', $buildBox).Replace('C:\Tools\bin', $tools)
# A missed replacement would let a case write to the real C:\build-box on this machine, so fail before any run.
if ($scratchText.Contains('C:\build-box')) { throw 'the scratch copy still names C:\build-box' }
# No case may run a step: while the parameter binding is broken an unknown parameter is silently ignored and every
# step would run on this PC. The guard sits after the param block, the -User check and the -Only validation, so they
# stay under test, and before the elevation check, so no case needs an elevated session.
$anchor = [regex]::Match($scratchText, '(?m)^\$elevated = ')
if (-not $anchor.Success) { throw 'the scratch copy has no top-level $elevated line to guard' }
$guardText = "# test guard: no step may run`nexit 0`n"
# A second copy keeps the elevation check and guards the first top-level New-Item after it instead, so an unelevated
# run shows the check's own exit, and a broken check still stops at the guard.
$writeAnchor = [regex]::Match($scratchText, '(?m)^New-Item -ItemType Directory')
if (-not $writeAnchor.Success) { throw 'the scratch copy has no top-level New-Item -ItemType Directory line to guard' }
$elevationCopy = Join-Path $root 'install-toolchains-elevation.ps1'
Set-Content -LiteralPath $elevationCopy -Value $scratchText.Insert($writeAnchor.Index, $guardText) -NoNewline
$scratchText = $scratchText.Insert($anchor.Index, $guardText)
if (@([regex]::Matches($scratchText, '(?m)^# test guard: no step may run$')).Count -ne 1) {
    throw 'the guard line was not inserted exactly once'
}
Set-Content -LiteralPath $copy -Value $scratchText -NoNewline
# A copy whose step name is neither a literal nor an expandable string, to prove the AST guard refuses it.
$badNameCopy = Join-Path $root 'install-toolchains-badname.ps1'
$badNameText = $scratchText.Replace($guardText, "Invoke-Step `$x {}`n" + $guardText)
if ($badNameText -eq $scratchText) { throw 'the bad-name line was not inserted' }
Set-Content -LiteralPath $badNameCopy -Value $badNameText -NoNewline

try {
    Test-Case '-Bogus 1 exits on the binding error and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Bogus', '1'))
        if ($r.ExitCode -eq 0) { return "exit 0, the unknown parameter was ignored; stdout: $($r.StdOut)" }
        if ($r.StdErr -notmatch [regex]::Escape("parameter name 'Bogus'")) {
            return "stderr is not the binding error: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case '-Onyl Python exits on the binding error and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Onyl', 'Python'))
        if ($r.ExitCode -eq 0) { return "exit 0, the misspelled parameter was ignored; stdout: $($r.StdOut)" }
        if ($r.StdErr -notmatch [regex]::Escape("parameter name 'Onyl'")) {
            return "stderr is not the binding error: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'nothing above the $elevated line runs a step, writes, registers a task, sets an ACL or starts a process' {
        $source = [regex]::Match($sourceText, '(?m)^\$elevated = ')
        if (-not $source.Success) { return 'install-toolchains.ps1 has no top-level $elevated line' }
        $ast = [System.Management.Automation.Language.Parser]::ParseInput($sourceText, [ref]$null, [ref]$null)
        $early = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true) |
            Where-Object { $_.Extent.StartOffset -lt $source.Index } |
            Where-Object { $_.GetCommandName() -match '^(Invoke-Step|New-Item|Register-.*|icacls(\.exe)?|Start-Process)$' })
        if ($early.Count) {
            $named = $early | ForEach-Object { "$($_.GetCommandName()) at line $($_.Extent.StartLineNumber)" }
            return "called before the elevation check: $($named -join ', ')"
        }
        return ''
    }

    Test-Case 'no -User exits 2 with the required line and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments @('-Only', 'Git')
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape('-User is required')) { return "stderr is not the required line: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case 'an unknown -User exits 2 naming it and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments @('-User', 'no-such-account-532')
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape("-User 'no-such-account-532' is not an account")) {
            return "stderr is not the unknown-account line: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'a -User other than the running account exits 2 and creates nothing' {
        if ($identity.User.Value -eq 'S-1-5-18') { return 'skipped: the test runs as SYSTEM, the other account it names' }
        $r = Invoke-Child -Path $copy -ScriptArguments @('-User', 'SYSTEM')
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape("not -User 'SYSTEM'; run it as SYSTEM")) {
            return "stderr is not the wrong-account line: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'a -GateRoot with a ; exits 2 and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-GateRoot', 'D:\gate;x'))
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape("-GateRoot 'D:\gate;x' is not a local path")) {
            return "stderr is not the path line: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'a relative -SccacheDir exits 2 and creates nothing' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-SccacheDir', 'sccache'))
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape("-SccacheDir 'sccache' is not a local path")) {
            return "stderr is not the path line: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'an unelevated run exits 1 at the elevation check and creates nothing' {
        if ($testElevated) { return 'skipped: the test runs elevated' }
        $r = Invoke-Child -Path $elevationCopy -ScriptArguments ($userArguments + @('-Only', 'Git'))
        if ($r.ExitCode -ne 1) { return "exit $($r.ExitCode), expected 1; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape('install-toolchains: not elevated')) {
            return "stderr is not the elevation line: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case '-Only NoSuchStep exits 2, names the prefix and lists the steps' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'NoSuchStep'))
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape('NoSuchStep')) { return "stderr does not name the prefix: $($r.StdErr)" }
        $names = @(Get-StepName $copy)
        $shown = @($names | Where-Object { $r.StdErr -match [regex]::Escape($_) })
        if ($shown.Count -lt 3) { return "stderr lists $($shown.Count) step names, expected at least 3: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case '-Only NoSuch,AlsoNot (one comma word) exits 2' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'NoSuch,AlsoNot'))
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        return ''
    }

    Test-Case 'the AST step list equals the quoted Invoke-Step names' {
        $astNames = @(Get-StepName $copy)
        if (-not $astNames.Count) { return 'the AST read found no step names' }
        $text = Get-Content -LiteralPath $copy -Raw
        $regexNames = @([regex]::Matches($text, 'Invoke-Step\s+(?:''([^'']*)''|"([^"]*)")') |
            ForEach-Object { if ($_.Groups[1].Success) { $_.Groups[1].Value } else { $_.Groups[2].Value } })
        $diff = @(Compare-Object -ReferenceObject @($regexNames | Sort-Object) -DifferenceObject @($astNames | Sort-Object))
        if ($diff.Count) {
            $shown = ($diff | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }) -join '; '
            return "AST and regex disagree: $shown"
        }
        return ''
    }

    Test-Case '-Only Git exits 0 at the guard with no error' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'Git'))
        if ($r.ExitCode -ne 0) { return "exit $($r.ExitCode), expected 0; stderr: $($r.StdErr)" }
        if ($r.StdErr) { return "stderr: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case '-Only WSL exits 0 at the guard with no error' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'WSL'))
        if ($r.ExitCode -ne 0) { return "exit $($r.ExitCode), expected 0; stderr: $($r.StdErr)" }
        if ($r.StdErr) { return "stderr: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case '-Only Node,.NET,keeper exits 0 at the guard' {
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'Node,.NET,keeper'))
        if ($r.ExitCode -ne 0) { return "exit $($r.ExitCode), expected 0; stderr: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case '-Only Work-folder runs only the prune task step' {
        $matched = @(Get-StepName $copy | Where-Object { $_ -like 'Work-folder*' })
        if ($matched.Count -ne 1) { return "-Only Work-folder matches $($matched.Count) steps, expected 1: $($matched -join ', ')" }
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'Work-folder'))
        if ($r.ExitCode -ne 0) { return "exit $($r.ExitCode), expected 0; stderr: $($r.StdErr)" }
        if ($r.StdErr) { return "stderr: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case '-Only Summary is reserved, runs no step and exits 0' {
        $clash = @(Get-StepName $copy | Where-Object { $_ -like 'Summary*' })
        if ($clash.Count) { return "steps start with the reserved prefix Summary: $($clash -join ', ')" }
        $r = Invoke-Child -Path $copy -ScriptArguments ($userArguments + @('-Only', 'Summary'))
        if ($r.ExitCode -ne 0) { return "exit $($r.ExitCode), expected 0; stderr: $($r.StdErr)" }
        if ($r.StdErr) { return "stderr: $($r.StdErr)" }
        Get-CreatedProblem
    }

    Test-Case 'an Invoke-Step name that is not a string exits 2 with the guard message' {
        $r = Invoke-Child -Path $badNameCopy -ScriptArguments $userArguments
        if ($r.ExitCode -ne 2) { return "exit $($r.ExitCode), expected 2; stderr: $($r.StdErr)" }
        if ($r.StdErr -notmatch [regex]::Escape('has a name that is not a string')) {
            return "stderr is not the not-a-string message: $($r.StdErr)"
        }
        Get-CreatedProblem
    }

    Test-Case 'gate root: the protected SYSTEM, Administrators and user triple passes' {
        $lines = @("D:\gate-remote NT AUTHORITY\SYSTEM:(OI)(CI)(F)", 'Successfully processed 1 files; Failed processing 0 files')
        Invoke-GateRootSecurityCheck -Sddl $protectedTriple -IcaclsLines $lines
    }

    Test-Case 'gate root: the triple passes in another ACE order' {
        Invoke-GateRootSecurityCheck -Sddl "O:BAG:SYD:PAI$aceUser$aceSystem$aceAdmins"
    }

    Test-Case 'gate root: the triple passes with SYSTEM and Administrators written as SIDs, under D:P' {
        Invoke-GateRootSecurityCheck -Sddl "O:S-1-5-32-544G:SYD:P(A;OICI;FA;;;S-1-5-32-544)$aceUser(A;OICI;FA;;;S-1-5-18)"
    }

    Test-Case 'gate root: the builder user as owner fails' {
        Get-GateRootRefusalProblem -Sddl "O:${testUserSid}G:SYD:PAI$aceSystem$aceAdmins$aceUser" `
            -Expect ([regex]::Escape("its owner is $testUserSid, not Administrators; rerun -Only ""Gate work root"""))
    }

    Test-Case 'gate root: a foreign owner fails' {
        Get-GateRootRefusalProblem -Sddl "O:AUG:SYD:PAI$aceSystem$aceAdmins$aceUser" -Expect ([regex]::Escape('its owner is AU, not Administrators'))
    }

    Test-Case 'gate root: an inherited ACE says to rerun' {
        Get-GateRootRefusalProblem -Sddl "$protectedTriple(A;ID;0x1301bf;;;AU)" `
            -Expect ([regex]::Escape('it is inherited; rerun -Only "Gate work root", whose icacls /inheritance:r drops it'))
    }

    Test-Case 'gate root: a deny ACE says /remove:d with its SID' {
        Get-GateRootRefusalProblem -Sddl "$protectedTriple(D;OICI;FA;;;BU)" -Expect ([regex]::Escape('/remove:d *S-1-5-32-545'))
    }

    Test-Case 'gate root: another allow ACE says /remove:g with its SID' {
        Get-GateRootRefusalProblem -Sddl "$protectedTriple(A;OICI;0x1301bf;;;AU)" -Expect ([regex]::Escape('/remove:g *S-1-5-11'))
    }

    Test-Case 'gate root: a junction is refused as a link' {
        $target = Join-Path $root 'junction-target'
        $link = Join-Path $root 'junction-link'
        New-Item -ItemType Directory -Force $target | Out-Null
        & cmd.exe /c "mklink /J `"$link`" `"$target`"" | Out-Null
        if ($LASTEXITCODE) { return "mklink /J exited $LASTEXITCODE" }
        try {
            $message = Invoke-GateRootLinkCheck -Path $link
            $expect = "gate work root ${link}: it is a link (reparse point), not a folder; remove the link and rerun -Only ""Gate work root"""
            if ($message -ne $expect) { return "expected '$expect', got '$message'" }
            return ''
        }
        finally {
            & cmd.exe /c "rmdir `"$link`"" | Out-Null
        }
    }

    # Probed on Windows 11 26200: icacls and Get-Acl act on a junction itself, /L or not, so a junction's target is never
    # touched. A directory symbolic link is followed without /L, which is what the step's /L guards against.
    Test-Case 'icacls on a junction leaves its target alone, with /L and without' {
        Get-IcaclsLinkProblem -MklinkSwitch '/J' -PlainChangesTarget $false
    }

    Test-Case 'icacls /L on a directory symbolic link leaves its target alone, and without /L changes it' {
        Get-IcaclsLinkProblem -MklinkSwitch '/D' -PlainChangesTarget $true
    }

    Test-Case 'gate root: a plain folder passes the link check' {
        $folder = Join-Path $root 'plain-folder'
        New-Item -ItemType Directory -Force $folder | Out-Null
        Invoke-GateRootLinkCheck -Path $folder
    }

    Test-Case 'gate root: an unprotected DACL fails' {
        Get-GateRootRefusalProblem -Sddl "O:BAG:SYD:AI$aceSystem$aceAdmins$aceUser" -Expect 'not protected'
    }

    Test-Case 'gate root: an inherited Authenticated Users ACE fails' {
        Get-GateRootRefusalProblem -Sddl "$protectedTriple(A;ID;0x1301bf;;;AU)" -Expect ([regex]::Escape('(A;ID;0x1301bf;;;AU)'))
    }

    Test-Case 'gate root: a Users read ACE fails' {
        Get-GateRootRefusalProblem -Sddl "$protectedTriple(A;OICI;0x1200a9;;;BU)" -Expect ([regex]::Escape('(A;OICI;0x1200a9;;;BU)'))
    }

    Test-Case 'gate root: a missing user ACE fails' {
        Get-GateRootRefusalProblem -Sddl "O:BAG:SYD:PAI$aceSystem$aceAdmins" -Expect "lacks.*$testUserSid"
    }

    Test-Case 'gate root: a read-only user ACE fails' {
        $sddl = "O:BAG:SYD:PAI$aceSystem$aceAdmins(A;OICI;0x1200a9;;;$testUserSid)"
        Get-GateRootRefusalProblem -Sddl $sddl -Expect ([regex]::Escape("(A;OICI;0x1200a9;;;$testUserSid)"))
    }

    Test-Case 'gate root: an added deny ACE fails' {
        Get-GateRootRefusalProblem -Sddl "O:BAG:SYD:PAI(D;OICI;FA;;;BU)$aceSystem$aceAdmins$aceUser" `
            -Expect ([regex]::Escape('(D;OICI;FA;;;BU)'))
    }

    Test-Case 'gate root: a mandatory label in the icacls lines fails' {
        $lines = @("D:\gate-remote NT AUTHORITY\SYSTEM:(OI)(CI)(F)", '               Mandatory Label\High Mandatory Level:(OI)(CI)(NW)')
        Get-GateRootRefusalProblem -Sddl $protectedTriple -IcaclsLines $lines -Expect 'mandatory label'
    }

    Test-Case 'the sccache server log is set inside a step, from LOCALAPPDATA, outside C:\build-box' {
        $assignment = $sourceAst.Find({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
                $node.Left.VariablePath.UserPath -eq 'sccacheServerLog'
            }, $true)
        if (-not $assignment) { return 'install-toolchains.ps1 assigns no $sccacheServerLog' }
        $value = $assignment.Right.Extent.Text
        $variables = @($assignment.Right.FindAll({ param($node) $node -is [System.Management.Automation.Language.VariableExpressionAst] }, $true) |
            ForEach-Object { $_.VariablePath.UserPath })
        # $scriptFiles, $dl and $log are the script's C:\build-box paths.
        $boxVariables = @($variables | Where-Object { $_ -in 'scriptFiles', 'dl', 'log' })
        if ($boxVariables.Count) {
            return "`$sccacheServerLog = $value builds on `$$($boxVariables[0]), a C:\build-box path the builder user cannot write"
        }
        $strings = @($assignment.Right.FindAll({
                    param($node)
                    $node -is [System.Management.Automation.Language.StringConstantExpressionAst] -or
                    $node -is [System.Management.Automation.Language.ExpandableStringExpressionAst]
                }, $true) | ForEach-Object { $_.Value })
        if (@($strings | Where-Object { $_ -match '^[A-Za-z]:\\build-box' }).Count) {
            return "`$sccacheServerLog = $value names C:\build-box, which the builder user cannot write"
        }
        if ('env:LOCALAPPDATA' -notin $variables) { return "`$sccacheServerLog = $value does not build on `$env:LOCALAPPDATA" }
        $step = $assignment.Parent
        while ($step -and $step -isnot [System.Management.Automation.Language.ScriptBlockExpressionAst]) { $step = $step.Parent }
        if (-not $step) { return "`$sccacheServerLog is set at top level, so an unset LOCALAPPDATA stops the whole script" }
        return ''
    }

    Test-Case 'the prune task step registers S4U Limited every 6 hours running gate box prune' {
        $block = Get-StepBlock 'Work-folder prune task'
        if (-not $block) { return 'install-toolchains.ps1 has no Work-folder prune task step' }
        $principals = Get-BlockCommand $block 'New-ScheduledTaskPrincipal'
        if ($principals.Count -ne 1) { return "the step has $($principals.Count) New-ScheduledTaskPrincipal calls, expected 1" }
        $logon = Get-ParameterArgument $principals[0] 'LogonType'
        $level = Get-ParameterArgument $principals[0] 'RunLevel'
        if ("$($logon.Extent.Text)" -ne 'S4U' -or "$($level.Extent.Text)" -ne 'Limited') {
            return "the principal is $($principals[0].Extent.Text), expected -LogonType S4U -RunLevel Limited"
        }
        $triggers = Get-BlockCommand $block 'New-ScheduledTaskTrigger'
        if ($triggers.Count -ne 1) { return "the step has $($triggers.Count) New-ScheduledTaskTrigger calls, expected 1" }
        $interval = Get-ParameterArgument $triggers[0] 'RepetitionInterval'
        if (-not $interval) { return "the trigger $($triggers[0].Extent.Text) has no -RepetitionInterval" }
        $intervalText = (Resolve-BlockExpression -Block $block -Expression $interval).Texts -join ' '
        if ($intervalText -notmatch 'New-TimeSpan\s+-Hours\s+6\b') { return "the repetition interval is $intervalText, not New-TimeSpan -Hours 6" }
        if ($triggers[0].Extent.Text -match '-RepetitionDuration') { return "the trigger $($triggers[0].Extent.Text) sets -RepetitionDuration" }
        $settings = Get-BlockCommand $block 'New-ScheduledTaskSettingsSet'
        if ($settings.Count -ne 1) { return "the step has $($settings.Count) New-ScheduledTaskSettingsSet calls, expected 1" }
        $instances = Get-ParameterArgument $settings[0] 'MultipleInstances'
        if ("$($instances.Extent.Text)" -ne 'IgnoreNew') {
            return "the settings are $($settings[0].Extent.Text), expected -MultipleInstances IgnoreNew"
        }
        $actions = Get-BlockCommand $block 'New-ScheduledTaskAction'
        if ($actions.Count -ne 1) { return "the step has $($actions.Count) New-ScheduledTaskAction calls, expected 1" }
        $argument = Get-ParameterArgument $actions[0] 'Argument'
        if (-not $argument) { return "the action $($actions[0].Extent.Text) has no -Argument" }
        $resolved = Resolve-BlockExpression -Block $block -Expression $argument
        $text = $resolved.Texts -join ' '
        foreach ($wanted in 'C:\Program Files\gate\bin\gate.exe', 'box prune --root') {
            if (-not $text.Contains($wanted)) { return "the action argument does not name $wanted`: $text" }
        }
        if ('GateRoot' -notin $resolved.Variables) { return "the action argument does not build on `$GateRoot: $text" }
        return ''
    }

    Test-Case 'the prune task closing event is Information on exit 0 and Warning on another exit or none' {
        $closing = (Get-PruneCommandString).Closing
        foreach ($type in '[System.Diagnostics.EventLog]::', '[System.IO.DriveInfo]::') {
            if (-not $closing.Contains($type)) { return "the closing command does not call $type`: $closing" }
        }
        $closing = $closing.Replace('[System.Diagnostics.EventLog]::', '[FakeEventLog]::').Replace('[System.IO.DriveInfo]::', '[FakeDriveInfo]::')
        $cases = @(
            @{ Code = 0; Type = 'Information'; Message = 'gate box prune exited 0; D: free 10 GB' }
            @{ Code = 3; Type = 'Warning'; Message = 'gate box prune exited 3; D: free 10 GB' }
            @{ Code = $null; Type = 'Warning'; Message = 'gate box prune exited (no exit code); D: free 10 GB' }
        )
        foreach ($case in $cases) {
            [FakeEventLog]::Entries.Clear()
            & {
                $LASTEXITCODE = $case.Code
                Invoke-Expression $closing
            }
            $events = [FakeEventLog]::Entries.ToArray()
            if ($events.Count -ne 1) { return "exit '$($case.Code)' wrote $($events.Count) events, expected 1" }
            if ($events[0].Source -ne 'gate-prune' -or $events[0].EventId -ne 1) {
                return "exit '$($case.Code)' wrote source '$($events[0].Source)' event $($events[0].EventId), expected gate-prune event 1"
            }
            if ($events[0].Type -ne $case.Type -or $events[0].Message -ne $case.Message) {
                return "exit '$($case.Code)' wrote $($events[0].Type) '$($events[0].Message)', expected $($case.Type) '$($case.Message)'"
            }
        }
        return ''
    }

    Test-Case 'the prune task command turns module auto-loading off and calls no module cmdlet' {
        $prune = (Get-PruneCommandString).Prune
        if (-not $prune.StartsWith("`$PSModuleAutoLoadingPreference = 'None';")) {
            return "the task command does not start by turning module auto-loading off: $prune"
        }
        $errors = $null
        $commandAst = [System.Management.Automation.Language.Parser]::ParseInput($prune, [ref]$null, [ref]$errors)
        if ($null -ne $errors -and $errors.Count) { return "the task command does not parse: $($errors[0].Message): $prune" }
        # ForEach-Object is in Microsoft.PowerShell.Core, which is loaded from the PowerShell install, never auto-loaded.
        $called = @($commandAst.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true) |
            ForEach-Object { $_.GetCommandName() })
        $others = @($called | Where-Object { $_ -notin 'C:\Program Files\gate\bin\gate.exe', 'ForEach-Object' })
        if ($others.Count) { return "the task command calls $($others -join ', '), which may auto-load a module: $prune" }
        return ''
    }

    Test-Case 'the prune task writes no script file' {
        $block = Get-StepBlock 'Work-folder prune task'
        if (-not $block) { return 'install-toolchains.ps1 has no Work-folder prune task step' }
        $writes = (Get-BlockCommand $block 'Set-Content', 'Out-File', 'Add-Content') + @($block.FindAll({
                    param($node)
                    $node -is [System.Management.Automation.Language.InvokeMemberExpressionAst] -and
                    "$($node.Member.Extent.Text)" -in 'WriteAllText', 'WriteAllLines', 'WriteAllBytes'
                }, $true))
        foreach ($write in $writes) {
            $text = (Resolve-BlockExpression -Block $block -Expression $write).Texts -join ' '
            if ($text -match '\.ps1\b') { return "the step writes a script file: $($write.Extent.Text)" }
        }
        return ''
    }
}
finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
}

"`n$($script:fails) failure(s)"
exit $script:fails
