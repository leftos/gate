#Requires -Version 7.3
# Formats disk -DiskNumber as a ReFS Dev Drive labelled builds at -GateRoot's drive letter, for the gate's work root,
# erasing the disk. It formats only a disk that holds nothing: a RAW disk, or one whose volumes hold only
# System Volume Information and $RECYCLE.BIN. Any other disk is refused, naming its volumes and their used bytes.
# Before it erases anything it asks for FORMAT to be typed. When Dev Drive formatting fails it tries plain ReFS.
#   pwsh -File dev-drive.ps1 -DiskNumber <n> [-GateRoot D:\gate-remote] [-DryRun]
# Exits 0 when done or already done, 1 on a failure, a disk that holds data or no FORMAT, 2 on a bad argument.
[CmdletBinding()]
param(
    [int]$DiskNumber,
    [string]$GateRoot = 'D:\gate-remote',
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$emptyNames = @('System Volume Information', '$RECYCLE.BIN')

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("dev-drive: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    if ($DryRun) {
        Write-Output "dev-drive: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "dev-drive: $What"
}

# What the disk's partitions hold besides the empty-volume names, one line per volume or partition; none for an empty disk.
function Get-DiskContent([object[]]$Partitions) {
    foreach ($partition in $Partitions) {
        if ("$($partition.Type)" -eq 'Reserved') { continue }
        $letter = "$($partition.DriveLetter)"
        if ($letter -notmatch '^[A-Za-z]$') {
            "partition $($partition.PartitionNumber) (no drive letter to look inside)"
            continue
        }
        $volume = Get-Volume -DriveLetter $letter
        $items = @(Get-ChildItem -LiteralPath "${letter}:\" -Force | Where-Object { $_.Name -notin $emptyNames })
        if ($items) { "${letter}: ($($volume.FileSystemLabel)) $($volume.Size - $volume.SizeRemaining) bytes used" }
    }
}

if (-not $PSBoundParameters.ContainsKey('DiskNumber')) { Exit-Step 2 '-DiskNumber is required: the empty disk to format, as Get-Disk numbers it' }
if ($GateRoot -notmatch '^[A-Za-z]:\\') { Exit-Step 2 "-GateRoot '$GateRoot' is not a local path like D:\folder" }
$letter = $GateRoot.Substring(0, 1).ToUpperInvariant()
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

if (-not $DryRun -and [Console]::IsInputRedirected) {
    Exit-Step 2 'dev-drive asks you to type FORMAT; run it at the console or over ssh -t'
}
$disk = Get-Disk -Number $DiskNumber -ErrorAction SilentlyContinue
if (-not $disk) { Exit-Step 1 "there is no disk $DiskNumber; list the disks with Get-Disk" }
if ($disk.IsSystem -or $disk.IsBoot) { Exit-Step 1 "disk $DiskNumber holds Windows; give the number of an empty disk" }
$raw = "$($disk.PartitionStyle)" -eq 'RAW'
$partitions = if ($raw) { @() } else { @(Get-Partition -DiskNumber $DiskNumber) }
$own = @($partitions | Where-Object { "$($_.DriveLetter)" -eq $letter })
if ($own) {
    $volume = Get-Volume -DriveLetter $letter
    if ("$($volume.FileSystemType)" -eq 'ReFS' -and "$($volume.FileSystemLabel)" -eq 'builds') {
        Write-Output 'dev-drive: already done'
        exit 0
    }
}
$contents = @(Get-DiskContent $partitions)
if ($contents) {
    Exit-Step 1 "disk $DiskNumber holds data, so it is not formatted: $($contents -join '; '); give an empty disk or empty this one first"
}
$taken = Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue
if ($taken -and -not $own) { Exit-Step 1 "${letter}: is in use by another volume; free the letter or give a -GateRoot on another drive" }

$name = "disk $DiskNumber ($($disk.FriendlyName))"
if (-not $DryRun) {
    Write-Output "this formats disk $DiskNumber; type FORMAT to go on"
    if ("$(Read-Host)" -cne 'FORMAT') { Exit-Step 1 "FORMAT was not typed; $name is left as it was" }
}
if (-not $raw) { Invoke-Change "erase $name" { Clear-Disk -Number $DiskNumber -RemoveData -RemoveOEM -Confirm:$false } }
Invoke-Change "initialize $name as GPT with one partition at ${letter}:" {
    Initialize-Disk -Number $DiskNumber -PartitionStyle GPT
    New-Partition -DiskNumber $DiskNumber -UseMaximumSize -DriveLetter $letter
}
if ($DryRun) {
    Write-Output "dev-drive: would format ${letter}: as a ReFS Dev Drive labelled builds, or plain ReFS when that fails"
    exit 0
}
try {
    Format-Volume -DriveLetter $letter -DevDrive -FileSystem ReFS -NewFileSystemLabel builds -Confirm:$false | Out-Null
    Write-Output "dev-drive: formatted ${letter}: as a ReFS Dev Drive labelled builds"
}
catch {
    Write-Output "dev-drive: Dev Drive formatting failed ($($_.Exception.Message)); trying plain ReFS"
    try { Format-Volume -DriveLetter $letter -FileSystem ReFS -NewFileSystemLabel builds -Confirm:$false | Out-Null }
    catch {
        Exit-Step 1 ("formatting ${letter}: as ReFS failed: $($_.Exception.Message)`n" +
            "hint: format it by hand as NTFS: Format-Volume -DriveLetter $letter -FileSystem NTFS -NewFileSystemLabel builds")
    }
    Write-Output "dev-drive: formatted ${letter}: as plain ReFS labelled builds"
}
exit 0
