#Requires -Version 7.3
# Turns Developer Mode on, which lets a non-elevated token create symbolic links: AppModelUnlock's
# AllowDevelopmentWithoutDevLicense is set to 1 and read back.
#   pwsh -File developer-mode.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'
$name = 'AllowDevelopmentWithoutDevLicense'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("developer-mode: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

function Get-DeveloperMode {
    $item = Get-ItemProperty -LiteralPath $key -Name $name -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return $item.$name
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

if ((Get-DeveloperMode) -eq 1) {
    Write-Output 'developer-mode: already done'
    exit 0
}
if ($DryRun) {
    Write-Output "developer-mode: would set $name to 1 under $key"
    exit 0
}
if (-not (Test-Path -LiteralPath $key)) { New-Item -Path $key -Force | Out-Null }
New-ItemProperty -LiteralPath $key -Name $name -PropertyType DWord -Value 1 -Force | Out-Null
$after = Get-DeveloperMode
if ($after -ne 1) { Exit-Step 1 "$key $name reads '$after' after setting it; expected 1" }
Write-Output "developer-mode: set $name to 1 under $key"
exit 0
