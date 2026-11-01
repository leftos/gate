#Requires -Version 7.3
# Lets ssh in over the tailnet only. It creates or resets its own rule gate-prep-tailnet-ssh (group gate-prep-tailnet):
# TCP 22 from the tailnet ranges 100.64.0.0/10 and fd7a:115c:a1e0::/48 on the Tailscale interface, profiles Private and
# Domain. Then it disables OpenSSH's stock OpenSSH-Server-In-TCP and every other enabled inbound TCP 22 rule outside a
# gate-prep-* group, noting each one's old scope in prep.log. It never touches ssh-lan-only's gate-prep-lan rule: that
# rule is the one to disable by hand to close LAN ssh. It refuses while Tailscale is not signed in, while the Tailscale
# network is not Private, and over an ssh session that does not come from the tailnet.
#   pwsh -File ssh-tailnet-only.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'DryRun', Justification = 'Invoke-Change reads it')]
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$tailscale = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
$ruleName = 'gate-prep-tailnet-ssh'
$group = 'gate-prep-tailnet'
$ranges = @('100.64.0.0/10', 'fd7a:115c:a1e0::/48')
$profiles = @('Private', 'Domain')

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("ssh-tailnet-only: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "ssh-tailnet-only: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "ssh-tailnet-only: $What"
}

# An address or subnet in one form, network/prefix with the host bits cleared, so a /10 equals the dotted mask the
# firewall reads it back as. A bare address is a /32 or /128.
function ConvertTo-Subnet([string]$Text) {
    $address, $mask = $Text -split '/', 2
    $bytes = [System.Net.IPAddress]::Parse($address).GetAddressBytes()
    $prefix = $bytes.Count * 8
    if ($mask -match '^\d+$') { $prefix = [int]$mask }
    elseif ($mask) {
        $ones = @([System.Net.IPAddress]::Parse($mask).GetAddressBytes() | ForEach-Object { [Convert]::ToString($_, 2).Replace('0', '') })
        $prefix = ($ones -join '').Length
    }
    for ($i = 0; $i -lt $bytes.Count; $i++) {
        $bytes[$i] = $bytes[$i] -band ((0xFF -shl (8 - [Math]::Min(8, [Math]::Max(0, $prefix - 8 * $i)))) -band 0xFF)
    }
    return "$([System.Net.IPAddress]::new([byte[]]$bytes))/$prefix"
}

# A rule's remote addresses in one sorted form; words such as LocalSubnet stay as they are.
function Get-ScopeKey([object[]]$Addresses) {
    $keys = foreach ($address in $Addresses) {
        try { ConvertTo-Subnet "$address" }
        catch { "$address" }
    }
    return (@($keys | Sort-Object) -join ',')
}

# Whether $Address lies in one of the tailnet ranges.
function Test-InTailnet([string]$Address) {
    foreach ($range in $ranges) {
        $prefix = [int]($range -split '/')[1]
        if ((ConvertTo-Subnet "$Address/$prefix") -eq (ConvertTo-Subnet $range)) { return $true }
    }
    return $false
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$state = 'NotInstalled'
if (Get-Command -Name $tailscale -ErrorAction SilentlyContinue) {
    $status = (& $tailscale status --json) -join "`n"
    try { $state = "$(($status | ConvertFrom-Json).BackendState)" }
    catch { $state = 'Unknown' }
}
if ($state -ne 'Running') { Exit-Step 1 'tailscale is not signed in; sign in first or skip this step' }
$ip = "$(@(& $tailscale ip -4)[0])".Trim()
$alias = "$(@(Get-NetIPAddress -IPAddress $ip -ErrorAction SilentlyContinue)[0].InterfaceAlias)"
if (-not $alias) { Exit-Step 1 "no network interface holds the tailnet address '$ip'; check tailscale ip -4" }
$category = "$(@(Get-NetConnectionProfile -InterfaceAlias $alias -ErrorAction SilentlyContinue)[0].NetworkCategory)"
if ($category -notin 'Private', 'DomainAuthenticated') {
    Exit-Step 1 "the Tailscale network ($alias) is '$category', so a Private and Domain rule would not apply there; run ssh-lan-only first"
}
$client = "$(("$env:SSH_CLIENT" -split ' ')[0])"
if ($client -and -not (Test-InTailnet $client)) {
    Exit-Step 1 ("this ssh session comes from $client, which $($ranges -join ' and ') do not hold, so it would be shut out; " +
        'reconnect over the tailnet or skip this step')
}
$own = Get-NetFirewallRule -Name $ruleName -ErrorAction SilentlyContinue
if (-not $own) {
    Invoke-Change "add the firewall rule $ruleName (group $group): TCP 22 in from $($ranges -join ', ') on $alias" {
        New-NetFirewallRule -Name $ruleName -DisplayName 'gate prep tailnet ssh' -Group $group -Direction Inbound -Protocol TCP `
            -LocalPort 22 -Action Allow -Profile $profiles -RemoteAddress $ranges -InterfaceAlias $alias -Enabled True
    }
}
elseif ("$($own.Group)" -cne $group) {
    Exit-Step 1 "the firewall rule $ruleName is in group '$($own.Group)', not $group; remove it with Remove-NetFirewallRule -Name $ruleName and rerun"
}
else {
    $scope = Get-ScopeKey @((Get-NetFirewallAddressFilter -AssociatedNetFirewallRule $own).RemoteAddress)
    $interfaces = @((Get-NetFirewallInterfaceFilter -AssociatedNetFirewallRule $own).InterfaceAlias) -join ','
    if ("$($own.Enabled)" -ne 'True' -or $scope -ne (Get-ScopeKey $ranges) -or $interfaces -ne $alias) {
        Invoke-Change "reset the firewall rule $ruleName to TCP 22 in from $($ranges -join ', ') on $alias, enabled" {
            Set-NetFirewallRule -Name $ruleName -Enabled True -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow `
                -Profile $profiles -RemoteAddress $ranges -InterfaceAlias $alias
        }
    }
}
$others = @(Get-NetFirewallRule -Direction Inbound -Action Allow -Enabled True | Where-Object {
        $port = Get-NetFirewallPortFilter -AssociatedNetFirewallRule $_
        "$($_.Enabled)" -eq 'True' -and "$($_.Direction)" -eq 'Inbound' -and "$($_.Action)" -eq 'Allow' -and
        "$($_.Group)" -notlike 'gate-prep-*' -and "$($port.Protocol)" -eq 'TCP' -and @($port.LocalPort) -contains '22'
    })
foreach ($rule in $others) {
    $was = "enabled, RemoteAddress $(@((Get-NetFirewallAddressFilter -AssociatedNetFirewallRule $rule).RemoteAddress) -join ', ')"
    Write-Output "NOTE firewall rule $($rule.Name) was $was"
    Invoke-Change "disable the firewall rule $($rule.Name) (was $was)" { Set-NetFirewallRule -Name $rule.Name -Enabled False }
}
if (-not $changed) { Write-Output 'ssh-tailnet-only: already done' }
exit 0
