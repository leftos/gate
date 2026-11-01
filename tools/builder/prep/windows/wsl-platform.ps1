#Requires -Version 7.3
# Enables the Windows features VirtualMachinePlatform and Microsoft-Windows-Subsystem-Linux, without restarting. When
# either changed, or one waits for a restart, it prints a line that is exactly REBOOT: prep-windows.ps1 then stops and
# asks for the reboot. The WSL distribution comes later, from install-toolchains.ps1's WSL Ubuntu step.
#   pwsh -File wsl-platform.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$features = @('VirtualMachinePlatform', 'Microsoft-Windows-Subsystem-Linux')

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("wsl-platform: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "wsl-platform: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "wsl-platform: $What"
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$pending = $false
foreach ($feature in $features) {
    $state = "$((Get-WindowsOptionalFeature -Online -FeatureName $feature).State)"
    if ($state -eq 'EnablePending') { $pending = $true; continue }
    if ($state -ne 'Enabled') {
        Invoke-Change "enable the Windows feature $feature (a reboot finishes it)" {
            Enable-WindowsOptionalFeature -Online -FeatureName $feature -All -NoRestart
        }
    }
}
if (-not $changed -and -not $pending) {
    Write-Output 'wsl-platform: already done'
    exit 0
}
if (-not $DryRun) {
    Write-Output 'wsl-platform: a reboot finishes the WSL features'
    Write-Output 'REBOOT'
}
exit 0
