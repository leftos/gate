#Requires -Version 7.2

<#
.SYNOPSIS
    Builds gate.exe and its desktop helper in release and installs them into %LOCALAPPDATA%\gate\bin, putting that
    folder on the user's PATH.

.DESCRIPTION
    A running gate holds its own executable open, so a rebuild into target\release cannot replace an installed copy,
    and installed gates never run from target\, which build cleaners empty. This script builds gate.exe and
    gate-desktop-helper.exe in one release build through the repo gate, so both name the same build, and refuses a
    helper built with its test seams. It installs the helper first, then gate.exe, each the same way: it copies the
    built file beside the installed one under a temporary name, moves the installed file aside to
    <name>.<stamp>.<pid>.old, which works while a gate runs it, and renames the temporary copy into place, so the name
    never points at nothing; a file whose installed bytes are the same already is kept as it is. Once the install is
    confirmed, it holds push.lock in the install folder open, unshared, as gate push does, to its end, and stops with
    nothing changed while a push holds it or when push.lock is a link, a second name of another file, a folder or a
    junction; whether the same files are installed already is decided under the lock. A helper that cannot be placed
    is replaced by the previous helper again and gate.exe is not touched. An installed gate.exe that does not answer
    --version, or that cannot be placed, is replaced by the previous gate.exe again, or removed when there was none,
    the previous helper is put back the same way, and the install fails. Once the installed gate.exe answers, it
    deletes the aside copies of both files no process holds any more, its own kept, and leaves the ones a gate still
    runs for a later install; an install that ends with no gate.exe that answers keeps every one. It then adds
    %LOCALAPPDATA%\gate\bin to the user's PATH when it is missing, by reading and writing the raw Path value of the
    user environment registry key, which keeps that value's kind and its unexpanded %NAME% entries, and tells the
    running windows the environment changed. Once the installed gate.exe answers, it records the commit and build id the
    gate was built from in source.json beside it, or removes the record an earlier install left when this install's gate
    was built from no commit, and it never touches the state root's files. Run with -WhatIf to list every step without
    taking it.

.PARAMETER Exe
    Installs this prebuilt executable instead of building one; the build is skipped. Given only with -HelperExe. For
    tests only.

.PARAMETER HelperExe
    Installs this prebuilt desktop helper beside the -Exe executable. Given only with -Exe. For tests only.

.PARAMETER Source
    Records the folder's commit and build id in source.json, as a build in that folder would, instead of those of a
    build this install makes. Given only with -Exe. For tests only.

.PARAMETER EnvironmentKey
    The user environment registry key the PATH entry is read from and written to, HKCU:\Environment by default. For
    tests only: the environment change is broadcast only for the real key.

.PARAMETER PrintBuild
    Prints the arguments the release build gives cargo, on one line, and ends without building or installing. For
    tests only.

.EXAMPLE
    pwsh -NoProfile -File tools/install.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$Exe,
    [string]$HelperExe,
    [string]$Source,
    [string]$EnvironmentKey = 'HKCU:\Environment',
    [switch]$PrintBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# A native command's status is the installer's to read: a caller's $true must not turn git's `diff --quiet` exit 1, how
# it says the tree differs from the commit, into a throw.
$PSNativeCommandUseErrorActionPreference = $false

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

