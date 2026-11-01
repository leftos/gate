#Requires -Version 7.3
# Case table for prep-windows.ps1 and its step scripts under prep/windows. Copies them into a scratch folder under the
# repo's .tmp/ with C:\build-box, C:\ProgramData\ssh and the OpenSSH and Tailscale tool paths swapped for scratch
# stand-ins and every elevation check swapped for $true, so no case needs admin. Two copies are made:
# - a flow copy, whose step scripts are recorders that write their id to a file and change nothing, for the menu, the
#   argument refusals, the state file, the REASON and NOTE lines and the reboot stop;
# - a stubbed copy, run in a child pwsh that first dot-sources a stub file: every machine cmdlet and native tool the
#   scripts call is a function there, reads answering from a per-case config and writes only recording their call.
# Before any case runs, the stubbed copy's AST is checked, and the run stops when a script calls a command that is
# neither stubbed nor a plain file or text command, runs & on a variable that is not a stub name, a scriptblock
# parameter or the master's step path, calls a static .NET method outside the safe types, or hands a file cmdlet a
# literal path outside the scratch root or in the registry. A few pure functions (the ACL shape, the sshd_config
# rewrite, the subnet normaliser) are taken from the AST and run here. Prints ok/FAIL per case and exits with the
# failure count.
#
#   pwsh -NoProfile -File tools/builder/test-prep-windows.ps1
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceMaster = Join-Path $PSScriptRoot 'prep-windows.ps1'
$sourceSteps = Join-Path $PSScriptRoot 'prep/windows'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$root = Join-Path $repoRoot ".tmp/test-prep-windows-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
$machine = Join-Path $root 'machine'
$buildBox = Join-Path $machine 'build-box'
$sshDir = Join-Path $machine 'ssh'
$defaultConfig = Join-Path $machine 'sshd_config_default'
$flowDir = Join-Path $root 'flow'
$stubDir = Join-Path $root 'stubbed'
$stubFile = Join-Path $root 'stubs.ps1'
$runner = Join-Path $root 'run.ps1'
$keyFile = Join-Path $root 'builder.pub'
$testKey = 'ssh-' + 'ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest gate-prep-test'
$testUser = 'gate-prep-test-user'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$rebootLine = 'prep: reboot, then run prep again; finished steps show as done'
$topSettings = @('PasswordAuthentication no', 'PubkeyAuthentication yes', 'ClientAliveInterval 15', 'ClientAliveCountMax 4')
$script:fails = 0

# Commands the scripts may call unstubbed: they only touch paths the copies swapped into the scratch root, or no path.
$plainCommands = @(
    'Add-Content', 'ConvertFrom-Json', 'ConvertTo-Json', 'Copy-Item', 'ForEach-Object', 'Get-Command', 'Get-Content', 'Get-Date',
    'Get-Variable', 'Import-PowerShellDataFile', 'Join-Path', 'Measure-Object', 'Out-Null', 'Select-Object', 'Set-Content',
    'Set-StrictMode', 'Sort-Object', 'Split-Path', 'Test-Path', 'Where-Object', 'Write-Host', 'Write-Output', 'Write-Warning'
)
# Stubs that change state: each records 'W <name> <arguments>' and does nothing else.
$writeStubs = @(
    'Add-LocalGroupMember', 'Add-MpPreference', 'Add-Type', 'Add-WindowsCapability', 'Clear-Disk', 'Enable-WindowsOptionalFeature',
    'Format-Volume', 'Initialize-Disk', 'New-ItemProperty', 'New-LocalUser', 'New-NetFirewallRule', 'New-Partition',
    'Remove-ItemProperty', 'Restart-Service', 'Set-Acl', 'Set-ItemProperty', 'Set-MpPreference', 'Set-NetConnectionProfile',
    'Set-NetFirewallRule', 'Set-Service', 'Start-Service', 'Stop-Service', 'icacls.exe'
)
# Stubs that answer from the case's config; the native ones record a write for any subcommand that changes state, and
# New-Item creates only under the scratch root.
$readStubs = @(
    'Get-Acl', 'Get-ChildItem', 'Get-Disk', 'Get-ItemProperty', 'Get-LocalGroupMember', 'Get-LocalUser', 'Get-MpComputerStatus',
    'Get-MpPreference', 'Get-NetConnectionProfile', 'Get-NetFirewallAddressFilter', 'Get-NetFirewallApplicationFilter',
    'Get-NetFirewallInterfaceFilter', 'Get-NetFirewallPortFilter', 'Get-NetFirewallRule', 'Get-NetIPAddress', 'Get-Partition',
    'Get-Service', 'Get-Volume', 'Get-WindowsCapability', 'Get-WindowsOptionalFeature', 'New-Item', 'Read-Host', 'net.exe',
    'powercfg.exe', 'ssh-keygen.exe', 'sshd.exe', 'Start-Sleep', 'tailscale.exe', 'wevtutil.exe', 'winget'
)
# Static .NET calls that change nothing on the machine. GatePrepAutologon exists only through Add-Type, which is stubbed.
$safeTypes = @(
    'System.IO.Path', 'System.String', 'System.Text.*', 'System.Version', 'System.Console', 'System.Math', 'System.Convert',
    'System.Int32', 'System.Int64', 'System.Net.IPAddress', 'System.Security.Principal.*', 'System.Security.AccessControl.*',
    'System.Runtime.InteropServices.Marshal', 'System.Collections.Generic.*', 'GatePrepAutologon'
)
$pathCommands = @('New-Item', 'Copy-Item', 'Set-Content', 'Add-Content', 'Remove-Item', 'Set-ItemProperty')

