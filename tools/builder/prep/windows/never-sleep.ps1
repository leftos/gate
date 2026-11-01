#Requires -Version 7.3
# Keeps the builder awake: the High performance power plan (SCHEME_MIN), no standby on AC power and hibernate off. A
# machine that refuses SCHEME_MIN (Modern Standby keeps Balanced) gets a warning, and the other two still change.
#   pwsh -File never-sleep.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$highPerformance = '8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("never-sleep: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running powercfg with $Arguments, or under -DryRun only says it would. Returns powercfg's
# exit code; a failure exits 1 unless -Soft.
function Invoke-Powercfg([string]$What, [string[]]$Arguments, [switch]$Soft) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "never-sleep: would $What"
        return
    }
    powercfg.exe @Arguments | Out-Null
    if ($LASTEXITCODE -and $Soft) { return $LASTEXITCODE }
    if ($LASTEXITCODE) { Exit-Step 1 "powercfg $($Arguments -join ' ') exited $LASTEXITCODE" }
    Write-Output "never-sleep: $What"
}

# The active plan's standby timeout on AC power, in seconds. powercfg's labels are localized, so this reads the
# hex values: the AC index is the second-to-last one, before the DC index.
function Get-StandbyAc {
    $values = @((powercfg.exe /query SCHEME_CURRENT SUB_SLEEP STANDBYIDLE) | ForEach-Object { [regex]::Matches($_, '0x[0-9a-fA-F]{8}') } |
        ForEach-Object { $_.Value })
    if ($values.Count -lt 2) { throw 'powercfg /query SCHEME_CURRENT SUB_SLEEP STANDBYIDLE printed no AC and DC indexes' }
    return [Convert]::ToInt64($values[-2], 16)
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

if (((powercfg.exe /getactivescheme) -join ' ') -notmatch $highPerformance) {
    $outcome = @(Invoke-Powercfg 'set the High performance power plan (SCHEME_MIN)' @('/setactive', 'SCHEME_MIN') -Soft)
    if ($outcome.Count -and $outcome[0] -isnot [string]) {
        Write-Output 'never-sleep: this machine keeps the Balanced plan (Modern Standby); standby and hibernate are still set'
    }
    else { $outcome }
}
if (($DryRun -and $changed) -or (Get-StandbyAc) -ne 0) {
    Invoke-Powercfg 'turn standby on AC power off (standby-timeout-ac 0)' @('/change', 'standby-timeout-ac', '0')
}
$hibernate = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\Power' -Name HibernateEnabled -ErrorAction SilentlyContinue
if ($null -eq $hibernate -or $hibernate.HibernateEnabled -ne 0) { Invoke-Powercfg 'turn hibernate off' @('/hibernate', 'off') }
if (-not $changed) { Write-Output 'never-sleep: already done' }
exit 0
