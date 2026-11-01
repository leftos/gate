#Requires -Version 7.3
# Installs OpenSSH Server and sets it up for key-only login before it ever starts:
# - -PublicKeyFile must hold one public key line with no options; ssh-keygen -l checks it, and it is appended to
#   administrators_authorized_keys (readable by Administrators and SYSTEM only).
# - A missing sshd_config is copied from sshd_config_default, and ssh-keygen -A makes missing host keys.
# - PasswordAuthentication no, PubkeyAuthentication yes, ClientAliveInterval 15 and ClientAliveCountMax 4 go at the
#   top of sshd_config, and any other global line for them before the first Match goes; Match blocks stay as they are.
# - Each change to sshd_config backs it up to sshd_config.gate-prep.<time>.bak (the first-ever one is also kept as
#   sshd_config.gate-prep.bak). sshd -t must accept the new file before sshd starts or restarts; on a failure the file
#   is restored from this run's backup, a sshd this run started is stopped, and one that was running is started again
#   on its old config. sshd is set to start automatically only after it started, so a failed run keeps its start type.
#   pwsh -File openssh.ps1 -PublicKeyFile <path to the .pub file> [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument.
[CmdletBinding()]
param(
    [string]$PublicKeyFile,
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$changed = $false
$started = $false
$backup = $null
$capabilityName = 'OpenSSH.Server~~~~0.0.1.0'
$sshDir = 'C:\ProgramData\ssh'
$authorized = Join-Path $sshDir 'administrators_authorized_keys'
$config = Join-Path $sshDir 'sshd_config'
$pristine = Join-Path $sshDir 'sshd_config.gate-prep.bak'
$sshd = Join-Path $env:WINDIR 'System32\OpenSSH\sshd.exe'
$sshKeygen = Join-Path $env:WINDIR 'System32\OpenSSH\ssh-keygen.exe'
$defaultConfig = Join-Path $env:WINDIR 'System32\OpenSSH\sshd_config_default'
$settings = [ordered]@{ PasswordAuthentication = 'no'; PubkeyAuthentication = 'yes'; ClientAliveInterval = '15'; ClientAliveCountMax = '4' }
$keyPattern = '^(ssh-ed25519|ssh-rsa|ecdsa-sha2-[\w-]+|sk-[\w@.-]+) [A-Za-z0-9+/]+={0,3}( .*)?$'

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("openssh: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

# Makes the change $What by running $Action, or under -DryRun only says it would.
function Invoke-Change([string]$What, [scriptblock]$Action) {
    $script:changed = $true
    if ($DryRun) {
        Write-Output "openssh: would $What"
        return
    }
    & $Action | Where-Object { $_ -is [string] }
    Write-Output "openssh: $What"
}

# sshd_config's lines with the four settings first and every other global line for them, before the first Match, gone.
function ConvertTo-SshdConfig([string[]]$Lines) {
    $inMatch = $false
    $rest = @(foreach ($line in $Lines) {
            if ($line -match '^\s*Match\s') { $inMatch = $true }
            if (-not $inMatch -and $line -match '^\s*(\w+)[\s=]' -and $settings.Contains($Matches[1])) { continue }
            $line
        })
    return @($settings.Keys | ForEach-Object { "$_ $($settings[$_])" }) + $rest
}

# Whether the keys file is protected and grants only Administrators and SYSTEM.
function Test-KeyFileAcl {
    if (-not (Test-Path -LiteralPath $authorized)) { return $false }
    $acl = Get-Acl -LiteralPath $authorized
    $rules = $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    $sids = @($rules | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique)
    return $acl.AreAccessRulesProtected -and ($sids -join ',') -eq 'S-1-5-18,S-1-5-32-544'
}

# After a failure, starts a sshd that was running before the run and is down now, and says whether it runs.
function Resume-Sshd {
    if ("$((Get-Service -Name sshd).Status)" -ne 'Running') {
        try { Start-Service -Name sshd }
        catch { Write-Warning "openssh: Start-Service sshd failed: $($_.Exception.Message)" }
    }
    if ("$((Get-Service -Name sshd).Status)" -eq 'Running') { return 'openssh: sshd keeps its previous config' }
    return "openssh: sshd is stopped; start it from the console after fixing $config"
}

# After a failure: puts this run's backup and sshd's old start type back, stops a sshd this run started or starts one
# that was running, and exits 1.
function Restore-SshdConfig([string]$Why) {
    if ($backup) { Copy-Item -LiteralPath $backup -Destination $config -Force }
    if ($script:startTypeChanged) { Set-Service -Name sshd -StartupType $oldStartType }
    if ($started) { Stop-Service -Name sshd }
    elseif ($wasRunning) { Write-Output (Resume-Sshd) }
    $restored = if ($backup) { "; restored $config from $backup" } else { '' }
    Exit-Step 1 "$Why$restored"
}

if (-not $PublicKeyFile) { Exit-Step 2 '-PublicKeyFile is required: the PC''s public key file, e.g. -PublicKeyFile .\id_ed25519.pub' }
if (-not (Test-Path -LiteralPath $PublicKeyFile -PathType Leaf)) { Exit-Step 2 "-PublicKeyFile '$PublicKeyFile' is not a file" }
$keyLines = @(Get-Content -LiteralPath $PublicKeyFile | Where-Object { $_.Trim() })
if ($keyLines.Count -ne 1 -or $keyLines[0].Trim() -notmatch $keyPattern) {
    Exit-Step 2 "-PublicKeyFile must hold one public key line with no options; '$PublicKeyFile' does not"
}
$key = $keyLines[0].Trim()
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

if ("$((Get-WindowsCapability -Online -Name $capabilityName).State)" -ne 'Installed') {
    Invoke-Change "install the Windows capability $capabilityName" { Add-WindowsCapability -Online -Name $capabilityName }
}
if (Get-Command -Name $sshKeygen -ErrorAction SilentlyContinue) {
    & $sshKeygen -l -f $PublicKeyFile | Out-Null
    if ($LASTEXITCODE) { Exit-Step 2 "-PublicKeyFile must hold one public key line with no options; ssh-keygen -l rejects '$PublicKeyFile'" }
}
$service = Get-Service -Name sshd -ErrorAction SilentlyContinue
$wasRunning = $service -and "$($service.Status)" -eq 'Running'
$oldStartType = if ($service) { "$($service.StartType)" } else { '' }
$startTypeChanged = $false
if (-not (Test-Path -LiteralPath $config)) {
    Invoke-Change "copy $defaultConfig to $config" {
        New-Item -ItemType Directory -Force -Path $sshDir
        Copy-Item -LiteralPath $defaultConfig -Destination $config
    }
}
if (-not (Test-Path -LiteralPath (Join-Path $sshDir 'ssh_host_ed25519_key'))) {
    Invoke-Change 'make the missing host keys with ssh-keygen -A' {
        & $sshKeygen -A
        if ($LASTEXITCODE) { throw "ssh-keygen -A exited $LASTEXITCODE" }
    }
}
$existing = if (Test-Path -LiteralPath $authorized) { "$(Get-Content -LiteralPath $authorized -Raw)" } else { '' }
if (@($existing -split '\r?\n' | ForEach-Object { $_.Trim() }) -notcontains $key) {
    $prefix = if ($existing -and -not $existing.EndsWith("`n")) { "`r`n" } else { '' }
    Invoke-Change "add the key in $PublicKeyFile to $authorized" { Add-Content -LiteralPath $authorized -Value "$prefix$key" -Encoding ascii }
}
if (-not (Test-KeyFileAcl)) {
    Invoke-Change "limit $authorized to Administrators and SYSTEM" {
        icacls.exe $authorized /inheritance:r /grant:r '*S-1-5-32-544:F' '*S-1-5-18:F' | Out-Null
        if ($LASTEXITCODE) { throw "icacls $authorized exited $LASTEXITCODE" }
        if (-not (Test-KeyFileAcl)) { throw "$authorized grants more than Administrators and SYSTEM; remove the others with icacls /remove" }
    }
}
$source = if (Test-Path -LiteralPath $config) { $config } else { $defaultConfig }
$lines = if (Test-Path -LiteralPath $source) { @(Get-Content -LiteralPath $source) } else { @() }
$updated = ConvertTo-SshdConfig $lines
$configChanged = ($updated -join "`n") -ne ($lines -join "`n")
if ($configChanged) {
    $shown = ($settings.Keys | ForEach-Object { "$_ $($settings[$_])" }) -join ', '
    Invoke-Change "put $shown at the top of $config" {
        $script:backup = Join-Path $sshDir "sshd_config.gate-prep.$(Get-Date -Format 'yyyyMMddHHmmss').bak"
        Copy-Item -LiteralPath $config -Destination $backup
        if (-not (Test-Path -LiteralPath $pristine)) { Copy-Item -LiteralPath $config -Destination $pristine }
        Set-Content -LiteralPath $config -Value $updated -Encoding utf8NoBOM
    }
}
if ($configChanged -or -not $wasRunning) {
    if ($DryRun) {
        $verb = if ($wasRunning) { 'restart' } else { 'start' }
        Write-Output "openssh: would check $config with sshd -t, then $verb sshd"
    }
    else {
        & $sshd -t
        if ($LASTEXITCODE) { Restore-SshdConfig "sshd -t rejected $config (exit $LASTEXITCODE)" }
        # A Disabled service cannot start; Manual lets it, and a failure below puts Disabled back.
        if ($oldStartType -eq 'Disabled') { Set-Service -Name sshd -StartupType Manual; $startTypeChanged = $true }
        try {
            if ($wasRunning) { Restart-Service -Name sshd; Write-Output 'openssh: restarted sshd' }
            else { $started = $true; Start-Service -Name sshd; Write-Output 'openssh: started sshd' }
        }
        catch { Restore-SshdConfig "sshd did not start: $($_.Exception.Message)" }
        $changed = $true
    }
}
# Only once sshd has started, so a failed run leaves the start type as it found it.
if ($oldStartType -ne 'Automatic') {
    Invoke-Change 'set sshd to start automatically' { Set-Service -Name sshd -StartupType Automatic }
}
if (-not $changed) { Write-Output 'openssh: already done' }
exit 0
