#Requires -Version 7

<#
.SYNOPSIS
    Builds gate.exe in release and installs it into %LOCALAPPDATA%\gate\bin, putting that folder on the user's PATH.

.DESCRIPTION
    A running gate holds its own executable open, so a rebuild into target\release cannot replace an installed copy,
    and installed gates never run from target\, which build cleaners empty. This script builds through the repo gate,
    copies the built executable beside the installed one under a temporary name, moves an installed gate.exe aside to
    gate.exe.<stamp>.<pid>.old, which works while a gate runs it, and renames the temporary copy into place, so the
    name gate.exe never points at nothing. It deletes the aside copies no process holds any more and leaves the ones a
    gate still runs for a later install. It then adds %LOCALAPPDATA%\gate\bin to the user's PATH when it is missing, by
    reading and writing the raw Path value of the user environment registry key, which keeps that value's kind and its
    unexpanded %NAME% entries, and tells the running windows the environment changed. It never touches the state root's
    files. Run with -WhatIf to list every step without taking it.

.PARAMETER Exe
    Installs this prebuilt executable instead of building one; the build is skipped. For tests only.

.PARAMETER EnvironmentKey
    The user environment registry key the PATH entry is read from and written to, HKCU:\Environment by default. For
    tests only: the environment change is broadcast only for the real key.

.EXAMPLE
    pwsh -NoProfile -File tools/install.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$Exe,
    [string]$EnvironmentKey = 'HKCU:\Environment'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

# One line on standard error, the way the rest of the house writes one.
function Write-InstallError {
    param([string]$Line)

    [Console]::Error.WriteLine($Line)
}

# What `gate --version` prints after `gate `, or $null when the program cannot be run or prints something else.
function Get-GateVersion {
    param([string]$Path)

    try {
        $output = & $Path '--version' 2>$null
    }
    catch {
        return $null
    }
    $line = [string]($output | Select-Object -First 1)
    if ($line -notmatch '^gate\s+(.+?)\s*$') { return $null }
    return $Matches[1]
}

# The first 8 hex of the SHA-256 of what `git diff HEAD --binary` writes in $Root, hashed as the bytes git writes them;
# $null when git cannot be run or fails.
function Get-DiffHash {
    param([string]$Root)

    $start = [Diagnostics.ProcessStartInfo]::new('git')
    foreach ($arg in '--no-optional-locks', '-C', $Root, 'diff', 'HEAD', '--binary') { $start.ArgumentList.Add($arg) }
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    try {
        $process = [Diagnostics.Process]::Start($start)
        try {
            $errors = $process.StandardError.ReadToEndAsync()
            $sha = [Security.Cryptography.SHA256]::Create()
            try {
                $digest = $sha.ComputeHash($process.StandardOutput.BaseStream)
            }
            finally {
                $sha.Dispose()
            }
            $process.WaitForExit()
            $null = $errors.Result
            if ($process.ExitCode -ne 0) { return $null }
        }
        finally {
            $process.Dispose()
        }
    }
    catch {
        return $null
    }
    return -join ($digest[0..3] | ForEach-Object { $_.ToString('x2') })
}

# The commit this build is being made from, as `gate --version` names it: `git rev-parse --short=12 HEAD`, with
# `-dirty.<hash>` added when tracked files differ, <hash> the first 8 hex of the SHA-256 of `git diff HEAD --binary`, so a
# build of a dirty tree is told apart from a build of its commit and from a build of other changes to it. Git runs with
# --no-optional-locks, so reading the tree never takes the index lock. $null when git cannot be run here or any of its
# calls fails, which builds a gate that names no commit and so says `+dev`.
function Get-BuildId {
    param([string]$Root)

    try {
        $head = & git --no-optional-locks -C $Root rev-parse --short=12 HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace([string]$head)) { return $null }
        $dirty = & git --no-optional-locks -C $Root status --porcelain --untracked-files=no 2>$null
        if ($LASTEXITCODE -ne 0) { return $null }
    }
    catch {
        return $null
    }
    $id = ([string]$head).Trim()
    if ([string]::IsNullOrWhiteSpace([string]$dirty)) { return $id }
    $hash = Get-DiffHash -Root $Root
    if ($null -eq $hash) { return $null }
    return "$id-dirty.$hash"
}

