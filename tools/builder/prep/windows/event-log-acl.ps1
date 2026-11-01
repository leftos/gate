#Requires -Version 7.3
# Lets -User write to the Application event log, where the gate's tasks log: (A;;0x2;;;<SID of -User>) is added to the
# log's channelAccess with wevtutil and read back. Routed commands, which run as -User, can then write events under
# any source.
#   pwsh -File event-log-acl.ps1 -User <name> [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [string]$User,
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("event-log-acl: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# -User's SID, or $null when it names no account.
function Get-UserSid {
    try { return ([Security.Principal.NTAccount]::new($User)).Translate([Security.Principal.SecurityIdentifier]).Value }
    catch {
        return $null
    }
}

function Get-ApplicationLogAccess {
    $lines = @(wevtutil.exe gl Application)
    if ($LASTEXITCODE) { throw "wevtutil gl Application exited $LASTEXITCODE" }
    $access = @($lines | Where-Object { $_ -match '^\s*channelAccess:' })
    if (-not $access) { throw 'wevtutil gl Application printed no channelAccess line' }
    return ($access[0] -replace '^\s*channelAccess:\s*', '').Trim()
}

if (-not $User) { Exit-Step 2 '-User is required: the builder account that writes events, e.g. -User builder' }
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$sid = Get-UserSid
if (-not $sid -and -not $DryRun) { Exit-Step 2 "-User '$User' is not an account on this machine; run the builder-account step first" }
$ace = if ($sid) { "(A;;0x2;;;$sid)" } else { "(A;;0x2;;;<SID of $User>)" }
$sddl = Get-ApplicationLogAccess
if ($sid -and $sddl.Contains($ace)) {
    Write-Output 'event-log-acl: already done'
    exit 0
}
if ($DryRun) {
    Write-Output "event-log-acl: would add $ace to the Application log's channelAccess $sddl"
    exit 0
}
$saclAt = $sddl.IndexOf('S:')
$updated = if ($saclAt -ge 0) { $sddl.Insert($saclAt, $ace) } else { $sddl + $ace }
wevtutil.exe sl Application "/ca:$updated"
if ($LASTEXITCODE) { Exit-Step 1 "wevtutil sl Application /ca exited $LASTEXITCODE" }
$after = Get-ApplicationLogAccess
if (-not $after.Contains($ace)) { Exit-Step 1 "the Application log's channelAccess reads $after after wevtutil sl; $ace is missing" }
Write-Output "event-log-acl: Application log channelAccess $sddl -> $after"
exit 0
