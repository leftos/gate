#Requires -Version 7.3
# Sets every network profile that is not a domain's to Private and lets TCP 22 in from the local subnet only. It owns
# the rule gate-prep-lan-ssh (group gate-prep-lan), which it creates or resets to TCP 22 from LocalSubnet on the
# Private and Domain profiles, and every enabled inbound TCP 22 rule outside a gate-prep-* group, such as OpenSSH's
# own, which it narrows the same way; each narrowed rule's old scope goes to prep.log as a NOTE. Rules that also admit
# TCP 22 through port Any or a range are named and left alone. Run over ssh, it refuses when the session's own address
# is not on a local subnet.
#   pwsh -File ssh-lan-only.ps1 [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'DryRun', Justification = 'Invoke-Change reads it')]
[CmdletBinding()]
param(
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$ruleName = 'gate-prep-lan-ssh'
$group = 'gate-prep-lan'
$profiles = @('Private', 'Domain')

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("ssh-lan-only: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "ssh-lan-only: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "ssh-lan-only: $What"
}

# The enabled inbound Allow rules outside the gate-prep groups whose port filter $Test accepts.
function Get-InboundRule([scriptblock]$Test) {
    return @(Get-NetFirewallRule -Direction Inbound -Action Allow -Enabled True | Where-Object {
            "$($_.Enabled)" -eq 'True' -and "$($_.Direction)" -eq 'Inbound' -and "$($_.Action)" -eq 'Allow' -and
            "$($_.Group)" -notlike 'gate-prep-*' -and
            (& $Test (Get-NetFirewallPortFilter -AssociatedNetFirewallRule $_) $_)
        })
}

# Whether a port filter admits TCP 22 through port Any or a range, for a rule whose program is any or sshd.
function Test-BroadSsh($Port, $Rule) {
    if ("$($Port.Protocol)" -notin 'TCP', 'Any') { return $false }
    $wide = @($Port.LocalPort | Where-Object {
            "$_" -eq 'Any' -or ("$_" -match '^(\d+)-(\d+)$' -and [int]$Matches[1] -le 22 -and [int]$Matches[2] -ge 22)
        })
    $program = "$((Get-NetFirewallApplicationFilter -AssociatedNetFirewallRule $Rule).Program)"
    return $wide.Count -gt 0 -and ($program -eq 'Any' -or $program -like '*sshd.exe')
}

function Get-ProfileKey($RuleProfile) { return ((("$RuleProfile" -split ',\s*') | Sort-Object) -join ',') }

function Test-InSubnet([string]$Address, [string]$Network, [int]$Prefix) {
    $a = [System.Net.IPAddress]::Parse($Address).GetAddressBytes()
    $n = [System.Net.IPAddress]::Parse($Network).GetAddressBytes()
    if ($a.Count -ne $n.Count) { return $false }
    for ($i = 0; $i -lt $a.Count; $i++) {
        $mask = (0xFF -shl (8 - [Math]::Min(8, [Math]::Max(0, $Prefix - 8 * $i)))) -band 0xFF
        if (($a[$i] -band $mask) -ne ($n[$i] -band $mask)) { return $false }
    }
    return $true
}

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$client = "$(("$env:SSH_CLIENT" -split ' ')[0])"
if ($client) {
    $local = @(Get-NetIPAddress | Where-Object { Test-InSubnet -Address $client -Network "$($_.IPAddress)" -Prefix ([int]$_.PrefixLength) })
    if (-not $local) {
        Exit-Step 1 ("this ssh session comes from $client, which LocalSubnet does not hold, so the new scope would shut it out; " +
            'run it from the LAN or skip this step')
    }
}
foreach ($network in @(Get-NetConnectionProfile)) {
    if ("$($network.NetworkCategory)" -notin 'Private', 'DomainAuthenticated') {
        Invoke-Change "set the network '$($network.Name)' Private" {
            Set-NetConnectionProfile -InterfaceIndex $network.InterfaceIndex -NetworkCategory Private
        }
    }
}
$lanRule = Get-NetFirewallRule -Name $ruleName -ErrorAction SilentlyContinue
if (-not $lanRule) {
    Invoke-Change "add the firewall rule $ruleName (group $group): TCP 22 in from LocalSubnet, profiles Private and Domain" {
        New-NetFirewallRule -Name $ruleName -DisplayName 'gate prep LAN ssh' -Group $group -Direction Inbound -Protocol TCP `
            -LocalPort 22 -Action Allow -Profile $profiles -RemoteAddress LocalSubnet -Enabled True
    }
}
elseif ("$($lanRule.Group)" -cne $group) {
    Exit-Step 1 ("the firewall rule $ruleName is in group '$($lanRule.Group)', not $group; " +
        "remove it with Remove-NetFirewallRule -Name $ruleName and rerun")
}
else {
    $address = Get-NetFirewallAddressFilter -AssociatedNetFirewallRule $lanRule
    $right = "$($lanRule.Enabled)" -eq 'True' -and (@($address.RemoteAddress) -join ',') -eq 'LocalSubnet' -and
    (Get-ProfileKey $lanRule.Profile) -eq 'Domain,Private'
    if (-not $right) {
        Invoke-Change "reset the firewall rule $ruleName to TCP 22 in from LocalSubnet, profiles Private and Domain, enabled" {
            Set-NetFirewallRule -Name $ruleName -Enabled True -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow `
                -Profile $profiles -RemoteAddress LocalSubnet
        }
    }
}
foreach ($rule in Get-InboundRule { param($Port) "$($Port.Protocol)" -eq 'TCP' -and @($Port.LocalPort) -contains '22' }) {
    $address = Get-NetFirewallAddressFilter -AssociatedNetFirewallRule $rule
    $was = "RemoteAddress $(@($address.RemoteAddress) -join ', '), Profile $($rule.Profile)"
    if ((@($address.RemoteAddress) -join ',') -ne 'LocalSubnet' -or (Get-ProfileKey $rule.Profile) -ne 'Domain,Private') {
        Write-Output "NOTE firewall rule $($rule.Name) was $was"
        Invoke-Change "narrow the firewall rule $($rule.Name) to LocalSubnet, profiles Private and Domain (was $was)" {
            Set-NetFirewallRule -Name $rule.Name -RemoteAddress LocalSubnet -Profile $profiles
        }
    }
}
foreach ($rule in Get-InboundRule { param($Port, $Rule) Test-BroadSsh $Port $Rule }) {
    Write-Output "ssh-lan-only: TCP 22 is also open through the firewall rule $($rule.Name) (port Any or a range); it is not scoped"
    Write-Output "NOTE TCP 22 is also open through the firewall rule $($rule.Name), which this step leaves alone"
}
if (-not $changed) { Write-Output 'ssh-lan-only: already done' }
exit 0
