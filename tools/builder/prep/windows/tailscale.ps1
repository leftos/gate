#Requires -Version 7.3
# Installs Tailscale with winget and signs the builder in: tailscale up --unattended prints a link, and the person at
# the terminal opens it to sign in. No auth key is ever taken or passed.
#   pwsh -File tailscale.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$tailscale = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("tailscale: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would. -Show lets the action's output through.
function Invoke-Change([string]$What, [scriptblock]$Action, [switch]$Show) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "tailscale: would $What"
        return
    }
    if ($Show) { & $Action } else { & $Action | Where-Object { $_ -is [string] } }
    Write-Output "tailscale: $What"
}

# Tailscale's backend state (Running once signed in), or NotInstalled.
function Get-TailscaleState {
    if (-not (Test-Path -LiteralPath $tailscale)) { return 'NotInstalled' }
    $status = (& $tailscale status --json) -join "`n"
    try { return "$(($status | ConvertFrom-Json).BackendState)" }
    catch { throw "tailscale status --json printed no JSON: $status" }
}

# Waits until tailscaled answers tailscale status, polling once a second for 30 s; a fresh install starts it late.
function Wait-Tailscaled {
    for ($poll = 1; $poll -le 30; $poll++) {
        $status = (& $tailscale status --json 2>$null) -join "`n"
        if (-not $LASTEXITCODE -and $status) { return }
        if ($poll -lt 30) { Start-Sleep -Seconds 1 }
    }
    Exit-Step 1 'tailscaled did not start within 30 s; open Tailscale once, then run this step again'
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$state = Get-TailscaleState
if ($state -eq 'NotInstalled') {
    Invoke-Change 'install Tailscale.Tailscale with winget' {
        winget install --id Tailscale.Tailscale -e --silent --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE) { throw "winget install Tailscale.Tailscale exited $LASTEXITCODE; install it from tailscale.com and rerun" }
    }
    if (-not $DryRun) { Wait-Tailscaled }
}
if ($state -ne 'Running') {
    Invoke-Change 'run tailscale up --unattended; open the link it prints to sign in' -Show {
        & $tailscale up --unattended
        if ($LASTEXITCODE) { throw "tailscale up --unattended exited $LASTEXITCODE" }
    }
}
if (-not $changed) { Write-Output 'tailscale: already done' }
exit 0
