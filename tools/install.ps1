#Requires -Version 7.2

<#
.SYNOPSIS
    Builds gate.exe in release and installs it into %LOCALAPPDATA%\gate\bin, putting that folder on the user's PATH.

.DESCRIPTION
    A running gate holds its own executable open, so a rebuild into target\release cannot replace an installed copy,
    and installed gates never run from target\, which build cleaners empty. This script builds through the repo gate,
    copies the built executable beside the installed one under a temporary name, moves an installed gate.exe aside to
    gate.exe.<stamp>.<pid>.old, which works while a gate runs it, and renames the temporary copy into place, so the
    name gate.exe never points at nothing. Once the install is confirmed, it holds push.lock in the install folder
    open, unshared, as gate push does, to its end, and stops with nothing changed while a push holds it or when
    push.lock is a link, a second name of another file, a folder or a junction; whether the same exe is installed
    already is decided under the lock. An installed copy that does not answer --version is
    replaced by the previous gate.exe again, or removed when there was none, and the install fails. Once the installed
    gate.exe answers, it deletes the aside copies no process holds any more, its own kept, and leaves the ones a gate
    still runs for a later install; an install that ends with no gate.exe that answers keeps every one. It then adds
    %LOCALAPPDATA%\gate\bin to the user's PATH when it is missing, by reading and writing the raw Path value of the
    user environment registry key, which keeps that value's kind and its unexpanded %NAME% entries, and tells the
    running windows the environment changed. It never touches the state root's files. Run with -WhatIf to list every
    step without taking it.

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

# What `gate --version` prints after `gate `, or $null when the program cannot be run, prints something else or has not
# ended within 30 s, as gate push's builder script reads it: one that has not ended is killed, its whole tree, and
# waited for up to 10 s more, so the file it ran from is let go before anything moves it.
function Get-GateVersion {
    param([string]$Path)

    $start = [Diagnostics.ProcessStartInfo]::new($Path)
    $start.ArgumentList.Add('--version')
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    try {
        $process = [Diagnostics.Process]::Start($start)
    }
    catch {
        return $null
    }
    try {
        $output = $process.StandardOutput.ReadToEndAsync()
        $null = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(30000)) {
            try {
                $process.Kill($true)
            }
            catch {
                # Whatever the kill says, an exit it raced included, the read is waited for and gives no version.
                Write-Verbose "The version read of $Path could not be killed: $($_.Exception.Message)"
            }
            $null = $process.WaitForExit(10000)
            return $null
        }
        $line = ($output.Result -split '\r?\n')[0]
    }
    finally {
        $process.Dispose()
    }
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

