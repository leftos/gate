#Requires -Version 7.3
# Creates C:\build-box when it is missing and gives it a protected DACL: owner Administrators, full control for
# Administrators and SYSTEM, read and execute for -User, all inherited by files and folders, and no deny entry (a deny
# on -User would also deny the operator's elevated token). A routed command then cannot replace a script or download
# that an elevated run executes from there. "Already done" compares the owner, the protection and the explicit
# entries, not the raw SDDL that Windows reads back in its own form.
#   pwsh -File build-box-acl.ps1 -User <name> [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'DryRun', Justification = 'Invoke-Change reads it')]
[CmdletBinding()]
param(
    [string]$User,
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$path = 'C:\build-box'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("build-box-acl: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "build-box-acl: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "build-box-acl: $What"
}

# -User's SID, or $null when it names no account.
function Get-UserSid {
    try { return ([Security.Principal.NTAccount]::new($User)).Translate([Security.Principal.SecurityIdentifier]).Value }
    catch { return $null }
}

function Get-BuildBoxAcl([string]$UserSid) {
    $acl = [System.Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner([System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    $inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $grants = @(
        @('S-1-5-32-544', [System.Security.AccessControl.FileSystemRights]::FullControl),
        @('S-1-5-18', [System.Security.AccessControl.FileSystemRights]::FullControl),
        @($UserSid, [System.Security.AccessControl.FileSystemRights]::ReadAndExecute))
    foreach ($grant in $grants) {
        $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
                [System.Security.Principal.SecurityIdentifier]::new($grant[0]), $grant[1], $inherit,
                [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow))
    }
    return $acl
}

# The owner, the protection and the sorted explicit entries (SID, rights without Synchronize, type, inheritance and
# propagation), which read the same for an ACL built here and the one Windows stores from it.
function Get-AclShape($Acl) {
    $sidType = [System.Security.Principal.SecurityIdentifier]
    $rules = @($Acl.GetAccessRules($true, $false, $sidType) | ForEach-Object {
            $rights = [int]$_.FileSystemRights -band -bnot 0x100000
            '{0}|{1}|{2}|{3}|{4}' -f $_.IdentityReference.Value, $rights, $_.AccessControlType, [int]$_.InheritanceFlags, [int]$_.PropagationFlags
        } | Sort-Object)
    return '{0};{1};{2}' -f $Acl.GetOwner($sidType).Value, $Acl.AreAccessRulesProtected, ($rules -join ',')
}

if (-not $User) { Exit-Step 2 '-User is required: the builder account that gets read and execute, e.g. -User builder' }
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$sid = Get-UserSid
if (-not $sid -and -not $DryRun) { Exit-Step 2 "-User '$User' is not an account on this machine; run the builder-account step first" }
$what = "owner Administrators, inheritance off, full control for Administrators and SYSTEM, read and execute for $User"
$exists = Test-Path -LiteralPath $path
if (-not $exists) {
    Invoke-Change "create $path with $what" { New-Item -ItemType Directory -Path $path }
}
if (-not $exists -or -not $sid -or (Get-AclShape (Get-Acl -LiteralPath $path)) -ne (Get-AclShape (Get-BuildBoxAcl $sid))) {
    Invoke-Change "set the DACL of $path to $what" {
        $want = Get-BuildBoxAcl $sid
        Set-Acl -LiteralPath $path -AclObject $want
        $have = Get-AclShape (Get-Acl -LiteralPath $path)
        if ($have -ne (Get-AclShape $want)) { throw "$path reads $have after Set-Acl, expected $(Get-AclShape $want)" }
    }
}
if (-not $changed) { Write-Output 'build-box-acl: already done' }
exit 0