# The first 8 hex of the SHA-256 of what `git diff <Commit> --binary` writes in $Root, hashed as the bytes git writes
# them: the changed bytes of the tree against the commit already read, so the dirty part describes that commit and no
# other; $null when git cannot be run or fails.
function Get-DiffHash {
    param([string]$Root, [string]$Commit)

    $start = [Diagnostics.ProcessStartInfo]::new('git')
    foreach ($arg in '--no-optional-locks', '-C', $Root, 'diff', '--binary', $Commit, '--') { $start.ArgumentList.Add($arg) }
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

# The tree the gate this install places is built from, from one read of HEAD, so the commit source.json records and the
# build id gate --version names always describe the same commit: Commit, the full SHA `git rev-parse HEAD` gives, and
# BuildId, that same sha's `rev-parse --short=12` form with `-dirty.<hash>` added when the tree differs from it, <hash>
# the first 8 hex of the SHA-256 of `git diff <sha> --binary`, so a build of a dirty tree is told apart from a build of
# its commit and from a build of other changes to it. The dirty check and the hash both name the sha already read, never
# HEAD again. Git runs with --no-optional-locks, so reading the tree never takes the index lock. $null when git cannot be
# run here or any of its calls fails, which builds a gate that names no commit and so says `+dev`.
function Get-BuildSource {
    param([string]$Root)

    try {
        $head = & git --no-optional-locks -C $Root rev-parse HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace([string]$head)) { return $null }
        $commit = ([string]$head).Trim()
        $short = & git --no-optional-locks -C $Root rev-parse --short=12 $commit 2>$null
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace([string]$short)) { return $null }
        & git --no-optional-locks -C $Root diff --quiet $commit -- 2>$null
        $changed = $LASTEXITCODE
    }
    catch {
        return $null
    }
    $id = ([string]$short).Trim()
    if ($changed -eq 0) { return @{ Commit = $commit; BuildId = $id } }
    if ($changed -ne 1) { return $null }
    $hash = Get-DiffHash -Root $Root -Commit $commit
    if ($null -eq $hash) { return $null }
    return @{ Commit = $commit; BuildId = "$id-dirty.$hash" }
}

# Whether $Root is the root of a git work tree, as -Source must name: `rev-parse --is-inside-work-tree` says yes from
# anywhere in a tree, and `--show-prefix` says how far below its root the folder is, which is nothing at all only at the
# root itself. Git resolves the folder it was given, so a junction to a root and a path with a trailing separator are
# accepted as the root they name.
function Test-WorkTreeRoot {
    param([string]$Root)

    try {
        $said = @(& git --no-optional-locks -C $Root rev-parse --is-inside-work-tree --show-prefix 2>$null)
        if ($LASTEXITCODE -ne 0 -or $said.Count -ne 2) { return $false }
    }
    catch {
        return $false
    }
    return ([string]$said[0] -eq 'true') -and [string]::IsNullOrEmpty([string]$said[1])
}

# The arguments the release build gives cargo: gate.exe and its desktop helper in one call, with their default features
# only, so the helper is never built with its test seams.
function Get-BuildArguments {
    return @('build', '--release', '-p', 'gate', '-p', 'gate-desktop')
}