# Deletes every aside copy in $Dir that no process holds any more, but the one at $Keep, this install's own. One a gate
# still runs is left for a later install, and left silently: its delete fails, and the probe shows the file is still held.
function Remove-AsideCopy {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Dir, [string]$Keep)

    foreach ($file in Get-ChildItem -LiteralPath $Dir -Filter 'gate.exe.*.old' -File) {
        if ($Keep -and $file.Name -eq [IO.Path]::GetFileName($Keep)) { continue }
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

# Adds GateFile, the interop the install's file checks use, once per run, and exits 1 when it cannot be added, as in
# constrained language or with no compiler. GateFile.Id is the file id of an open handle: FILE_ID_INFO's volume serial
# number and 128-bit id within the volume, as hex, the same for a file under another name after a rename on its volume
# and new for a file written in its place. GateFile.Of is the id of the file at a path, read from a handle that only
# reads and lets others read, write and delete. GateFile.OpenLock opens push.lock unshared, made when missing, without
# following a link, and returns $null when it is a link, a second name of another file, a folder or a junction.
function Add-GateFile {
    if ('GateFile' -as [type]) { return }
    try {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class GateFile {
    const uint ReadWrite = 0xC0000000, OpenAlways = 4, NoFollow = 0x200080, Folder = 0x10, Link = 0x400;
    const int Busy = -2147024864, FileIdInfo = 18;
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetFileAttributesW(string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandle(SafeFileHandle handle, [Out] uint[] information);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandleEx(SafeFileHandle handle, int kind, [Out] byte[] information, uint size);
    static IOException Error(string text) {
        int error = Marshal.GetLastPInvokeError();
        return new IOException(text + new Win32Exception(error).Message, unchecked((int)0x80070000) | error);
    }
    public static string Id(SafeFileHandle handle) {
        var information = new byte[24];
        if (!GetFileInformationByHandleEx(handle, FileIdInfo, information, 24)) {
            throw Error("the file id could not be read: ");
        }
        return Convert.ToHexString(information);
    }
    public static string Of(string path) {
        using (var handle = File.OpenHandle(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete)) {
            return Id(handle);
        }
    }
    public static FileStream OpenLock(string path) {
        var handle = CreateFileW(path, ReadWrite, 0, IntPtr.Zero, OpenAlways, NoFollow, IntPtr.Zero);
        if (handle.IsInvalid) {
            var failure = Error("");
            handle.Dispose();
            var attributes = GetFileAttributesW(path);
            if (failure.HResult != Busy && attributes != uint.MaxValue && (attributes & (Folder | Link)) != 0) {
                return null;
            }
            throw failure;
        }
        var information = new uint[13];
        if (!GetFileInformationByHandle(handle, information)) {
            var failure = Error("");
            handle.Dispose();
            throw failure;
        }
        if ((information[0] & Link) != 0 || information[10] != 1) {
            handle.Dispose();
            return null;
        }
        return new FileStream(handle, FileAccess.ReadWrite);
    }
}
'@
    }
    catch {
        Write-InstallError "install: could not prepare the install: $($_.Exception.Message)"
        exit 1
    }
}

# Makes the install folder $Dir and takes the lock gate push holds while it installs there: push.lock, held open and
# unshared until the handle this returns is disposed, so an install and a push never change the folder at once. Its
# open never follows a link, and a push.lock that is a link, a second name of another file, a folder or a junction is
# refused; the plain file is left in place, since the open handle is the lock. Exits 1 when the file checks cannot be
# added, the folder cannot be made or the lock cannot be taken, a push holding it included.
function Lock-InstallFolder {
    param([string]$Dir)

    Add-GateFile
    try {
        New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    }
    catch {
        Write-InstallError "install: could not create ${Dir}: $($_.Exception.Message)"
        exit 1
    }
    $lockPath = Join-Path $Dir 'push.lock'
    try {
        $lock = [GateFile]::OpenLock($lockPath)
    }
    catch {
        $reason = $_.Exception.GetBaseException()
        if ($reason.HResult -eq -2147024864) {
            Write-InstallError 'install: another gate push is installing; nothing was changed'
            exit 1
        }
        Write-InstallError "install: could not lock ${Dir}: $($reason.Message)"
        exit 1
    }
    if ($null -eq $lock) {
        Write-InstallError "install: $lockPath is a link; nothing was changed"
        exit 1
    }
    return $lock
}

# Whether $Dest holds the exe at $Built, the same bytes by SHA-256.
function Test-SameExe {
    param([string]$Dest, [string]$Built)

    (Test-Path -LiteralPath $Dest) -and
        (Get-FileHash -LiteralPath $Dest -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath $Built -Algorithm SHA256).Hash
}

# What is at $Dest: 'missing', 'mine' when it is the file whose id is $Own, the install's own copy, or 'foreign'. An id
# that cannot be read is tried three times, 300 ms apart, and $Dest looked at once more 300 ms after the last try: a
# file gone at any try or by then is missing, and one still there counts as another's.
function Get-InstalledState {
    param([string]$Dest, [string]$Own)

    foreach ($try in 1..3) {
        if (-not (Test-Path -LiteralPath $Dest)) { return 'missing' }
        try {
            if ([GateFile]::Of($Dest) -eq $Own) { return 'mine' }
            return 'foreign'
        }
        catch {
            if ($_.Exception.GetBaseException() -is [IO.FileNotFoundException]) { return 'missing' }
            Write-Verbose "The file id of $Dest could not be read: $($_.Exception.Message)"
            Start-Sleep -Milliseconds 300
        }
    }
    if (Test-Path -LiteralPath $Dest) { return 'foreign' }
    return 'missing'
}

# Undoes an install that failed for $Reason, a whole `install: ` text, and exits 1: puts the previous gate.exe at $Aside
# back at $Dest, or removes the install's copy there when there was no earlier gate.exe, but only while $Dest is missing
# or the install's own copy, the file whose id is $Own, and over a missing one never by force; a gate.exe another process
# placed is left as it is. Says which. Every aside copy is kept.
function Restore-PreviousGate {
    param([string]$Reason, [string]$Dest, [string]$Aside, [string]$Own)

    $state = Get-InstalledState -Dest $Dest -Own $Own
    $placed = "${Reason}; another gate.exe was placed meanwhile and was left as it is"
    if (-not [string]::IsNullOrEmpty($Aside)) { $placed += ", the previous gate.exe is at $Aside" }
    if ($state -eq 'foreign') {
        Write-InstallError $placed
        exit 1
    }
    if ([string]::IsNullOrEmpty($Aside)) {
        if ($state -eq 'mine') {
            try {
                Remove-Item -LiteralPath $Dest -Force -ErrorAction Stop
            }
            catch {
                Write-InstallError "${Reason}; the copy at $Dest could not be removed: $($_.Exception.Message)"
                exit 1
            }
        }
        Write-InstallError "${Reason}; the copy was removed and no earlier gate.exe existed"
        exit 1
    }
    try {
        if ($state -eq 'mine') {
            Move-Item -LiteralPath $Aside -Destination $Dest -Force -ErrorAction Stop
        }
        else {
            Move-Item -LiteralPath $Aside -Destination $Dest -ErrorAction Stop
        }
        Write-InstallError "${Reason}; the previous gate.exe is back in place"
    }
    catch {
        if ($state -eq 'missing' -and (Test-Path -LiteralPath $Dest)) {
            Write-InstallError $placed
        }
        else {
            Write-InstallError "${Reason}; the previous gate.exe is at $Aside"
        }
    }
    exit 1
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

$lock = $null
$same = $false
$previewSame = $false
if ($WhatIfPreference -and ($null -ne $newVersion)) {
    try {
        $previewSame = Test-SameExe -Dest $dest -Built $built
    }
    catch {
        # A hash that cannot be read previews the install a real run would then try.
        Write-Verbose "Whether $dest is the same exe could not be read: $($_.Exception.Message)"
    }
}
try {
    if ($previewSame) {
        # The preview of an install of the exe already in place, decided without the lock, since it changes nothing but
        # the aside copies a real run deletes once the installed gate answers.
        Write-Output "install: gate $newVersion already installed at $dest"
        if ($null -ne (Get-GateVersion $dest)) { Remove-AsideCopy -Dir $installDir -WhatIf }
    }
    elseif ($PSCmdlet.ShouldProcess($dest, 'Install')) {
        if ($null -eq $newVersion) {
            Write-InstallError "install: could not read the version of $built"
            exit 1
        }
        # Every look at the install folder from here on, the same-exe check included, is made under the lock.
        $lock = Lock-InstallFolder -Dir $installDir
        $same = Test-SameExe -Dest $dest -Built $built
    }
    elseif ($WhatIfPreference -and (Test-Path -LiteralPath $installDir)) {
        # The aside copies a verified install would delete, listed.
        Remove-AsideCopy -Dir $installDir -WhatIf
    }

    # Declined or -WhatIf, the lock is not held and nothing is installed.
    if ($same) {
        Write-Output "install: gate $newVersion already installed at $dest"
        if ($null -ne (Get-GateVersion $dest)) { Remove-AsideCopy -Dir $installDir }
    }
    elseif ($null -ne $lock) {
        $oldVersion = if (Test-Path -LiteralPath $dest) { Get-GateVersion $dest } else { $null }

        # The new exe is written beside the installed one first, afresh and through a handle of the install's own, so a
        # copy that fails never touches the installed exe, and a name gate.exe never points at nothing. The copy's file
        # id, read from that handle and kept by its move into place, is what tells the install's own gate.exe from one
        # another process placed.
        $temp = $dest + '.' + $PID + '.tmp'
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        try {
            $source = [IO.File]::OpenRead($built)
            try {
                $copy = [IO.File]::Open($temp, 'CreateNew', 'Write', 'None')
                try {
                    $source.CopyTo($copy)
                    $own = [GateFile]::Id($copy.SafeFileHandle)
                }
                finally {
                    $copy.Dispose()
                }
            }
            finally {
                $source.Dispose()
            }
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

        # Never by force: with the old exe moved aside, gate.exe is missing, so a file that appeared there meanwhile is
        # another process's and makes the move fail.
        try {
            Move-Item -LiteralPath $temp -Destination $dest -ErrorAction Stop
        }
        catch {
            $message = $_.Exception.Message
            Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
            if ($null -eq $aside) {
                Write-InstallError "install: could not install ${dest}: $message"
                exit 1
            }
            Restore-PreviousGate -Reason "install: could not install ${dest}: $message" -Dest $dest -Aside $aside -Own $own
        }

        if ($null -eq (Get-GateVersion $dest)) {
            Restore-PreviousGate -Reason 'install: the installed gate.exe does not run' -Dest $dest -Aside $aside -Own $own
        }
        Remove-AsideCopy -Dir $installDir -Keep $aside
        if ($null -ne $oldVersion) { Write-Output "install: replaced gate $oldVersion" }
        Write-Output "install: installed gate $newVersion at $dest"
    }

    # The remote.ps1 bridge script an earlier install put beside gate.exe, which no gate starts any more. One that cannot
    # be removed, as when a pwsh still holds it, is said once and left; the install goes on.
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
}
finally {
    if ($null -ne $lock) { $lock.Dispose() }
}
exit 0
