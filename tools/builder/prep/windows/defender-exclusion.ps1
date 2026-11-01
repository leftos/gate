#Requires -Version 7.3
# Excludes the gate's work root (-GateRoot) and C:\build-box from Defender scans, then reads the exclusions back.
#   pwsh -File defender-exclusion.ps1 [-GateRoot D:\gate-remote] [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [string]$GateRoot = 'D:\gate-remote',
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("defender-exclusion: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

if ($GateRoot -notmatch '^[A-Za-z]:\\[^''"$;]+$') { Exit-Step 2 "-GateRoot '$GateRoot' is not a local path like D:\folder without quotes, `$ or ;" }
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$paths = @($GateRoot, 'C:\build-box')
$missing = @($paths | Where-Object { $_ -notin @((Get-MpPreference).ExclusionPath) })
if (-not $missing) {
    Write-Output 'defender-exclusion: already done'
    exit 0
}
if ($DryRun) {
    Write-Output "defender-exclusion: would exclude $($missing -join ', ') from Defender scans"
    exit 0
}
Add-MpPreference -ExclusionPath $missing
$still = @($missing | Where-Object { $_ -notin @((Get-MpPreference).ExclusionPath) })
if ($still) { Exit-Step 1 "Defender does not list $($still -join ', ') as excluded after Add-MpPreference" }
Write-Output "defender-exclusion: excluded $($missing -join ', ') from Defender scans"
exit 0