# Runs the release build of gate.exe and its desktop helper through the repo gate from the repo root, where the gate's
# -Log lands, and returns its status. Cargo gets Get-BuildArguments and nothing else.
# The build runs with -Local, so it is never routed to a builder. $BuildId reaches cargo as GATE_BUILD_ID for this one
# build, so the exe it writes names the commit it was made from; an empty one removes GATE_BUILD_ID for the build, so a
# value the caller set never leaks in, and builds a gate that names no commit (`+dev`).
function Invoke-ReleaseBuild {
    param([string]$Root, [string]$BuildId)

    $gate = Join-Path $Root 'tools\gate.ps1'
    $cargoArguments = Get-BuildArguments
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
            & pwsh -NoProfile -File $gate -Log '.tmp/install-build.log' -TimeoutSeconds 900 -Slot heavy -Local `
                -Reason 'the installer builds the gate on this PC' -- `
                cargo @cargoArguments | Out-Host
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

# Deletes every aside copy of gate.exe and of the desktop helper in $Dir that no process holds any more, but the ones at
# $Keep, this install's own. One a gate still runs is left for a later install, and left silently: its delete fails,
# and the probe shows the file is still held.
function Remove-AsideCopy {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Dir, [string[]]$Keep)

    $kept = @($Keep | Where-Object { $_ } | ForEach-Object { [IO.Path]::GetFileName($_) })
    $asides = foreach ($pattern in 'gate.exe.*.old', 'gate-desktop-helper.exe.*.old') {
        Get-ChildItem -LiteralPath $Dir -Filter $pattern -File
    }
    foreach ($file in $asides) {
        if ($kept -contains $file.Name) { continue }
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

# Removes the record at $Path, when one is there: a gate built from no commit names no source, a prebuilt exe names
# none, and a gate that does not answer --version cannot be trusted to name one, which -NotAnswering says. Returns
# $false when it could not be removed, with the line why on standard error.
function Remove-SourceJson {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Path, [switch]$NotAnswering)

    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    if (-not $PSCmdlet.ShouldProcess($Path, 'Delete')) { return $true }
    try {
        # A file delete, never Remove-Item: a folder at the record's name fails here, where Remove-Item would ask
        # whether to remove its children.
        [IO.File]::Delete($Path)
    }
    catch {
        Write-InstallError "install: could not remove ${Path}: $($_.Exception.Message)"
        return $false
    }
    if ($NotAnswering) {
        Write-InstallError "install: the installed gate does not answer --version; removed $Path"
    }
    return $true
}

# Removes the source.json temps in $Dir that a run killed between its write and its rename left behind, so no earlier
# run's record is read back instead of this one's and the folder never holds more than the one this run writes. Each is
# tried on its own: one that cannot be removed is noted at verbose and the rest still go.
function Remove-LeftoverSourceTemp {
    param([string]$Dir)

    try {
        $leftovers = [IO.Directory]::GetFiles($Dir, 'source.json.*.tmp')
    }
    catch {
        Write-Verbose "The leftover record temps in ${Dir} could not be listed: $($_.Exception.Message)"
        return
    }
    foreach ($leftover in $leftovers) {
        try {
            [IO.File]::Delete($leftover)
        }
        catch {
            Write-Verbose "A leftover record temp, ${leftover}, could not be removed: $($_.Exception.Message)"
        }
    }
}

# Records where the installed gate was built from beside it, in source.json, the one file gate push reads to know which
# commit to send to a macOS builder: one JSON object naming the repo folder the build ran in, the full commit, the exact
# build id cargo was given and whether tracked files were changed, which the id's `-dirty.` says. It is written as
# source.json.<pid>.tmp and renamed over source.json, so a reader never sees half a record, and a temp a killed run left
# behind goes before a new one is written. An empty $Commit or $BuildId removes the record instead, as
# Remove-SourceJson does. Returns $false when the record could not be written or removed, with the line why on standard
# error; the caller decides when to stop.
function Set-SourceJson {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Dir, [string]$Root, [string]$Commit, [string]$BuildId, [switch]$NotAnswering)

    $path = Join-Path $Dir 'source.json'
    if ([string]::IsNullOrEmpty($Commit) -or [string]::IsNullOrEmpty($BuildId)) {
        return Remove-SourceJson -Path $path -NotAnswering:$NotAnswering -WhatIf:$WhatIfPreference
    }
    if (-not $PSCmdlet.ShouldProcess($path, 'Write')) { return $true }
    Remove-LeftoverSourceTemp -Dir $Dir
    $record = [ordered]@{
        repoRoot = $Root
        commit = $Commit
        buildId = $BuildId
        dirty = $BuildId.Contains('-dirty.', [StringComparison]::Ordinal)
    }
    $temp = Join-Path $Dir "source.json.$PID.tmp"
    try {
        [IO.File]::WriteAllText($temp, ($record | ConvertTo-Json -Compress) + "`n", [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($temp, $path, $true)
    }
    catch {
        Write-InstallError "install: could not write ${path}: $($_.Exception.Message)"
        if (Test-Path -LiteralPath $temp) {
            try {
                [IO.File]::Delete($temp)
            }
            catch {
                Write-InstallError "install: could not remove ${temp}: $($_.Exception.Message)"
            }
        }
        return $false
    }
    return $true
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

# Whether the desktop helper at $Path was built with its test seams: its bytes hold the ASCII text of the seams'
# variable names, which only a build with the `test-seams` feature compiles in.
function Test-HelperSeams {
    param([string]$Path)

    $text = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($Path))
    return $text.Contains('GATE_DESKTOP_TEST_', [StringComparison]::Ordinal)
}

# Writes $Built beside $Dest as $Dest.<pid>.tmp, afresh and through a handle of the install's own, so a copy that fails
# never touches the installed file, then moves an existing $Dest aside to $Dest.<stamp>.<pid>.old. Returns the swap:
# Dest, Temp, Own, the copy's file id, which its move into place keeps and which tells the install's own file from one
# another process placed, and Aside, $null when nothing was at $Dest. Throws when a step fails, with the copy removed
# and $Dest as it was.
function New-Swap {
    param([string]$Dest, [string]$Built)

    $temp = $Dest + '.' + $PID + '.tmp'
    Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
    $aside = $null
    try {
        $source = [IO.File]::OpenRead($Built)
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
        if (Test-Path -LiteralPath $Dest) {
            $aside = $Dest + '.' + (Get-Date -Format 'yyyyMMddHHmmss') + '.' + $PID + '.old'
            Move-Item -LiteralPath $Dest -Destination $aside -Force -ErrorAction Stop
        }
    }
    catch {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        throw
    }
    return @{ Dest = $Dest; Temp = $temp; Own = $own; Aside = $aside }
}

# Renames the copy of $Swap into place, never by force: with the installed file moved aside its name is missing, so a
# file that appeared there meanwhile is another process's and makes the move fail. Throws then, with the copy removed.
function Move-SwapIntoPlace {
    param([hashtable]$Swap)

    try {
        Move-Item -LiteralPath $Swap.Temp -Destination $Swap.Dest -ErrorAction Stop
    }
    catch {
        Remove-Item -LiteralPath $Swap.Temp -Force -ErrorAction SilentlyContinue
        throw
    }
}

# Puts the desktop helper back as it was before $Swap, this install's swap of it, when there was one: moves the previous
# helper back from its aside copy, or removes the install's copy when there was no earlier helper, but only while the
# helper's name is missing or holds the install's own copy, and over a missing one never by force; a helper another
# process placed is left as it is. Silent when the helper is back as it was; says what it left otherwise. Returns whether
# a previous helper was moved back into place.
function Undo-HelperSwap {
    param([hashtable]$Swap)

    if ($null -eq $Swap) { return $false }
    $dest = $Swap.Dest
    $aside = $Swap.Aside
    $left = if ($null -eq $aside) { '' } else { "; the previous desktop helper is at $aside" }
    $state = Get-InstalledState -Dest $dest -Own $Swap.Own
    if ($state -eq 'foreign') {
        Write-InstallError "install: another desktop helper was placed at $dest meanwhile and was left as it is$left"
        return $false
    }
    try {
        if ($null -eq $aside) {
            if ($state -eq 'mine') { Remove-Item -LiteralPath $dest -Force -ErrorAction Stop }
            return $false
        }
        if ($state -eq 'mine') {
            Move-Item -LiteralPath $aside -Destination $dest -Force -ErrorAction Stop
        }
        else {
            Move-Item -LiteralPath $aside -Destination $dest -ErrorAction Stop
        }
        return $true
    }
    catch {
        Write-InstallError "install: the desktop helper at $dest could not be put back as it was: $($_.Exception.Message)$left"
        return $false
    }
}

# Undoes an install that failed for $Reason, a whole `install: ` text, and exits 1: puts the desktop helper back as it
# was before $Helper, this install's swap of it ($null when the helper was kept), then puts the previous gate.exe at
# $Aside back at $Dest, or removes the install's copy there when there was no earlier gate.exe, but only while $Dest is
# missing or the install's own copy, the file whose id is $Own, and over a missing one never by force; a gate.exe another
# process placed is left as it is. Says which, and says when a previous helper was put back beside that other gate.exe.
# Every gate.exe aside copy is kept.
function Restore-PreviousGate {
    param([string]$Reason, [string]$Dest, [string]$Aside, [string]$Own, [hashtable]$Helper)

    $helperBack = Undo-HelperSwap -Swap $Helper
    $state = Get-InstalledState -Dest $Dest -Own $Own
    $placed = "${Reason}; another gate.exe was placed meanwhile and was left as it is"
    if (-not [string]::IsNullOrEmpty($Aside)) { $placed += ", the previous gate.exe is at $Aside" }
    $mismatch = 'install: the previous desktop helper was put back beside a gate.exe another process placed; they may not match'
    if ($state -eq 'foreign') {
        Write-InstallError $placed
        if ($helperBack) { Write-InstallError $mismatch }
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
            if ($helperBack) { Write-InstallError $mismatch }
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

if ([string]::IsNullOrEmpty($Exe) -ne [string]::IsNullOrEmpty($HelperExe)) {
    Write-InstallError 'install: -Exe and -HelperExe are given together or not at all'
    exit 1
}

if ((-not [string]::IsNullOrEmpty($Source)) -and [string]::IsNullOrEmpty($Exe)) {
    Write-InstallError 'install: -Source is given only with -Exe'
    exit 1
}

if ($PrintBuild) {
    Write-Output ((Get-BuildArguments) -join ' ')
    exit 0
}

if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
    Write-InstallError 'install: LOCALAPPDATA is not set'
    exit 1
}

$installDir = Join-Path $env:LOCALAPPDATA 'gate\bin'
$dest = Join-Path $installDir 'gate.exe'
$helperDest = Join-Path $installDir 'gate-desktop-helper.exe'

if ([string]::IsNullOrEmpty($Exe)) {
    $built = Join-Path $repoRoot 'target\release\gate.exe'
    $builtHelper = Join-Path $repoRoot 'target\release\gate-desktop-helper.exe'
    # Read before the preview decides, so a -WhatIf run lists the source.json a real run would write.
    $sourceRoot = $repoRoot
    $buildSource = Get-BuildSource -Root $repoRoot
    $buildId = if ($null -eq $buildSource) { $null } else { $buildSource.BuildId }
    if ($PSCmdlet.ShouldProcess($repoRoot, "cargo $((Get-BuildArguments) -join ' ')")) {
        if ($null -eq $buildId) {
            Write-InstallError 'install: git could not say which commit this is or what changed in it; building a gate whose version ends +dev'
        }
        if ((Invoke-ReleaseBuild -Root $repoRoot -BuildId $buildId) -ne 0) {
            Write-InstallError 'install: cargo build --release failed; see .tmp/install-build.log'
            exit 1
        }
        foreach ($file in $built, $builtHelper) {
            if (-not (Test-Path -LiteralPath $file)) {
                Write-InstallError "install: the build left no executable at '$file'"
                exit 1
            }
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
    try {
        $builtHelper = (Resolve-Path -LiteralPath $HelperExe -ErrorAction Stop).ProviderPath
    }
    catch {
        Write-InstallError "install: no desktop helper at '$HelperExe'"
        exit 1
    }
    # A prebuilt exe names no commit, so an -Exe install records no source and removes the record an earlier install
    # left. -Source names the folder a test build would have run in, whose commit and build id are recorded instead. It
    # must be the root of a git work tree, since only there is its commit the tree's own: a folder that cannot be
    # resolved, that git calls something other than a tree's top, or whose HEAD names no commit, names no source, and is
    # refused rather than left to remove the record.
    if ([string]::IsNullOrEmpty($Source)) {
        $sourceRoot = $null
        $buildSource = $null
    }
    else {
        $sourceRoot = $null
        try {
            $resolved = (Resolve-Path -LiteralPath $Source -ErrorAction Stop).ProviderPath
            if (Test-WorkTreeRoot -Root $resolved) { $sourceRoot = $resolved }
        }
        catch {
            Write-Verbose "The -Source folder $Source could not be resolved: $($_.Exception.Message)"
        }
        if ($null -eq $sourceRoot) {
            Write-InstallError "install: -Source $Source is not a git work tree"
            exit 1
        }
        $buildSource = Get-BuildSource -Root $sourceRoot
        if ($null -eq $buildSource) {
            Write-InstallError "install: -Source $sourceRoot has no commits yet"
            exit 1
        }
    }
}

# A helper built with its test seams is never installed. A -WhatIf run that would build checks none: nothing is built in
# a preview, so whatever target\release holds is not what a real run would install.
$previewBuild = $WhatIfPreference -and [string]::IsNullOrEmpty($HelperExe)
if ((-not $previewBuild) -and (Test-Path -LiteralPath $builtHelper)) {
    try {
        $seams = Test-HelperSeams -Path $builtHelper
    }
    catch {
        Write-InstallError "install: could not read the desktop helper at '${builtHelper}': $($_.Exception.Message)"
        exit 1
    }
    if ($seams) {
        Write-InstallError "install: the desktop helper at '$builtHelper' was built with its test seams; build it without features"
        exit 1
    }
}

$newVersion = $null
if (Test-Path -LiteralPath $built) { $newVersion = Get-GateVersion $built }

$lock = $null
$same = $false
$sameGate = $false
$sameHelper = $false
$previewSame = $false
# Whether the installed gate.exe answered --version on this run, read once on each path and shared with the record step
# below, which never asks the gate again.
$answered = $false
$withHelper = " with its desktop helper at $helperDest"
if ($WhatIfPreference -and ($null -ne $newVersion)) {
    try {
        $previewSame = (Test-SameExe -Dest $dest -Built $built) -and (Test-SameExe -Dest $helperDest -Built $builtHelper)
    }
    catch {
        # A hash that cannot be read previews the install a real run would then try.
        Write-Verbose "Whether $dest and $helperDest are the same files could not be read: $($_.Exception.Message)"
    }
}
try {
    if ($previewSame) {
        # The preview of an install of the files already in place, decided without the lock, since it changes nothing
        # but the aside copies a real run deletes once the installed gate answers.
        Write-Output "install: gate $newVersion already installed at $dest$withHelper"
        $answered = $null -ne (Get-GateVersion $dest)
        if ($answered) { Remove-AsideCopy -Dir $installDir -WhatIf }
    }
    else {
        # Both installs are asked for, so -WhatIf lists the helper's and gate.exe's.
        $installHelper = $PSCmdlet.ShouldProcess($helperDest, 'Install')
        $installGate = $PSCmdlet.ShouldProcess($dest, 'Install')
        if ($installHelper -and $installGate) {
            if ($null -eq $newVersion) {
                Write-InstallError "install: could not read the version of $built"
                exit 1
            }
            # Every look at the install folder from here on, the same-file checks included, is made under the lock.
            $lock = Lock-InstallFolder -Dir $installDir
            $sameGate = Test-SameExe -Dest $dest -Built $built
            try {
                $sameHelper = Test-SameExe -Dest $helperDest -Built $builtHelper
            }
            catch {
                Write-InstallError "install: could not read the desktop helper at '${helperDest}': $($_.Exception.Message)"
                exit 1
            }
            $same = $sameGate -and $sameHelper
        }
        elseif ($WhatIfPreference -and (Test-Path -LiteralPath $installDir)) {
            # The aside copies a verified install would delete, listed.
            Remove-AsideCopy -Dir $installDir -WhatIf
        }
    }

    # Declined or -WhatIf, the lock is not held and nothing is installed.
    if ($same) {
        Write-Output "install: gate $newVersion already installed at $dest$withHelper"
        $answered = $null -ne (Get-GateVersion $dest)
        if ($answered) { Remove-AsideCopy -Dir $installDir }
    }
    elseif ($null -ne $lock) {
        # The helper is placed first; one that cannot be is put back as it was, and gate.exe is not touched.
        $helperSwap = $null
        $helperAside = $null
        if (-not $sameHelper) {
            try {
                $helperSwap = New-Swap -Dest $helperDest -Built $builtHelper
            }
            catch {
                Write-InstallError "install: could not install ${helperDest}: $($_.Exception.Message)"
                exit 1
            }
            $helperAside = $helperSwap.Aside
            try {
                Move-SwapIntoPlace -Swap $helperSwap
            }
            catch {
                Write-InstallError "install: could not install ${helperDest}: $($_.Exception.Message)"
                $null = Undo-HelperSwap -Swap $helperSwap
                exit 1
            }
        }

        # Then gate.exe; any way its install fails puts the previous helper back too.
        $oldVersion = $null
        $gateAside = $null
        if (-not $sameGate) {
            $oldVersion = if (Test-Path -LiteralPath $dest) { Get-GateVersion $dest } else { $null }
            try {
                $swap = New-Swap -Dest $dest -Built $built
            }
            catch {
                Write-InstallError "install: could not install ${dest}: $($_.Exception.Message)"
                $null = Undo-HelperSwap -Swap $helperSwap
                exit 1
            }
            $gateAside = $swap.Aside
            try {
                Move-SwapIntoPlace -Swap $swap
            }
            catch {
                $reason = "install: could not install ${dest}: $($_.Exception.Message)"
                if ($null -eq $gateAside) {
                    Write-InstallError $reason
                    $null = Undo-HelperSwap -Swap $helperSwap
                    exit 1
                }
                Restore-PreviousGate -Reason $reason -Dest $dest -Aside $gateAside -Own $swap.Own -Helper $helperSwap
            }

            if ($null -eq (Get-GateVersion $dest)) {
                Restore-PreviousGate -Reason 'install: the installed gate.exe does not run' -Dest $dest -Aside $gateAside `
                    -Own $swap.Own -Helper $helperSwap
            }
        }
        # A gate.exe kept as it was is asked too: the aside copies go only once the installed gate answers, and a
        # gate.exe placed just above answered already, or the previous one would have been put back.
        $answered = if ($sameGate) { $null -ne (Get-GateVersion $dest) } else { $true }
        if ($answered) {
            Remove-AsideCopy -Dir $installDir -Keep $gateAside, $helperAside
        }
        if ($sameGate) {
            Write-Output "install: gate $newVersion already installed at $dest; installed its desktop helper at $helperDest"
        }
        else {
            if ($null -ne $oldVersion) { Write-Output "install: replaced gate $oldVersion" }
            Write-Output "install: installed gate $newVersion at $dest$withHelper"
        }
    }

    # The record gate push reads to know which commit the installed gate was built from, beside it, written only for an
    # install that happened under push.lock, or for the files already installed under it, and only while the installed
    # gate answers --version: a gate that does not answer, an -Exe install with no source and a `+dev` build all name no
    # source, so any record an earlier install left goes. An install declined with -Confirm keeps the record as it was,
    # and a -WhatIf run lists the step and takes none. The gate is never asked again here: $answered is the answer the
    # install's own path already read.
    $recordFailed = $false
    if ($WhatIfPreference -or ($null -ne $lock)) {
        # A -WhatIf run names the action a real run would take: for the files already installed, the one the installed
        # gate's answer decides; otherwise a write, since a real run reaches this step only with a gate that answered (an
        # -Exe that does not answer stops it first, and a gate.exe placed that does not answer is put back). A real run
        # uses the answer its own path already read.
        $answering = if ($WhatIfPreference -and (-not $previewSame)) { $true } else { $answered }
        $keepSource = $answering -and ($null -ne $buildSource)
        $sourceCommit = if ($keepSource) { $buildSource.Commit } else { $null }
        $sourceBuildId = if ($keepSource) { $buildSource.BuildId } else { $null }
        $recordFailed = -not (Set-SourceJson -Dir $installDir -Root $sourceRoot -Commit $sourceCommit `
            -BuildId $sourceBuildId -NotAnswering:(-not $answering) -WhatIf:$WhatIfPreference)
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

    # A record that could not be written or removed fails the install here, after the PATH step, so a gate this run
    # installed is still on the PATH; the line why was said above.
    if ($recordFailed) { exit 1 }
}
finally {
    if ($null -ne $lock) { $lock.Dispose() }
}
exit 0