$stubText = @'
Set-StrictMode -Version Latest
$stubConfig = Get-Content -LiteralPath $env:PREP_STUB_CONFIG -Raw | ConvertFrom-Json
$global:StubChannelAccess = $stubConfig.ChannelAccess
$global:StubStatusCalls = 0
# autologon reads the LSA secret's length through this type; a case that sets SecretLength stands it in.
if ($null -ne $stubConfig.SecretLength) {
    Microsoft.PowerShell.Utility\Add-Type -TypeDefinition ('public static class GatePrepAutologon { ' +
        "public static int SecretLength(string name) { return $([int]$stubConfig.SecretLength); } }")
}
function global:Write-StubCall([string]$Kind, [string]$Name, [object[]]$Arguments) {
    $shown = @($Arguments | ForEach-Object { if ($_ -is [array]) { $_ -join ',' } else { "$_" } })
    Add-Content -LiteralPath $env:PREP_STUB_CALLS -Value "$Kind $Name $($shown -join ' ')"
}
function global:Get-StubArgument([object[]]$Arguments, [string]$Name) {
    for ($i = 0; $i -lt $Arguments.Count - 1; $i++) { if ("$($Arguments[$i])" -eq "-$Name") { return $Arguments[$i + 1] } }
    return $null
}
foreach ($name in ($env:PREP_STUB_WRITES -split ',')) {
    Set-Item -Path "function:global:$name" -Value ([scriptblock]::Create("Write-StubCall W '$name' `$args; `$global:LASTEXITCODE = 0"))
}
function global:Get-LocalUser { Write-StubCall R Get-LocalUser $args; $stubConfig.LocalUser }
function global:Get-LocalGroupMember {
    Write-StubCall R Get-LocalGroupMember $args
    if ($stubConfig.AdminMembersThrow) { throw 'stub: Failed to compare two elements in the array.' }
    $stubConfig.AdminMembers
}
function global:Start-Sleep { Write-StubCall R Start-Sleep $args }
function global:net.exe { Write-StubCall R net.exe $args; $global:LASTEXITCODE = 0; $stubConfig.NetLocalgroup }
function global:Get-WindowsCapability {
    Write-StubCall R Get-WindowsCapability $args
    [pscustomobject]@{ Name = 'OpenSSH.Server~~~~0.0.1.0'; State = $stubConfig.SshCapability }
}
$global:StubSshdStatus = $stubConfig.SshdStatus
function global:Get-Service {
    Write-StubCall R Get-Service $args
    [pscustomobject]@{ Name = 'sshd'; Status = $global:StubSshdStatus; StartType = $stubConfig.SshdStartType }
}
# The service writes track sshd's status; a name in ThrowOn throws after recording its call, as a failed start does.
function global:Start-Service {
    Write-StubCall W Start-Service $args
    if ($stubConfig.ThrowOn -contains 'Start-Service') { throw 'stub: Failed to start service sshd.' }
    $global:StubSshdStatus = 'Running'
}
function global:Restart-Service {
    Write-StubCall W Restart-Service $args
    $global:StubSshdStatus = 'Stopped'
    if ($stubConfig.ThrowOn -contains 'Restart-Service') { throw 'stub: Failed to start service sshd.' }
    $global:StubSshdStatus = 'Running'
}
function global:Stop-Service { Write-StubCall W Stop-Service $args; $global:StubSshdStatus = 'Stopped' }
function global:Get-NetConnectionProfile {
    Write-StubCall R Get-NetConnectionProfile $args
    $alias = Get-StubArgument $args 'InterfaceAlias'
    if ($alias) { return $stubConfig.NetProfiles | Where-Object { $_.InterfaceAlias -eq $alias } }
    $stubConfig.NetProfiles
}
function global:Get-NetIPAddress {
    Write-StubCall R Get-NetIPAddress $args
    $address = Get-StubArgument $args 'IPAddress'
    if ($address) { return $stubConfig.IPAddresses | Where-Object { $_.IPAddress -eq $address } }
    $stubConfig.IPAddresses
}
function global:Get-NetFirewallRule {
    Write-StubCall R Get-NetFirewallRule $args
    $name = Get-StubArgument $args 'Name'
    if ($name) { return $stubConfig.FirewallRules | Where-Object { $_.Name -eq $name } }
    $stubConfig.FirewallRules
}
# A stub rule carries its port, address, program and interface fields, so it is its own filter.
foreach ($name in 'Port', 'Address', 'Application', 'Interface') {
    Set-Item -Path "function:global:Get-NetFirewall${name}Filter" -Value ([scriptblock]::Create(
            "Write-StubCall R Get-NetFirewall${name}Filter `$args; Get-StubArgument `$args 'AssociatedNetFirewallRule'"))
}
function global:Get-WindowsOptionalFeature {
    Write-StubCall R Get-WindowsOptionalFeature $args
    [pscustomobject]@{ FeatureName = (Get-StubArgument $args 'FeatureName'); State = $stubConfig.FeatureState }
}
function global:Get-MpPreference { Write-StubCall R Get-MpPreference $args; [pscustomobject]@{ ExclusionPath = $stubConfig.ExclusionPath } }
function global:Get-MpComputerStatus {
    Write-StubCall R Get-MpComputerStatus $args
    [pscustomobject]@{ RealTimeProtectionEnabled = $stubConfig.RealTimeProtectionEnabled; IsTamperProtected = $stubConfig.IsTamperProtected }
}
function global:Get-Disk {
    Write-StubCall R Get-Disk $args
    $number = Get-StubArgument $args 'Number'
    $stubConfig.Disks | Where-Object { "$($_.Number)" -eq "$number" }
}
function global:Get-Partition {
    Write-StubCall R Get-Partition $args
    $number = Get-StubArgument $args 'DiskNumber'
    $stubConfig.Partitions | Where-Object { "$($_.DiskNumber)" -eq "$number" }
}
function global:Get-Volume {
    Write-StubCall R Get-Volume $args
    $letter = Get-StubArgument $args 'DriveLetter'
    if ($letter) { return $stubConfig.Volumes | Where-Object { "$($_.DriveLetter)" -eq "$letter" } }
    $stubConfig.Volumes
}
function global:Get-ChildItem {
    $path = Get-StubArgument $args 'LiteralPath'
    if ("$path" -notmatch '^([A-Za-z]):\\$') { return Microsoft.PowerShell.Management\Get-ChildItem @args }
    Write-StubCall R Get-ChildItem $args
    $items = $stubConfig.RootItems.PSObject.Properties[$Matches[1]]
    if ($items) { $items.Value | ForEach-Object { [pscustomobject]@{ Name = $_ } } }
}
function global:Get-ItemProperty {
    Write-StubCall R Get-ItemProperty $args
    $name = Get-StubArgument $args 'Name'
    $value = $stubConfig.Registry.PSObject.Properties[$name]
    if ($value) { [pscustomobject]@{ $name = $value.Value } }
}
function global:Get-Acl {
    Write-StubCall R Get-Acl $args
    if ("$(Get-StubArgument $args 'LiteralPath')" -notlike '*administrators_authorized_keys' -or -not $stubConfig.KeyAclOk) {
        return Microsoft.PowerShell.Security\Get-Acl @args
    }
    $acl = [System.Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in 'S-1-5-32-544', 'S-1-5-18') {
        $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
                [System.Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'Allow'))
    }
    $acl
}
function global:New-Item {
    $path = "$(Get-StubArgument $args 'Path')$(Get-StubArgument $args 'LiteralPath')"
    if (-not $path.StartsWith($env:PREP_STUB_ROOT, [StringComparison]::OrdinalIgnoreCase)) { Write-StubCall W New-Item $args; return }
    Write-StubCall S New-Item $args
    Microsoft.PowerShell.Management\New-Item @args
}
function global:Read-Host {
    if ($args -contains '-AsSecureString') { Write-StubCall W Read-Host $args; throw 'stub: no password is typed in a test' }
    Write-StubCall R Read-Host $args
    $stubConfig.TypedLine
}
function global:wevtutil.exe {
    $global:LASTEXITCODE = 0
    if ($args[0] -ne 'gl') {
        Write-StubCall W wevtutil.exe $args
        $ca = @($args | Where-Object { "$_" -like '/ca:*' })
        if ($ca) { $global:StubChannelAccess = "$($ca[0])".Substring(4) }
        return
    }
    Write-StubCall R wevtutil.exe $args
    'name: Application'
    "channelAccess: $global:StubChannelAccess"
}
function global:powercfg.exe {
    $global:LASTEXITCODE = 0
    if ($args[0] -eq '/getactivescheme') { Write-StubCall R powercfg.exe $args; return $stubConfig.ActiveScheme }
    if ($args[0] -eq '/query') { Write-StubCall R powercfg.exe $args; return $stubConfig.StandbyAc }
    Write-StubCall W powercfg.exe $args
    if ($args[0] -eq '/setactive') { $global:LASTEXITCODE = $stubConfig.SetactiveExit }
}
function global:sshd.exe {
    if ($args[0] -ne '-t') { Write-StubCall W sshd.exe $args; $global:LASTEXITCODE = 0; return }
    Write-StubCall R sshd.exe $args
    $global:LASTEXITCODE = $stubConfig.SshdTestExit
}
function global:ssh-keygen.exe {
    if ($args[0] -ne '-l') { Write-StubCall W ssh-keygen.exe $args; $global:LASTEXITCODE = 0; return }
    Write-StubCall R ssh-keygen.exe $args
    $global:LASTEXITCODE = $stubConfig.SshKeygenExit
    '256 SHA256:stub gate-prep-test (ED25519)'
}
function global:tailscale.exe {
    $global:LASTEXITCODE = 0
    if ($args[0] -eq 'ip') { Write-StubCall R tailscale.exe $args; return $stubConfig.TailnetIp }
    if ($args[0] -ne 'status') { Write-StubCall W tailscale.exe $args; return }
    Write-StubCall R tailscale.exe $args
    $global:StubStatusCalls++
    if ($global:StubStatusCalls -lt $stubConfig.TailscaleAnswerOn) { $global:LASTEXITCODE = 1; return }
    "{ ""BackendState"": ""$($stubConfig.TailscaleState)"" }"
}
function global:winget {
    if ($args[0] -ne 'list') { Write-StubCall W winget $args; $global:LASTEXITCODE = 0; return }
    Write-StubCall R winget $args
    $global:LASTEXITCODE = $stubConfig.WingetListExit
}
'@

$runnerText = @'
param([string]$Stubs, [string]$Script, [string]$ArgumentFile)
$ErrorActionPreference = 'Stop'
. $Stubs
$arguments = Get-Content -LiteralPath $ArgumentFile -Raw | ConvertFrom-Json -AsHashtable
& $Script @arguments
exit $LASTEXITCODE
'@

function Test-Case {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Check)
    try {
        $problem = & $Check
    }
    catch {
        $problem = "threw: $($_.Exception.Message)"
    }
    if ($problem) {
        "  FAIL {0}: {1}" -f $Name, $problem
        $script:fails++
    }
    else { "  ok   {0}" -f $Name }
}

