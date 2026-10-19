#Requires -Version 7
# Case table for install-toolchains.ps1's argument handling. Copies the script into a scratch folder under the repo's
# .tmp/ with C:\build-box and C:\Tools\bin swapped for scratch paths, runs the copy under Windows PowerShell 5.1 with
# -ExecutionPolicy Bypass as the box does, and checks an unknown parameter or a -Only prefix that matches no step exits
# non-zero before creating anything. Every copy carries an exit 0 guard before its elevation check, so no case needs
# admin or can ever run a step on this PC. It also pins the step list the script reads from its own AST against a plain
# regex over the file. Prints ok/FAIL per case and exits with the failure count.
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

New-Item -ItemType Directory -Force $root | Out-Null
$sourceText = Get-Content -LiteralPath $sourceScript -Raw
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
}
finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
}

"`n$($script:fails) failure(s)"
exit $script:fails
