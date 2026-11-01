#Requires -Version 7.3
# Turns Defender real-time scanning off, which lowers the builder's protection, and reads it back. Tamper Protection
# blocks the change; it is turned off by hand in Windows Security first.
#   pwsh -File realtime-off.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure (Tamper Protection included), 2 on a bad argument.
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tamperLine = 'Tamper Protection blocked this; turn it off in Windows Security first'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("realtime-off: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$status = Get-MpComputerStatus
if (-not $status.RealTimeProtectionEnabled) {
    Write-Output 'realtime-off: already done'
    exit 0
}
if ($status.IsTamperProtected) { Exit-Step 1 $tamperLine }
if ($DryRun) {
    Write-Output 'realtime-off: would turn Defender real-time scanning off'
    exit 0
}
Set-MpPreference -DisableRealtimeMonitoring $true
# Set-MpPreference returns before Defender applies the change, about a second later.
$deadline = (Get-Date).AddSeconds(30)
$status = Get-MpComputerStatus
while ($status.RealTimeProtectionEnabled -and -not $status.IsTamperProtected -and (Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    $status = Get-MpComputerStatus
}
if (-not $status.RealTimeProtectionEnabled) {
    Write-Output 'realtime-off: turned Defender real-time scanning off'
    exit 0
}
if ($status.IsTamperProtected) { Exit-Step 1 $tamperLine }
Exit-Step 1 'real-time scanning is still on 30 s after Set-MpPreference -DisableRealtimeMonitoring, with Tamper Protection off'