# A source script's text with the machine paths and the elevation check swapped for scratch stand-ins. A missed swap
# would let a case touch this PC, so any leftover throws before a copy is written.
function Get-ScratchText {
    param([Parameter(Mandatory)][string]$Path, [switch]$KeepConsoleCheck)
    $text = Get-Content -LiteralPath $Path -Raw
    $text = $text.Replace('C:\build-box', $buildBox).Replace('C:\ProgramData\ssh', $sshDir)
    foreach ($tool in 'sshd.exe', 'ssh-keygen.exe') { $text = $text.Replace("Join-Path `$env:WINDIR 'System32\OpenSSH\$tool'", "'$tool'") }
    $text = $text.Replace("Join-Path `$env:WINDIR 'System32\OpenSSH\sshd_config_default'", "'$defaultConfig'")
    $text = $text.Replace("Join-Path `$env:ProgramFiles 'Tailscale\tailscale.exe'", "'tailscale.exe'")
    $elevation = [regex]'(?m)^\$elevated = .*\r?\n.*Administrator\)\r?$'
    $count = $elevation.Matches($text).Count
    if ($count -ne 1) { throw "$Path has $count top-level `$elevated checks, expected 1" }
    $text = $elevation.Replace($text, '$elevated = $true')
    # dev-drive refuses redirected stdin before any disk call; the stubbed copy lets the cases past it.
    if ((Split-Path -Leaf $Path) -eq 'dev-drive.ps1' -and -not $KeepConsoleCheck) {
        $console = [regex]'(?m)^if \(-not \$DryRun -and \[Console\]::IsInputRedirected'
        $count = $console.Matches($text).Count
        if ($count -ne 1) { throw "$Path has $count top-level redirected-stdin checks, expected 1" }
        $text = $console.Replace($text, 'if (-not $DryRun -and $false')
    }
    foreach ($left in 'C:\build-box', 'C:\ProgramData\ssh', '$env:WINDIR', '$env:ProgramFiles') {
        if ($text.Contains($left)) { throw "the scratch copy of $Path still names $left" }
    }
    return $text
}

# Copies the master, the manifest and every step script into $Destination; -Recorders writes recorder steps instead.
function Copy-ScriptToScratch {
    param([Parameter(Mandatory)][string]$Destination, [switch]$Recorders)
    $stepCopy = Join-Path $Destination 'prep/windows'
    New-Item -ItemType Directory -Force $stepCopy | Out-Null
    Set-Content -LiteralPath (Join-Path $Destination 'prep-windows.ps1') -Value (Get-ScratchText $sourceMaster) -NoNewline
    Copy-Item -LiteralPath (Join-Path $sourceSteps 'steps.psd1') -Destination $stepCopy
    foreach ($step in Get-ChildItem -LiteralPath $sourceSteps -Filter '*.ps1') {
        $id = $step.BaseName
        $text = if ($Recorders) {
            "param([string]`$User, [string]`$PublicKeyFile, [string]`$GateRoot, [int]`$DiskNumber, [switch]`$DryRun)`n" +
            "Add-Content -LiteralPath `$env:PREP_TEST_RECORD -Value '$id'`n" +
            "if (`"`$env:PREP_TEST_NOTE`" -eq '$id') { 'NOTE old scope Any' }`n" +
            "if (`"`$env:PREP_TEST_FAIL`" -eq '$id') { 'REASON boom from $id'; exit 1 }`n" +
            "if (`"`$env:PREP_TEST_REBOOT`" -eq '$id') { 'REBOOT' }`nexit 0`n"
        }
        else { Get-ScratchText $step.FullName }
        Set-Content -LiteralPath (Join-Path $stepCopy $step.Name) -Value $text -NoNewline
    }
}

# The string literals assigned to $Name anywhere in $Ast, and whether every assignment to it is one.
function Get-LiteralAssignment {
    param([Parameter(Mandatory)]$Ast, [Parameter(Mandatory)][string]$Name)
    $variable = $Name
    $assignments = @($Ast.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and $node.Left.VariablePath.UserPath -eq $variable
            }.GetNewClosure(), $true))
    $values = @($assignments |
        ForEach-Object { if ($_.Right -is [System.Management.Automation.Language.CommandExpressionAst]) { $_.Right.Expression } } |
        Where-Object { $_ -is [System.Management.Automation.Language.StringConstantExpressionAst] } | ForEach-Object { $_.Value })
    return [pscustomobject]@{ Values = $values; AllLiteral = ($assignments.Count -gt 0 -and $values.Count -eq $assignments.Count) }
}

# Why `& $variable` in $Command is not allowed, or ''.
function Get-AmpersandProblem {
    param([Parameter(Mandatory)]$Command, [Parameter(Mandatory)]$Ast, [Parameter(Mandatory)][string]$File)
    $target = $Command.CommandElements[0]
    if ($Command.InvocationOperator -ne 'Ampersand' -or $target -isnot [System.Management.Automation.Language.VariableExpressionAst]) { return '' }
    $name = $target.VariablePath.UserPath
    if ($File -eq 'prep-windows.ps1' -and $name -eq 'path') { return '' }
    $blocks = @($Ast.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.ParameterAst] -and $node.Name.VariablePath.UserPath -eq $name -and
                $node.StaticType -eq [scriptblock]
            }.GetNewClosure(), $true))
    if ($blocks.Count) { return '' }
    $literal = Get-LiteralAssignment $Ast $name
    if ($literal.AllLiteral -and -not @($literal.Values | Where-Object { $_ -notin $readStubs + $writeStubs }).Count) { return '' }
    return "& `$$name"
}

# Why a path cmdlet call in $Command is not allowed, or '': a literal path (or a variable set only to literals) outside
# the scratch root or in the registry, unless the cmdlet is stubbed.
function Get-PathProblem {
    param([Parameter(Mandatory)]$Command, [Parameter(Mandatory)]$Ast)
    $name = $Command.GetCommandName()
    if ($name -notin $pathCommands -or $name -in $readStubs + $writeStubs) { return '' }
    $paths = foreach ($element in @($Command.CommandElements | Select-Object -Skip 1)) {
        if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) { $element.Value }
        elseif ($element -is [System.Management.Automation.Language.VariableExpressionAst]) {
            (Get-LiteralAssignment $Ast $element.VariablePath.UserPath).Values
        }
    }
    $bad = @($paths | Where-Object {
            $_ -match '^(?i)([a-z]:\\|hk(lm|cu):)' -and ($_ -match '^(?i)hk' -or -not $_.StartsWith($root, 'OrdinalIgnoreCase'))
        })
    if ($bad.Count) { return "$name $($bad -join ', ')" }
    return ''
}

# Why a static .NET call in $Member is not allowed, or ''.
function Get-StaticCallProblem {
    param([Parameter(Mandatory)]$Member)
    if (-not $Member.Static -or $Member.Expression -isnot [System.Management.Automation.Language.TypeExpressionAst]) { return '' }
    $written = $Member.Expression.TypeName.FullName
    $type = $written -as [type]
    $full = if ($type) { $type.FullName } else { $written }
    if (@($safeTypes | Where-Object { $full -like $_ }).Count) { return '' }
    return "[$full]::$($Member.Member)"
}

# Why the command $Command is not allowed, or '': the & and path checks, and a name that is neither covered nor one of
# the script's own functions $Local.
function Get-CommandProblem {
    param([Parameter(Mandatory)]$Command, [Parameter(Mandatory)]$Ast, [Parameter(Mandatory)][string]$File, [string[]]$Local = @())
    $name = $Command.GetCommandName()
    $problem = (Get-AmpersandProblem -Command $Command -Ast $Ast -File $File) + (Get-PathProblem -Command $Command -Ast $Ast)
    if ($name -and $name -notin $plainCommands + $writeStubs + $readStubs -and $name -notin $Local) { $problem += " calls $name" }
    return $problem
}

# Each '<file>:<line> <problem>' in the copied script $File.
function Get-ScriptProblem {
    param([Parameter(Mandatory)][System.IO.FileInfo]$File)
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($File.FullName, [ref]$null, [ref]$errors)
    if ($errors.Count) { throw "$($File.FullName) has parse errors: $($errors[0].Message)" }
    $local = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true) |
        ForEach-Object { $_.Name })
    foreach ($command in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true)) {
        $problem = Get-CommandProblem -Command $command -Ast $ast -File $File.Name -Local $local
        if ($problem) { "$($File.Name):$($command.Extent.StartLineNumber) $problem" }
    }
    foreach ($call in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.InvokeMemberExpressionAst] }, $true)) {
        $problem = Get-StaticCallProblem $call
        if ($problem) { "$($File.Name):$($call.Extent.StartLineNumber) $problem" }
    }
}

# Stops the run when a copied script does something the stubs cannot keep off this machine (see the header).
function Assert-CommandsCovered {
    param([Parameter(Mandatory)][string]$Directory)
    $uncovered = foreach ($file in Get-ChildItem -LiteralPath $Directory -Filter '*.ps1' -Recurse) { Get-ScriptProblem $file }
    if ($uncovered) { throw "the scripts do what the stubs cannot cover; stub or allow it in this test: $($uncovered -join ', ')" }
}

