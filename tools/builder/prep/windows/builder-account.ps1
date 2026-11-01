#Requires -Version 7.3
# Creates the local account -User, with a password typed twice at the console, and puts it in Administrators. An
# existing account keeps its password and settings; only a missing Administrators membership is added.
#   pwsh -File builder-account.ps1 -User <name> [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument or a session with no console.
[CmdletBinding()]
param(
    [string]$User,
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("builder-account: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "builder-account: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "builder-account: $What"
}

# Whether two SecureStrings hold the same text, compared in unmanaged memory that is zeroed afterwards.
function Test-SamePassword([securestring]$First, [securestring]$Second) {
    if ($First.Length -ne $Second.Length) { return $false }
    $marshal = [Runtime.InteropServices.Marshal]
    $a = $marshal::SecureStringToGlobalAllocUnicode($First)
    $b = $marshal::SecureStringToGlobalAllocUnicode($Second)
    try {
        for ($i = 0; $i -lt $First.Length * 2; $i += 2) {
            if ($marshal::ReadInt16($a, $i) -ne $marshal::ReadInt16($b, $i)) { return $false }
        }
        return $true
    }
    finally {
        $marshal::ZeroFreeGlobalAllocUnicode($a)
        $marshal::ZeroFreeGlobalAllocUnicode($b)
    }
}

# The new account's password, typed twice; a mismatch or an empty one asks again, at most 3 times.
function Read-NewPassword {
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $first = Read-Host -AsSecureString "password for $name"
        $second = Read-Host -AsSecureString "password for $name again"
        $same = $first.Length -gt 0 -and (Test-SamePassword $first $second)
        $second.Dispose()
        if ($same) { return $first }
        $first.Dispose()
        Write-Warning 'builder-account: the passwords were empty or differ; type them again'
    }
    throw "the passwords did not match 3 times; $name was not created"
}

# Whether the account with SID $Sid, named $Name, is in Administrators. Get-LocalGroupMember throws on a group that
# holds an orphaned or Entra SID, so then the members are read from net localgroup, by name.
function Test-AdminMember([string]$Sid, [string]$Name) {
    try {
        return @(Get-LocalGroupMember -SID 'S-1-5-32-544' | Where-Object { "$($_.SID)" -eq $Sid }).Count -gt 0
    }
    catch {
        Write-Warning "builder-account: Get-LocalGroupMember failed ($($_.Exception.Message)); reading net localgroup instead"
    }
    $group = ([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value -split '\\')[-1]
    $members = @(net.exe localgroup $group)
    if ($LASTEXITCODE) { throw "net localgroup $group exited ${LASTEXITCODE}: $($members -join ' ')" }
    return @($members | Where-Object { $_.Trim() -in $Name, "$env:COMPUTERNAME\$Name" }).Count -gt 0
}

if (-not $User) { Exit-Step 2 '-User is required: the builder account to create, e.g. -User builder' }
$name = $User
if ($User -match '^([^\\]+)\\(.+)$') {
    if ($Matches[1] -ne '.' -and $Matches[1] -ne $env:COMPUTERNAME) { Exit-Step 2 "-User '$User' is not a local account; give the name alone" }
    $name = $Matches[2]
}
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$account = Get-LocalUser -Name $name -ErrorAction SilentlyContinue
if ($account) {
    Write-Output "builder-account: $name exists; its password and settings are left alone"
}
else {
    if (-not $DryRun -and [Console]::IsInputRedirected) {
        Exit-Step 2 'builder-account asks for the password; run it at the console or over ssh -t'
    }
    Invoke-Change "create the local account $name with the password typed twice" {
        $password = Read-NewPassword
        try { New-LocalUser -Name $name -Password $password -FullName 'Gate builder' -PasswordNeverExpires -AccountNeverExpires }
        finally { $password.Dispose() }
    }
}
if (-not $account -and -not $DryRun) { $account = Get-LocalUser -Name $name }
$isAdmin = $account -and (Test-AdminMember "$($account.SID)" $name)
if (-not $isAdmin) {
    Invoke-Change "add $name to Administrators" { Add-LocalGroupMember -SID 'S-1-5-32-544' -Member $name }
}
if (-not $changed) { Write-Output 'builder-account: already done' }
exit 0