# Runs the release build through the repo gate from the repo root, where the gate's -Log lands, and returns its status.
# The build runs with -Local, so it is never routed to a builder. $BuildId reaches cargo as GATE_BUILD_ID for this one
# build, so the exe it writes names the commit it was made from; an empty one removes GATE_BUILD_ID for the build, so a
# value the caller set never leaks in, and builds a gate that names no commit (`+dev`).
function Invoke-ReleaseBuild {
    param([string]$Root, [string]$BuildId)

    $gate = Join-Path $Root 'tools\gate.ps1'
    Push-Location $Root
    try {
        $before = $env:GATE_BUILD_ID
        try {
            if ([string]::IsNullOrEmpty($BuildId)) {
                Remove-Item Env:GATE_BUILD_ID -ErrorAction Ignore
            }
            else {
                $env:GATE_BUILD_ID = $BuildId
            }
            & pwsh -NoProfile -File $gate -Log '.tmp/install-build.log' -TimeoutSeconds 900 -Slot heavy -Local -- `
                cargo build --release -p gate | Out-Host
            return [int]$LASTEXITCODE
        }
        finally {
            $env:GATE_BUILD_ID = $before
        }
    }
    finally {
        Pop-Location
    }
}

# Whether $Raw's entries hold $Folder, compared case-insensitively with a trailing backslash trimmed and %NAME%
# entries expanded for the comparison only.
function Test-PathEntry {
    param([string]$Raw, [string]$Folder)

    if ([string]::IsNullOrEmpty($Raw)) { return $false }
    foreach ($entry in $Raw -split ';') {
        if ([string]::IsNullOrEmpty($entry)) { continue }
        if ([Environment]::ExpandEnvironmentVariables($entry).TrimEnd([char]'\') -ieq $Folder) { return $true }
    }
    return $false
}

# Whether another process still holds $Path. A running gate's exe is held by a mapped image, which keeps no open
# handle, so an open that shares nothing succeeds on it even while no other handle exists: the probe asks for write
# access, which the mapping refuses with a sharing violation. PowerShell wraps that refusal, hence the base exception.
function Test-FileHeld {
    param([string]$Path)

    try {
        $handle = [IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')
    }
    catch {
        return $_.Exception.GetBaseException().HResult -eq -2147024864
    }
    $handle.Dispose()
    return $false
}

# Deletes every aside copy in $Dir that no process holds any more. One a gate still runs is left for a later install,
# and left silently: its delete fails, and the probe shows the file is still held.
function Remove-AsideCopy {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Dir)

    foreach ($file in Get-ChildItem -LiteralPath $Dir -Filter 'gate.exe.*.old' -File) {
        if (-not $PSCmdlet.ShouldProcess($file.FullName, 'Delete')) { continue }
        try {
            Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
        }
        catch {
            if (-not (Test-FileHeld -Path $file.FullName)) {
                Write-InstallError "install: could not delete $($file.FullName): $($_.Exception.Message)"
            }
        }
    }
}

# Tells the running windows that the user's environment changed, as `setx` does, so a terminal started from Start or
# Explorer after this install reads the new PATH. The interop is added inside the guard too, so a broadcast that fails
# or times out, and even the type failing to load, is not an error.
function Send-EnvironmentChange {
    $broadcast = [IntPtr]0xffff
    $settingChange = 0x001A
    $abortIfHung = 0x0002
    $result = [IntPtr]::Zero
    try {
        if (-not ('Gate.Install' -as [type])) {
            Add-Type -Namespace Gate -Name Install -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern IntPtr SendMessageTimeoutW(IntPtr hWnd, uint msg, IntPtr wParam, string lParam, uint flags, uint timeout, out IntPtr result);
'@
        }
        [Gate.Install]::SendMessageTimeoutW($broadcast, $settingChange, [IntPtr]::Zero, 'Environment', $abortIfHung, 5000, [ref]$result) | Out-Null
    }
    catch {
        Write-Verbose "The environment change could not be broadcast: $($_.Exception.Message)"
    }
}

# Adds $Folder to the user PATH in $Key when it is missing, and prepends it to this process's PATH. The value is read
# and written with DoNotExpandEnvironmentNames, so %NAME% entries stay unexpanded and the value keeps its kind. The
# environment change is broadcast only when $Key is the real user key, not for a test key (-EnvironmentKey).
function Add-UserPath {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Folder, [string]$Key)

    $item = Get-Item -LiteralPath $Key -ErrorAction SilentlyContinue
    $raw = $null
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    if ($null -ne $item) {
        $raw = $item.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $raw) { $kind = $item.GetValueKind('Path') }
        $item.Dispose()
    }

    if (-not (Test-PathEntry -Raw $raw -Folder $Folder)) {
        if ($PSCmdlet.ShouldProcess($Key, "Add $Folder to the user PATH")) {
            $prefix = if ([string]::IsNullOrEmpty($raw)) { '' } else { $raw.TrimEnd([char]';') }
            $value = if ([string]::IsNullOrEmpty($prefix)) { $Folder } else { $prefix + ';' + $Folder }
            try {
                if ($null -eq $item) {
                    $created = New-Item -Path $Key -Force
                    $created.Dispose()
                }
                Set-ItemProperty -LiteralPath $Key -Name 'Path' -Value $value -Type $kind
            }
            catch {
                Write-InstallError "install: could not update your user PATH: $($_.Exception.Message)"
                exit 1
            }
            Write-Output "install: added $Folder to your user PATH; open a new terminal to use gate"
            if ($Key -ieq 'HKCU:\Environment') { Send-EnvironmentChange }
        }
    }

    if (-not $WhatIfPreference) { $env:Path = "$Folder;$env:Path" }
}

if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
    Write-InstallError 'install: LOCALAPPDATA is not set'
    exit 1
}

$installDir = Join-Path $env:LOCALAPPDATA 'gate\bin'
$dest = Join-Path $installDir 'gate.exe'

if ([string]::IsNullOrEmpty($Exe)) {
    $built = Join-Path $repoRoot 'target\release\gate.exe'
    if ($PSCmdlet.ShouldProcess($repoRoot, 'cargo build --release -p gate')) {
        $buildId = Get-BuildId -Root $repoRoot
        if ($null -eq $buildId) {
            Write-InstallError 'install: git could not say which commit this is or what changed in it; building a gate whose version ends +dev'
        }
        if ((Invoke-ReleaseBuild -Root $repoRoot -BuildId $buildId) -ne 0) {
            Write-InstallError 'install: cargo build --release failed; see .tmp/install-build.log'
            exit 1
        }
        if (-not (Test-Path -LiteralPath $built)) {
            Write-InstallError "install: the build left no executable at '$built'"
            exit 1
        }
    }
    elseif (-not $WhatIfPreference) {
        Write-InstallError 'install: build declined; nothing installed'
        exit 1
    }
}
else {
    try {
        $built = (Resolve-Path -LiteralPath $Exe -ErrorAction Stop).ProviderPath
    }
    catch {
        Write-InstallError "install: no executable at '$Exe'"
        exit 1
    }
}

$newVersion = $null
if (Test-Path -LiteralPath $built) { $newVersion = Get-GateVersion $built }

$same = $false
if ((Test-Path -LiteralPath $dest) -and ($null -ne $newVersion)) {
    $same = (Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash -eq
        (Get-FileHash -LiteralPath $built -Algorithm SHA256).Hash
}

if (Test-Path -LiteralPath $installDir) {
    Remove-AsideCopy -Dir $installDir -WhatIf:$WhatIfPreference
}

if ($same) {
    Write-Output "install: gate $newVersion already installed at $dest"
}
elseif ($PSCmdlet.ShouldProcess($dest, 'Install')) {
    if ($null -eq $newVersion) {
        Write-InstallError "install: could not read the version of $built"
        exit 1
    }
    $oldVersion = if (Test-Path -LiteralPath $dest) { Get-GateVersion $dest } else { $null }
    try {
        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    }
    catch {
        Write-InstallError "install: could not create ${installDir}: $($_.Exception.Message)"
        exit 1
    }

    # The new exe is copied beside the installed one first, so a copy that fails never touches the installed exe, and
    # a name gate.exe never points at nothing.
    $temp = $dest + '.' + $PID + '.tmp'
    try {
        Copy-Item -LiteralPath $built -Destination $temp -Force -ErrorAction Stop
    }
    catch {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        Write-InstallError "install: could not install ${dest}: $($_.Exception.Message)"
        exit 1
    }

    $aside = $null
    if (Test-Path -LiteralPath $dest) {
        $aside = $dest + '.' + (Get-Date -Format 'yyyyMMddHHmmss') + '.' + $PID + '.old'
        try {
            Move-Item -LiteralPath $dest -Destination $aside -Force -ErrorAction Stop
        }
        catch {
            Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
            Write-InstallError "install: could not install ${dest}: $($_.Exception.Message)"
            exit 1
        }
    }

    try {
        Move-Item -LiteralPath $temp -Destination $dest -Force -ErrorAction Stop
    }
    catch {
        $message = $_.Exception.Message
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        if ($null -eq $aside) {
            Write-InstallError "install: could not install ${dest}: $message"
            exit 1
        }
        try {
            Move-Item -LiteralPath $aside -Destination $dest -Force -ErrorAction Stop
            Write-InstallError "install: could not install ${dest}: $message; the previous gate.exe is back in place"
        }
        catch {
            Write-InstallError "install: could not install ${dest}: $message; the previous gate.exe is at $aside"
        }
        exit 1
    }

    if ($null -ne $oldVersion) { Write-Output "install: replaced gate $oldVersion" }
    Write-Output "install: installed gate $newVersion at $dest"
}

# The remote.ps1 bridge script an earlier install put beside gate.exe, which no gate starts any more. One that cannot be
# removed, as when a pwsh still holds it, is said once and left; the install goes on.
$oldBridge = Join-Path $installDir 'remote-bridge.ps1'
if ((Test-Path -LiteralPath $oldBridge -PathType Leaf) -and $PSCmdlet.ShouldProcess($oldBridge, 'Delete')) {
    try {
        Remove-Item -LiteralPath $oldBridge -Force -ErrorAction Stop
    }
    catch {
        Write-InstallError "install: cannot remove the old remote-bridge.ps1 ($($_.Exception.Message)); it is unused"
    }
}

Add-UserPath -Folder $installDir -Key $EnvironmentKey -WhatIf:$WhatIfPreference
exit 0