# Runs pwsh -File with $Arguments, stdin redirected, under the given environment; returns the exit code, both streams
# and the recorder and stub-call lines.
function Invoke-Child {
    param([Parameter(Mandatory)][string[]]$Arguments, [hashtable]$Environment = @{}, [string]$Stdin = '')
    $tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
    $errFile = Join-Path $root "stderr-$tag.log"
    $record = Join-Path $root "record-$tag.log"
    $calls = Join-Path $root "calls-$tag.log"
    $set = @{
        PREP_TEST_RECORD = $record; PREP_STUB_CALLS = $calls; PREP_STUB_ROOT = $root; GATE_PREP_TEST = '1'
        PREP_TEST_REBOOT = $null; PREP_TEST_FAIL = $null; PREP_TEST_NOTE = $null; SSH_CLIENT = $null
    }
    foreach ($key in $Environment.Keys) { $set[$key] = $Environment[$key] }
    $saved = @{}
    foreach ($key in $set.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        [Environment]::SetEnvironmentVariable($key, $set[$key])
    }
    try { $out = @($Stdin | & pwsh -NoProfile -NonInteractive -File @Arguments 2> $errFile) }
    finally { foreach ($key in $saved.Keys) { [Environment]::SetEnvironmentVariable($key, $saved[$key]) } }
    $code = $LASTEXITCODE
    $read = { param($Path) if (Test-Path -LiteralPath $Path) { @(Get-Content -LiteralPath $Path) } else { @() } }
    return [pscustomobject]@{
        ExitCode = $code
        StdOut   = ($out -join "`n")
        StdErr   = (& $read $errFile) -join "`n"
        Record   = & $read $record
        Calls    = & $read $calls
    }
}

# Runs the flow copy of prep-windows.ps1; -Lines are the menu's typed lines, fed through -TestInputFile.
function Invoke-Flow {
    param([string[]]$Arguments = @(), [string[]]$Lines, [hashtable]$Environment = @{})
    $all = @((Join-Path $flowDir 'prep-windows.ps1')) + $Arguments
    if ($PSBoundParameters.ContainsKey('Lines')) {
        $inputFile = Join-Path $root "input-$([guid]::NewGuid().ToString('N').Substring(0, 8)).txt"
        Set-Content -LiteralPath $inputFile -Value $Lines
        $all += @('-TestInputFile', $inputFile)
    }
    return Invoke-Child -Arguments $all -Environment $Environment
}

# A stub firewall rule: an enabled inbound Allow rule for TCP 22 from anywhere unless $Fields say otherwise.
function Get-StubRule {
    param([Parameter(Mandatory)][string]$Name, [hashtable]$Fields = @{})
    $rule = @{
        Name = $Name; Group = ''; Protocol = 'TCP'; LocalPort = '22'; RemoteAddress = 'Any'; Direction = 'Inbound'; Action = 'Allow'
        Enabled = 'True'; Profile = 'Any'; Program = 'Any'; InterfaceAlias = 'Any'
    }
    foreach ($key in $Fields.Keys) { $rule[$key] = $Fields[$key] }
    return $rule
}

# The stub config: every read answers "not done yet" unless $Override replaces it.
function Get-StubConfig {
    param([hashtable]$Override = @{})
    $config = @{
        LocalUser = $null; AdminMembers = @(); AdminMembersThrow = $false; NetLocalgroup = @(); SecretLength = $null; TailscaleAnswerOn = 1
        SshCapability = 'NotPresent'; SshdStatus = 'Stopped'; SshdStartType = 'Manual'; SshdTestExit = 0; SshKeygenExit = 0; KeyAclOk = $true
        ThrowOn = @()
        NetProfiles = @(@{ Name = 'lan'; NetworkCategory = 'Public'; InterfaceIndex = 4; InterfaceAlias = 'Ethernet' })
        IPAddresses = @(@{ IPAddress = '192.168.1.10'; PrefixLength = 24; InterfaceAlias = 'Ethernet' })
        FirewallRules = @(Get-StubRule 'OpenSSH-Server-In-TCP')
        FeatureState = 'Disabled'; ExclusionPath = @(); RealTimeProtectionEnabled = $true; IsTamperProtected = $false
        Disks = @(); Partitions = @(); Volumes = @(); RootItems = @{}; TypedLine = ''
        Registry = @{ AllowDevelopmentWithoutDevLicense = 0; AutoAdminLogon = '0'; HibernateEnabled = 1 }
        ChannelAccess = 'O:BAG:SYD:(A;;0xf0007;;;SY)(A;;0x7;;;BA)S:(ML;;0x1;;;LW)'
        ActiveScheme = 'Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)'
        StandbyAc = @('    Current AC Power Setting Index: 0x00000708', '    Current DC Power Setting Index: 0x00000384')
        SetactiveExit = 0; TailscaleState = 'NeedsLogin'; TailnetIp = ''; WingetListExit = 1
    }
    foreach ($key in $Override.Keys) { $config[$key] = $Override[$key] }
    return $config
}

# Runs a stubbed copy's script with $Parameters under the stubs and $Config.
function Invoke-Stubbed {
    param([Parameter(Mandatory)][string]$Script, [Parameter(Mandatory)][hashtable]$Parameters, [hashtable]$Config = (Get-StubConfig),
        [hashtable]$Environment = @{}, [string]$Stdin = '')
    $tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
    $configFile = Join-Path $root "config-$tag.json"
    $argumentFile = Join-Path $root "arguments-$tag.json"
    Set-Content -LiteralPath $configFile -Value ($Config | ConvertTo-Json -Depth 8)
    Set-Content -LiteralPath $argumentFile -Value ($Parameters | ConvertTo-Json -Depth 4)
    $childEnvironment = @{ PREP_STUB_CONFIG = $configFile; PREP_STUB_WRITES = ($writeStubs -join ',') }
    foreach ($key in $Environment.Keys) { $childEnvironment[$key] = $Environment[$key] }
    return Invoke-Child -Arguments @($runner, '-Stubs', $stubFile, '-Script', $Script, '-ArgumentFile', $argumentFile) `
        -Environment $childEnvironment -Stdin $Stdin
}

function Get-StubbedStep([string]$Id) { return Join-Path $stubDir "prep/windows/$Id.ps1" }

# The stub calls that would have changed state.
function Get-WriteCall([object]$Result) { return , @($Result.Calls | Where-Object { $_ -like 'W *' }) }

# The source definitions of a step's functions $Name, for a case to dot-source.
function Get-StepFunctionText {
    param([Parameter(Mandatory)][string]$Id, [Parameter(Mandatory)][string[]]$Name)
    $ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $sourceSteps "$Id.ps1"), [ref]$null, [ref]$null)
    $texts = foreach ($each in $Name) {
        $definition = $ast.Find({
                param($node)
                $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $each
            }.GetNewClosure(), $true)
        if (-not $definition) { throw "$Id.ps1 defines no $each" }
        $definition.Extent.Text
    }
    return $texts -join "`n"
}

# Puts the scratch machine back: no build-box, an ssh folder with a stock sshd_config, and sshd_config_default.
function Clear-Machine {
    if (Test-Path -LiteralPath $machine) { Remove-Item -LiteralPath $machine -Recurse -Force }
    New-Item -ItemType Directory -Force $sshDir | Out-Null
    $stock = @('#Port 22', 'PasswordAuthentication yes', 'Subsystem sftp sftp-server.exe', 'Match Group administrators', '  AuthorizedKeysFile k')
    Set-Content -LiteralPath (Join-Path $sshDir 'sshd_config') -Value $stock
    Set-Content -LiteralPath $defaultConfig -Value $stock
}

# Every file under the scratch machine with its hash, to show a run changed nothing.
function Get-MachineSnapshot {
    return (@(Get-ChildItem -LiteralPath $machine -Recurse -Force | ForEach-Object {
                $hash = if ($_.PSIsContainer) { 'dir' } else { (Get-FileHash -LiteralPath $_.FullName).Hash }
                "$($_.FullName) $hash"
            }) -join "`n")
}

function Get-ProblemUnlessExit {
    param([Parameter(Mandatory)][object]$Result, [Parameter(Mandatory)][int]$Expect)
    if ($Result.ExitCode -ne $Expect) { return "exit $($Result.ExitCode), expected $Expect; stdout: $($Result.StdOut); stderr: $($Result.StdErr)" }
    return ''
}

function Get-RecordProblem {
    param([Parameter(Mandatory)][object]$Result, [string[]]$Expect = @())
    if (($Result.Record -join ',') -ne ($Expect -join ',')) { return "ran [$($Result.Record -join ', ')], expected [$($Expect -join ', ')]" }
    return ''
}

# '' when the write calls are exactly the ones matching $Expect, in any order; else what differs.
function Get-WriteProblem {
    param([Parameter(Mandatory)][object]$Result, [string[]]$Expect = @(), [string[]]$Forbid = @())
    $writes = Get-WriteCall $Result
    foreach ($pattern in $Expect) {
        if (-not @($writes | Where-Object { $_ -like $pattern }).Count) { return "no write like '$pattern' in: $($writes -join '; ')" }
    }
    foreach ($pattern in $Forbid) {
        $hit = @($writes | Where-Object { $_ -like $pattern })
        if ($hit.Count) { return "a forbidden write: $($hit -join '; ')" }
    }
    return ''
}

New-Item -ItemType Directory -Force $root | Out-Null
try {
    $manifest = Import-PowerShellDataFile -LiteralPath (Join-Path $sourceSteps 'steps.psd1')
    $allIds = @($manifest.Steps | ForEach-Object { $_.Id })
    $defaultIds = @($manifest.Steps | Where-Object { $_.Default } | ForEach-Object { $_.Id })
    Set-Content -LiteralPath $stubFile -Value $stubText
    Set-Content -LiteralPath $runner -Value $runnerText
    Set-Content -LiteralPath $keyFile -Value $testKey
    Copy-ScriptToScratch -Destination $flowDir -Recorders
    Copy-ScriptToScratch -Destination $stubDir
    Assert-CommandsCovered -Directory $stubDir
    $flowBase = @('-User', $testUser, '-PublicKeyFile', $keyFile)

    Test-Case 'every manifest entry has its script and every script has an entry' {
        $scripts = @(Get-ChildItem -LiteralPath $sourceSteps -Filter '*.ps1' | ForEach-Object { $_.BaseName } | Sort-Object)
        $diff = @(Compare-Object -ReferenceObject @($allIds | Sort-Object) -DifferenceObject $scripts)
        if ($diff.Count) {
            return "manifest and scripts disagree: $(($diff | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }) -join '; ')"
        }
        return ''
    }

    Test-Case 'step ids are unique' {
        $dupes = @($allIds | Group-Object | Where-Object { $_.Count -gt 1 } | ForEach-Object { $_.Name })
        if ($dupes.Count) { return "duplicate ids: $($dupes -join ', ')" }
        return ''
    }

    Test-Case 'every destructive step starts unticked' {
        $ticked = @($manifest.Steps | Where-Object { $_.Destructive -and $_.Default } | ForEach-Object { $_.Id })
        if ($ticked.Count) { return "destructive and ticked by default: $($ticked -join ', ')" }
        return ''
    }

    Test-Case 'menu: 4 11 toggles both' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('4 11', '')
        $expect = @($allIds | Where-Object { ($_ -in $defaultIds -and $_ -ne 'defender-exclusion') -or $_ -eq 'tailscale' })
        (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r $expect)
    }

    Test-Case 'menu: a ticks all, and the master asks no FORMAT question of its own' {
        Clear-Machine
        $r = Invoke-Flow -Arguments ($flowBase + @('-DiskNumber', '5')) -Lines @('a', '')
        $problem = (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r $allIds)
        if (-not $problem -and $r.StdOut.Contains('type FORMAT')) { return "the master asked for FORMAT itself: $($r.StdOut)" }
        $problem
    }

    Test-Case 'menu: d resets to the defaults' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('a', 'd', '')
        (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r $defaultIds)
    }

    Test-Case 'menu: a bad token prints the line and redraws' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('x9', 'q')
        $problem = (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r)
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains("prep: 'x9' is not a step number, a, d or q")) { return "no bad-token line: $($r.StdOut)" }
        $draws = ([regex]::Matches($r.StdOut, 'toggle: numbers \(e\.g\. 4 11\), a=all, d=defaults, Enter=run, q=quit')).Count
        if ($draws -ne 2) { return "drew the menu $draws times, expected 2" }
        return ''
    }

    Test-Case 'menu: q exits 0 and runs nothing' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('q')
        (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r)
    }

    Test-Case 'menu: Enter with the defaults runs exactly the default ids' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('')
        $problem = (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r $defaultIds)
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains("gate builder prep  (elevated: yes, user: $testUser)")) { return "no menu title: $($r.StdOut)" }
        if ($r.StdOut -notmatch '(?m)^  #  \[x\] step +caveat if skipped\r?$') { return "no menu header: $($r.StdOut)" }
        if ($r.StdOut -notmatch '(?m)^  2  \[x\] OpenSSH, key-only ssh +no routing at all\r?$') { return "no openssh row: $($r.StdOut)" }
        $log = Get-Content -LiteralPath (Join-Path $buildBox 'prep.log') -Raw
        if ($log -notmatch 'BEGIN builder-account' -or $log -notmatch 'OK builder-account' -or $log -notmatch 'COMPLETE failed=0') {
            return "prep.log lacks BEGIN, OK or COMPLETE: $log"
        }
        return ''
    }

    Test-Case '-TestInputFile is refused outside the test suite' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase -Lines @('') -Environment @{ GATE_PREP_TEST = $null }
        (Get-ProblemUnlessExit $r 2) + (Get-RecordProblem $r)
    }

    Test-Case '[done] comes from the state file and a done step is not ticked again' {
        Clear-Machine
        New-Item -ItemType Directory -Force $buildBox | Out-Null
        Set-Content -LiteralPath (Join-Path $buildBox 'prep-state.json') -Value '{ "done": { "builder-account": "2026-11-01T10:00:00" } }'
        $r = Invoke-Flow -Arguments $flowBase -Lines @('')
        $problem = (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r @($defaultIds | Where-Object { $_ -ne 'builder-account' }))
        if ($problem) { return $problem }
        if ($r.StdOut -notmatch '(?m)^  1  \[done\] Builder account\s*$') { return "no [done] row for step 1: $($r.StdOut)" }
        return ''
    }

    Test-Case 'a step''s REASON and NOTE lines reach prep.log' {
        Clear-Machine
        $r = Invoke-Flow -Arguments ($flowBase + @('-Steps', 'ssh-lan-only,developer-mode')) `
            -Environment @{ PREP_TEST_NOTE = 'ssh-lan-only'; PREP_TEST_FAIL = 'developer-mode' }
        $problem = (Get-ProblemUnlessExit $r 1) + (Get-RecordProblem $r @('ssh-lan-only', 'developer-mode'))
        if ($problem) { return $problem }
        $log = Get-Content -LiteralPath (Join-Path $buildBox 'prep.log') -Raw
        foreach ($want in 'NOTE ssh-lan-only: old scope Any', 'FAIL developer-mode: boom from developer-mode', 'COMPLETE failed=1') {
            if (-not $log.Contains($want)) { return "prep.log lacks '$want': $log" }
        }
        if ($r.StdOut -match '(?m)^(REASON|NOTE) ') { return "the channel lines were printed as they are: $($r.StdOut)" }
        return ''
    }

    Test-Case '-Steps with an unknown id exits 2 naming it' {
        Clear-Machine
        $r = Invoke-Flow -Arguments ($flowBase + @('-Steps', 'openssh,no-such-step'))
        $problem = (Get-ProblemUnlessExit $r 2) + (Get-RecordProblem $r)
        if (-not $problem -and -not $r.StdErr.Contains("'no-such-step'")) { return "stderr does not name the id: $($r.StdErr)" }
        $problem
    }

    Test-Case '-Steps with -Defaults exits 2' {
        Clear-Machine
        $r = Invoke-Flow -Arguments ($flowBase + @('-Steps', 'openssh', '-Defaults'))
        (Get-ProblemUnlessExit $r 2) + (Get-RecordProblem $r)
    }

    Test-Case 'openssh without -PublicKeyFile exits 2 before anything runs' {
        Clear-Machine
        $r = Invoke-Flow -Arguments @('-User', $testUser, '-Steps', 'builder-account,openssh')
        $problem = (Get-ProblemUnlessExit $r 2) + (Get-RecordProblem $r)
        if ($problem) { return $problem }
        if (-not $r.StdErr.Contains('-PublicKeyFile')) { return "stderr does not name -PublicKeyFile: $($r.StdErr)" }
        if (Test-Path -LiteralPath $buildBox) { return 'the refused run created the build-box folder' }
        return ''
    }

    Test-Case 'no console refuses the menu' {
        Clear-Machine
        $r = Invoke-Flow -Arguments $flowBase
        $problem = (Get-ProblemUnlessExit $r 2) + (Get-RecordProblem $r)
        $line = 'prep: no console for the menu; run it at the console, over ssh -t, or with -Defaults or -Steps'
        if (-not $problem -and -not $r.StdErr.Contains($line)) { return "stderr is not the no-console line: $($r.StdErr)" }
        $problem
    }

    Test-Case 'a reboot step stops the run with the line, and the next run shows it done' {
        Clear-Machine
        $r = Invoke-Flow -Arguments ($flowBase + @('-Defaults')) -Environment @{ PREP_TEST_REBOOT = 'wsl-platform' }
        $upTo = @($defaultIds[0..([array]::IndexOf($defaultIds, 'wsl-platform'))])
        $problem = (Get-ProblemUnlessExit $r 0) + (Get-RecordProblem $r $upTo)
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains($rebootLine)) { return "no reboot line: $($r.StdOut)" }
        $next = Invoke-Flow -Arguments $flowBase -Lines @('q')
        if ($next.StdOut -notmatch '(?m)^  7  \[done\] WSL platform\s*$') { return "the next menu does not show step 7 done: $($next.StdOut)" }
        return ''
    }

    Test-Case 'dev-drive refuses a disk with a data volume, naming it and its used bytes' {
        Clear-Machine
        $config = Get-StubConfig @{
            Disks      = @(@{ Number = 3; PartitionStyle = 'GPT'; IsSystem = $false; IsBoot = $false; Size = 2000; FriendlyName = 'test disk' })
            Partitions = @(@{ DiskNumber = 3; PartitionNumber = 2; DriveLetter = 'Q'; Type = 'Basic' })
            Volumes    = @(@{ DriveLetter = 'Q'; FileSystemLabel = 'data'; FileSystemType = 'NTFS'; Size = 1000; SizeRemaining = 400 })
            RootItems  = @{ Q = @('System Volume Information', 'photos') }
        }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'dev-drive') -Parameters @{ DiskNumber = 3 } -Config $config
        $problem = Get-ProblemUnlessExit $r 1
        if ($problem) { return $problem }
        if (-not $r.StdErr.Contains('Q:') -or -not $r.StdErr.Contains('600 bytes used')) {
            return "stderr does not name Q: and its 600 bytes: $($r.StdErr)"
        }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'dev-drive passes a RAW disk under -DryRun and changes nothing' {
        Clear-Machine
        $disk = @{ Number = 3; PartitionStyle = 'RAW'; IsSystem = $false; IsBoot = $false; Size = 2000; FriendlyName = 'test disk' }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'dev-drive') -Parameters @{ DiskNumber = 3; DryRun = $true } `
            -Config (Get-StubConfig @{ Disks = @($disk) })
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains('dev-drive: would initialize disk 3')) { return "no would-initialize line: $($r.StdOut)" }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'dev-drive asks for FORMAT itself and leaves the disk alone without it' {
        Clear-Machine
        $disk = @{ Number = 3; PartitionStyle = 'RAW'; IsSystem = $false; IsBoot = $false; Size = 2000; FriendlyName = 'test disk' }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'dev-drive') -Parameters @{ DiskNumber = 3 } `
            -Config (Get-StubConfig @{ Disks = @($disk); TypedLine = 'nope' })
        $problem = Get-ProblemUnlessExit $r 1
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains('this formats disk 3; type FORMAT to go on')) { return "no FORMAT question: $($r.StdOut)" }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'dev-drive refuses FORMAT on redirected stdin even under GATE_PREP_TEST, before any disk call' {
        Clear-Machine
        $real = Join-Path $root 'real/dev-drive.ps1'
        New-Item -ItemType Directory -Force (Split-Path -Parent $real) | Out-Null
        Set-Content -LiteralPath $real -Value (Get-ScratchText (Join-Path $sourceSteps 'dev-drive.ps1') -KeepConsoleCheck) -NoNewline
        $disk = @{ Number = 3; PartitionStyle = 'RAW'; IsSystem = $false; IsBoot = $false; Size = 2000; FriendlyName = 'test disk' }
        $r = Invoke-Stubbed -Script $real -Parameters @{ DiskNumber = 3 } -Config (Get-StubConfig @{ Disks = @($disk); TypedLine = 'FORMAT' }) `
            -Environment @{ GATE_PREP_TEST = '1' } -Stdin 'FORMAT'
        $problem = Get-ProblemUnlessExit $r 2
        if ($problem) { return $problem }
        if (@($r.Calls | Where-Object { $_ -like 'R Get-Disk*' }).Count) { return "it read the disk before refusing: $($r.Calls -join '; ')" }
        if (-not $r.StdErr.Contains('dev-drive asks you to type FORMAT; run it at the console or over ssh -t')) { return "stderr: $($r.StdErr)" }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'openssh: a failed first start leaves no Set-Service Automatic behind' {
        Clear-Machine
        $stub = Get-StubConfig @{ SshCapability = 'Installed'; SshdStatus = 'Stopped'; SshdStartType = 'Manual'; ThrowOn = @('Start-Service') }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'openssh') -Parameters @{ PublicKeyFile = $keyFile } -Config $stub
        (Get-ProblemUnlessExit $r 1) + (Get-WriteProblem $r -Expect '* Start-Service*' -Forbid '* Set-Service*')
    }

    Test-Case 'openssh: a failed restart restores the backup, starts sshd again and says whether it runs' {
        $config = Join-Path $sshDir 'sshd_config'
        foreach ($throwOn in @(@('Restart-Service'), @('Restart-Service', 'Start-Service'))) {
            Clear-Machine
            $before = Get-Content -LiteralPath $config -Raw
            $stub = Get-StubConfig @{ SshCapability = 'Installed'; SshdStatus = 'Running'; SshdStartType = 'Automatic'; ThrowOn = $throwOn }
            $r = Invoke-Stubbed -Script (Get-StubbedStep 'openssh') -Parameters @{ PublicKeyFile = $keyFile } -Config $stub
            $problem = Get-ProblemUnlessExit $r 1
            if ($problem) { return "with $($throwOn -join ', ') throwing: $problem" }
            if ((Get-Content -LiteralPath $config -Raw) -ne $before) { return "with $($throwOn -join ', ') throwing: sshd_config was not restored" }
            $calls = [string[]]$r.Calls
            $restart = [array]::FindIndex($calls, [Predicate[string]] { param($c) $c -like 'W Restart-Service*' })
            $start = [array]::FindIndex($calls, [Predicate[string]] { param($c) $c -like 'W Start-Service*' })
            if ($restart -lt 0 -or $start -lt $restart) { return "expected Restart-Service, then Start-Service: $($r.Calls -join '; ')" }
            $line = if ($throwOn -contains 'Start-Service') { "openssh: sshd is stopped; start it from the console after fixing $config" }
            else { 'openssh: sshd keeps its previous config' }
            if (-not $r.StdOut.Contains($line)) { return "with $($throwOn -join ', ') throwing, no '$line': $($r.StdOut)" }
        }
        return ''
    }

    Test-Case 'openssh: a failing sshd -t restores this run''s backup and sshd keeps its previous config' {
        Clear-Machine
        $config = Join-Path $sshDir 'sshd_config'
        $before = Get-Content -LiteralPath $config -Raw
        $stub = Get-StubConfig @{ SshCapability = 'Installed'; SshdStatus = 'Running'; SshdStartType = 'Automatic'; SshdTestExit = 1 }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'openssh') -Parameters @{ PublicKeyFile = $keyFile } -Config $stub
        $problem = Get-ProblemUnlessExit $r 1
        if ($problem) { return $problem }
        if ((Get-Content -LiteralPath $config -Raw) -ne $before) { return 'sshd_config was not restored' }
        if (-not (Test-Path -LiteralPath (Join-Path $sshDir 'sshd_config.gate-prep.bak'))) { return 'no pristine sshd_config.gate-prep.bak was kept' }
        if (-not @(Get-ChildItem -LiteralPath $sshDir -Filter 'sshd_config.gate-prep.*.bak').Count) { return 'no per-run backup was made' }
        if (-not $r.StdOut.Contains('sshd keeps its previous config')) { return "no keeps-its-config line: $($r.StdOut)" }
        Get-WriteProblem $r -Forbid '* Start-Service*', '* Restart-Service*', '* Stop-Service*'
    }

    Test-Case 'openssh: a missing sshd_config is copied from the default, host keys made, and sshd starts only after sshd -t' {
        Clear-Machine
        $config = Join-Path $sshDir 'sshd_config'
        Remove-Item -LiteralPath $config
        $keys = Join-Path $sshDir 'administrators_authorized_keys'
        $other = 'ssh-' + 'rsa AAAAB3otherkey other'
        Set-Content -LiteralPath $keys -Value $other -NoNewline
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'openssh') -Parameters @{ PublicKeyFile = $keyFile } `
            -Config (Get-StubConfig @{ SshCapability = 'Installed' })
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        $lines = @(Get-Content -LiteralPath $config)
        if (($lines[0..3] -join '|') -ne ($topSettings -join '|')) { return "sshd_config does not start with the settings: $($lines -join '|')" }
        if ((@(Get-Content -LiteralPath $keys) -join '|') -ne "$other|$testKey") {
            return "the keys file reads: $(Get-Content -LiteralPath $keys -Raw)"
        }
        $keygen = [array]::FindIndex([string[]]$r.Calls, [Predicate[string]] { param($c) $c -like 'W ssh-keygen.exe -A*' })
        $check = [array]::FindIndex([string[]]$r.Calls, [Predicate[string]] { param($c) $c -like 'R sshd.exe -t*' })
        $start = [array]::FindIndex([string[]]$r.Calls, [Predicate[string]] { param($c) $c -like 'W Start-Service*' })
        if ($keygen -lt 0 -or $check -lt $keygen -or $start -lt $check) {
            return "expected ssh-keygen -A, then sshd -t, then Start-Service: $($r.Calls -join '; ')"
        }
        return ''
    }

    Test-Case 'openssh: a key line with options in front exits 2' {
        Clear-Machine
        $optioned = Join-Path $root 'optioned.pub'
        Set-Content -LiteralPath $optioned -Value ('command="x" ' + $testKey)
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'openssh') -Parameters @{ PublicKeyFile = $optioned } `
            -Config (Get-StubConfig @{ SshCapability = 'Installed' })
        $problem = Get-ProblemUnlessExit $r 2
        if (-not $problem -and -not $r.StdErr.Contains('-PublicKeyFile must hold one public key line with no options')) {
            return "stderr: $($r.StdErr)"
        }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }

    Test-Case 'openssh: the settings go first, global lines before Match go, Match blocks and Include stay' {
        $ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $sourceSteps 'openssh.ps1'), [ref]$null, [ref]$null)
        $settingsLine = $ast.Find({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -eq '$settings'
            }, $true)
        . ([scriptblock]::Create($settingsLine.Extent.Text + "`n" + (Get-StepFunctionText 'openssh' 'ConvertTo-SshdConfig')))
        $in = @('Port 22', 'PasswordAuthentication yes', 'Include C:/ssh/conf.d/*.conf', 'ClientAliveInterval 60', 'Match Group administrators',
            '  PasswordAuthentication yes', '  AuthorizedKeysFile k')
        $want = $topSettings + @('Port 22', 'Include C:/ssh/conf.d/*.conf', 'Match Group administrators', '  PasswordAuthentication yes',
            '  AuthorizedKeysFile k')
        $got = @(ConvertTo-SshdConfig $in)
        if (($got -join '|') -ne ($want -join '|')) { return "got $($got -join '|')" }
        if ((@(ConvertTo-SshdConfig $got) -join '|') -ne ($got -join '|')) { return 'a second pass changed the file, so it is never already done' }
        $matchOnly = @('Match User x', 'PasswordAuthentication yes')
        if ((@(ConvertTo-SshdConfig $matchOnly) -join '|') -ne (($topSettings + $matchOnly) -join '|')) { return 'a Match-only setting was touched' }
        return ''
    }

    Test-Case 'build-box-acl: a DACL Set-Acl applied reads back as already done' {
        . ([scriptblock]::Create((Get-StepFunctionText 'build-box-acl' 'Get-BuildBoxAcl', 'Get-AclShape')))
        $folder = Join-Path $root "acl-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
        New-Item -ItemType Directory -Force $folder | Out-Null
        $me = $identity.User.Value
        $want = Get-BuildBoxAcl $me
        # An unelevated test may not give the folder to Administrators, so both sides name this account as owner.
        $want.SetOwner([System.Security.Principal.SecurityIdentifier]::new($me))
        Set-Acl -LiteralPath $folder -AclObject $want
        $have = Get-AclShape (Get-Acl -LiteralPath $folder)
        if ($have -ne (Get-AclShape $want)) { return "the folder reads $have, the step wants $(Get-AclShape $want)" }
        return ''
    }

    Test-Case 'ssh-lan-only: its own rule, the stock rule narrowed, the tailnet group untouched' {
        Clear-Machine
        $rules = @((Get-StubRule 'OpenSSH-Server-In-TCP'),
            (Get-StubRule 'gate-prep-tailnet-ssh' @{ Group = 'gate-prep-tailnet'; RemoteAddress = @('100.64.0.0/255.192.0.0') }),
            (Get-StubRule 'app-any' @{ LocalPort = 'Any'; Program = 'Any' }))
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'ssh-lan-only') -Parameters @{} -Config (Get-StubConfig @{ FirewallRules = $rules }) `
            -Environment @{ SSH_CLIENT = '192.168.1.50 50000 22' }
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        if (-not $r.StdOut.Contains('NOTE ') -or -not $r.StdOut.Contains('app-any')) { return "no NOTE line or no broad-rule warning: $($r.StdOut)" }
        Get-WriteProblem $r -Expect '*New-NetFirewallRule*gate-prep-lan-ssh*Private,Domain*',
        '*Set-NetFirewallRule*OpenSSH-Server-In-TCP*LocalSubnet*Private,Domain*' -Forbid '*gate-prep-tailnet*', '*app-any*'
    }

    Test-Case 'ssh-lan-only refuses a scope that leaves out this ssh session' {
        Clear-Machine
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'ssh-lan-only') -Parameters @{} -Environment @{ SSH_CLIENT = '10.9.9.9 50000 22' }
        $problem = Get-ProblemUnlessExit $r 1
        if (-not $problem -and (-not $r.StdErr.Contains('10.9.9.9') -or -not $r.StdErr.Contains('LocalSubnet'))) { return "stderr: $($r.StdErr)" }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }

    Test-Case 'ssh-tailnet-only: its own rule on the Tailscale interface, the stock rule off, the LAN group untouched' {
        Clear-Machine
        $config = Get-StubConfig @{
            TailscaleState = 'Running'; TailnetIp = '100.101.102.103'
            IPAddresses    = @(@{ IPAddress = '100.101.102.103'; PrefixLength = 32; InterfaceAlias = 'Tailscale' })
            NetProfiles    = @(@{ Name = 'tailnet'; NetworkCategory = 'Private'; InterfaceIndex = 9; InterfaceAlias = 'Tailscale' })
            FirewallRules  = @((Get-StubRule 'OpenSSH-Server-In-TCP'),
                (Get-StubRule 'gate-prep-lan-ssh' @{ Group = 'gate-prep-lan'; RemoteAddress = 'LocalSubnet' }))
        }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'ssh-tailnet-only') -Parameters @{} -Config $config `
            -Environment @{ SSH_CLIENT = '100.100.1.2 50000 22' }
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        Get-WriteProblem $r -Expect '*New-NetFirewallRule*gate-prep-tailnet-ssh*Tailscale*',
        '*Set-NetFirewallRule*OpenSSH-Server-In-TCP*-Enabled False*' -Forbid '*gate-prep-lan*'
    }

    Test-Case 'ssh-tailnet-only refuses while Tailscale is not signed in' {
        Clear-Machine
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'ssh-tailnet-only') -Parameters @{}
        $problem = Get-ProblemUnlessExit $r 1
        if (-not $problem -and -not $r.StdErr.Contains('tailscale is not signed in; sign in first or skip this step')) {
            return "stderr: $($r.StdErr)"
        }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }

    Test-Case 'ssh-tailnet-only: subnets compare parsed, so a /10 equals its dotted mask' {
        . ([scriptblock]::Create((Get-StepFunctionText 'ssh-tailnet-only' 'ConvertTo-Subnet')))
        $same = @(@('100.64.0.0/10', '100.64.0.0/255.192.0.0'), @('fd7a:115c:a1e0::/48', 'fd7a:115c:a1e0:0:0:0:0:0/48'),
            @('100.101.102.103', '100.101.102.103/32'), @('100.64.1.0/10', '100.64.0.0/10'))
        foreach ($pair in $same) {
            if ((ConvertTo-Subnet $pair[0]) -ne (ConvertTo-Subnet $pair[1])) {
                return "$($pair[0]) and $($pair[1]) differ: $(ConvertTo-Subnet $pair[0])"
            }
        }
        if ((ConvertTo-Subnet '100.64.0.0/10') -eq (ConvertTo-Subnet '100.64.0.0/11')) { return '/10 and /11 compare equal' }
        return ''
    }

    Test-Case 'event-log-acl inserts the ACE before the SACL' {
        Clear-Machine
        $sid = $identity.User.Value
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'event-log-acl') -Parameters @{ User = $identity.Name } `
            -Config (Get-StubConfig @{ ChannelAccess = 'O:BAG:SYD:(A;;0x7;;;BA)S:(ML;;0x1;;;LW)' })
        $want = "*wevtutil.exe sl Application /ca:O:BAG:SYD:(A;;0x7;;;BA)(A;;0x2;;;$sid)S:(ML;;0x1;;;LW)"
        (Get-ProblemUnlessExit $r 0) + (Get-WriteProblem $r -Expect $want)
    }

    Test-Case 'never-sleep: a failed SCHEME_MIN is a warning, and standby and hibernate still change' {
        Clear-Machine
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'never-sleep') -Parameters @{} -Config (Get-StubConfig @{ SetactiveExit = 1 })
        $problem = Get-ProblemUnlessExit $r 0
        $line = 'never-sleep: this machine keeps the Balanced plan (Modern Standby); standby and hibernate are still set'
        if (-not $problem -and -not ($r.StdOut + $r.StdErr).Contains($line)) { return "no Modern Standby line: $($r.StdOut) $($r.StdErr)" }
        $problem + (Get-WriteProblem $r -Expect '*powercfg.exe /change standby-timeout-ac 0*', '*powercfg.exe /hibernate off*')
    }

    Test-Case 'never-sleep reads a zero AC index from localized powercfg output as done' {
        Clear-Machine
        $acute = [char]0xE9
        $grave = [char]0xE8
        $config = Get-StubConfig @{
            ActiveScheme = "GUID du mode de gestion : 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c  (Performances ${acute}lev${acute}es)"
            StandbyAc    = @("  Index du param${grave}tre secteur actuel : 0x00000000", "  Index du param${grave}tre batterie actuel : 0x00000384")
            Registry     = @{ HibernateEnabled = 0 }
        }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'never-sleep') -Parameters @{} -Config $config
        $problem = Get-ProblemUnlessExit $r 0
        if (-not $problem -and -not $r.StdOut.Contains('never-sleep: already done')) { return "not already done: $($r.StdOut)" }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }

    Test-Case '-DryRun of the defaults changes nothing and prints each step''s would-do lines' {
        Clear-Machine
        $before = Get-MachineSnapshot
        $parameters = @{ User = $testUser; PublicKeyFile = $keyFile; Defaults = $true; DryRun = $true }
        $r = Invoke-Stubbed -Script (Join-Path $stubDir 'prep-windows.ps1') -Parameters $parameters
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        $silent = @($defaultIds | Where-Object { -not $r.StdOut.Contains("${_}: would ") })
        if ($silent.Count) { return "no would-do line from $($silent -join ', '): $($r.StdOut)" }
        if ((Get-MachineSnapshot) -ne $before) { return 'the dry run changed the scratch machine folder' }
        if ($r.StdOut.Contains($rebootLine)) { return 'the dry run stopped for a reboot' }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'autologon refuses a session with no console' {
        Clear-Machine
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'autologon') -Parameters @{ User = $testUser }
        $problem = Get-ProblemUnlessExit $r 2
        if ($problem) { return $problem }
        $line = 'autologon asks for the password; run it at the console or over ssh -t'
        if (-not $r.StdErr.Contains($line)) { return "stderr is not the no-console line: $($r.StdErr)" }
        Get-WriteProblem $r -Forbid '*'
    }

    $builderSid = 'S-1-5-21-1000-2000-3000-1001'
    Test-Case 'builder-account: an Administrators member is matched by its SID, not its name' {
        Clear-Machine
        $config = Get-StubConfig @{
            LocalUser    = @{ Name = $testUser; SID = $builderSid }
            AdminMembers = @(@{ Name = 'OTHERPC\renamed-account'; SID = $builderSid })
        }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'builder-account') -Parameters @{ User = $testUser } -Config $config
        $problem = Get-ProblemUnlessExit $r 0
        if (-not $problem -and -not $r.StdOut.Contains('builder-account: already done')) { return "not already done: $($r.StdOut)" }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }
    Test-Case 'builder-account: a throwing Get-LocalGroupMember falls back to net localgroup' {
        Clear-Machine
        $netLines = @('Alias name     Administrators', '', 'Members', '', ('-' * 79), 'Administrator', $testUser,
            'The command completed successfully.')
        $config = Get-StubConfig @{ LocalUser = @{ Name = $testUser; SID = $builderSid }; AdminMembersThrow = $true; NetLocalgroup = $netLines }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'builder-account') -Parameters @{ User = $testUser } -Config $config
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        if (-not @($r.Calls | Where-Object { $_ -like 'R net.exe localgroup *' }).Count) {
            return "net localgroup was not read: $($r.Calls -join '; ')"
        }
        if (-not $r.StdOut.Contains('builder-account: already done')) { return "not already done: $($r.StdOut)" }
        Get-WriteProblem $r -Forbid '*'
    }

    $autologonSet = @{ AutoAdminLogon = '1'; DefaultUserName = $testUser; DefaultDomainName = 'GATEPREP' }
    Test-Case 'autologon: Winlogon set and a stored LSA secret is already done' {
        Clear-Machine
        $config = Get-StubConfig @{ Registry = $autologonSet; SecretLength = 16 }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'autologon') -Parameters @{ User = $testUser } -Config $config
        $problem = Get-ProblemUnlessExit $r 0
        if (-not $problem -and -not $r.StdOut.Contains('autologon: already done')) { return "not already done: $($r.StdOut)" }
        $problem + (Get-WriteProblem $r -Forbid '*')
    }
    Test-Case 'autologon: Winlogon set without a stored LSA secret is not done' {
        Clear-Machine
        $config = Get-StubConfig @{ Registry = $autologonSet; SecretLength = 0 }
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'autologon') -Parameters @{ User = $testUser; DryRun = $true } -Config $config
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        if ($r.StdOut.Contains('already done') -or -not $r.StdOut.Contains('autologon: would ask for')) { return "no would-ask line: $($r.StdOut)" }
        Get-WriteProblem $r -Forbid '*'
    }

    Test-Case 'tailscale: after the install it waits for tailscaled, then runs tailscale up' {
        Clear-Machine
        $r = Invoke-Stubbed -Script (Get-StubbedStep 'tailscale') -Parameters @{} -Config (Get-StubConfig @{ TailscaleAnswerOn = 3 })
        $problem = Get-ProblemUnlessExit $r 0
        if ($problem) { return $problem }
        $polls = @($r.Calls | Where-Object { $_ -like 'R tailscale.exe status*' }).Count
        $sleeps = @($r.Calls | Where-Object { $_ -like 'R Start-Sleep*' }).Count
        if ($polls -ne 3 -or $sleeps -ne 2) { return "polled $polls times and slept $sleeps, expected 3 and 2: $($r.Calls -join '; ')" }
        $calls = [string[]]$r.Calls
        $lastPoll = [array]::FindLastIndex($calls, [Predicate[string]] { param($c) $c -like 'R tailscale.exe status*' })
        $up = [array]::FindIndex($calls, [Predicate[string]] { param($c) $c -like 'W tailscale.exe up*' })
        if ($up -lt $lastPoll) { return "tailscale up did not follow the answering poll: $($r.Calls -join '; ')" }
        Get-WriteProblem $r -Expect '*winget install*Tailscale.Tailscale*', '*tailscale.exe up --unattended*'
    }

    Test-Case 'no prep script holds an address, an ssh key or a C:\Users path' {
        $files = @(Get-Item -LiteralPath $sourceMaster) + @(Get-ChildItem -LiteralPath $sourceSteps -File)
        $allowed = '100.64.0.0/10', 'fd7a:115c:a1e0::/48'
        $patterns = @(
            '(?<![\w.~])\d{1,3}(\.\d{1,3}){3}(/\d+)?(?![\w.])',
            '(?i)(?<![\w:])[0-9a-f]{1,4}(:[0-9a-f]{0,4}){2,7}(/\d+)?',
            '\b(ssh-(rsa|ed25519|dss)|ecdsa-sha2-[\w-]+|sk-[\w@.-]+)\s+AAAA',
            '(?i)C:\\Users\\'
        )
        $found = foreach ($file in $files) {
            $text = Get-Content -LiteralPath $file.FullName -Raw
            foreach ($pattern in $patterns) {
                foreach ($match in [regex]::Matches($text, $pattern)) {
                    if ($match.Value -notin $allowed) { "$($file.Name): $($match.Value)" }
                }
            }
        }
        if ($found) { return "found $($found -join '; ')" }
        return ''
    }
}
finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
}

"`n$($script:fails) failure(s)"
exit $script:fails
