# Installs the builder's toolchains, pinned versions from official release URLs, and the gate's work root, keeper and
# git config, with the sccache server and WSL keep-alive tasks. claude-config's provisioning covers the machine and
# comes first: Developer Mode, the event log ACL, sshd, the network, accounts and autologon.
# Runs elevated over ssh, as the builder user named by -User, under Windows PowerShell 5.1, before pwsh exists;
# winget cannot reach its source from an ssh session. Rerunnable: finished downloads are reused.
# -User names the builder user: the scheduled tasks run as it and the write-deny ACLs name it. It is required and
# must resolve to an account on this machine.
# -GateRoot is the gate's work root on the builder, -SccacheDir sccache's disk cache and -SccacheCapBytes its size cap.
# -Only runs just the steps whose names start with one of the given prefixes, then the version summary; an unknown
# argument fails at parameter binding, and a -Only prefix that matches no step exits 2 before anything is written.
# -Only Summary is reserved and matches no step on purpose: it runs only the version summary.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'Only', Justification = 'Invoke-Step reads it')]
[CmdletBinding()]
param(
    [string[]]$Only,
    [string]$User,
    [string]$GateRoot = 'D:\gate-remote',
    [string]$SccacheDir = 'D:\sccache',
    [long]$SccacheCapBytes = 469224652800
)
# powershell -File hands a comma list over as one word.
$Only = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
# A missing -User exits 2 here rather than prompting: an ssh session has no console to answer a prompt.
if (-not $User) {
    [Console]::Error.WriteLine('install-toolchains: -User is required: the builder user the scheduled tasks run as, e.g. -User <name>')
    exit 2
}
try {
    $userSid = (New-Object Security.Principal.NTAccount $User).Translate([Security.Principal.SecurityIdentifier]).Value
}
catch {
    [Console]::Error.WriteLine("install-toolchains: -User '$User' is not an account on this machine: $($_.Exception.Message)")
    exit 2
}
# The toolchains land in the running user's profile and the tasks run as -User, so the two must be one account.
$runningSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
if ($runningSid -ne $userSid) {
    $running = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    [Console]::Error.WriteLine("install-toolchains: running as $running, not -User '$User'; run it as $User over ssh")
    exit 2
}
# Both folders reach a single-quoted cargo and sccache config line and a task's command line, so only a plain local
# path is taken.
foreach ($folder in @{ GateRoot = $GateRoot; SccacheDir = $SccacheDir }.GetEnumerator()) {
    if ($folder.Value -notmatch '^[A-Za-z]:\\[^''"$;]+$') {
        [Console]::Error.WriteLine(
            "install-toolchains: -$($folder.Key) '$($folder.Value)' is not a local path like D:\folder without quotes, `$ or ;")
        exit 2
    }
}
# The step names come from this script's own AST, so a new Invoke-Step needs no second list kept in step. A name that
# is a literal or an expandable string counts, taken unexpanded; any other kind exits 2 here. An unknown parameter
# never reaches here: CmdletBinding fails it at binding. A -Only prefix that matches no step exits 2 before anything
# is created or logged; the reserved prefix Summary skips that check.
$ast = [System.Management.Automation.Language.Parser]::ParseFile($PSCommandPath, [ref]$null, [ref]$null)
$stepNames = @()
foreach ($command in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true)) {
    if ($command.GetCommandName() -ne 'Invoke-Step') { continue }
    $first = if ($command.CommandElements.Count -ge 2) { $command.CommandElements[1] } else { $null }
    if ($first -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
        $stepNames += $first.Value
    }
    elseif ($first -is [System.Management.Automation.Language.ExpandableStringExpressionAst]) {
        $stepNames += $first.Value
    }
    else {
        $line = $command.Extent.StartLineNumber
        [Console]::Error.WriteLine(
            "install-toolchains: Invoke-Step at line $line has a name that is not a string; give it a literal or expandable string")
        exit 2
    }
}
if (-not $stepNames) {
    [Console]::Error.WriteLine("install-toolchains: found no Invoke-Step names in $PSCommandPath")
    exit 2
}
foreach ($prefix in $Only) {
    if ($prefix -eq 'Summary') { continue }
    if (@($stepNames | Where-Object { $_ -like "$prefix*" }).Count -eq 0) {
        [Console]::Error.WriteLine("install-toolchains: -Only '$prefix' matches no step. Steps: $($stepNames -join ', ')")
        exit 2
    }
}
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$log = 'C:\build-box\toolchains.log'
$dl = 'C:\build-box\downloads'
$scriptFiles = 'C:\build-box\gate'
$tools = 'C:\Tools\bin'
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) {
    [Console]::Error.WriteLine('install-toolchains: not elevated; run it from an elevated session (an administrator''s ssh session is one)')
    exit 1
}
New-Item -ItemType Directory -Force $dl, $scriptFiles, $tools | Out-Null
$failed = @()

function Write-InstallLog([string]$Message) {
    $line = '{0:s} {1}' -f (Get-Date), $Message
    Add-Content -Path $log -Value $line
    Write-Output $line
}

function Get-File([string]$Url) {
    $path = Join-Path $dl ([IO.Path]::GetFileName(([Uri]$Url).AbsolutePath))
    if (-not (Test-Path $path)) { Invoke-WebRequest -Uri $Url -OutFile $path -UseBasicParsing }
    return $path
}

function Invoke-Installer([string]$File, [string[]]$Arguments) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -Wait -PassThru -NoNewWindow
    if ($p.ExitCode -notin 0, 3010) { throw "$File exited $($p.ExitCode)" }
}

function Invoke-Step([string]$Name, [scriptblock]$Action) {
    if ($Only -and -not @($Only | Where-Object { $Name -like "$_*" })) { return }
    Write-InstallLog "BEGIN $Name"
    try { & $Action; Write-InstallLog "OK $Name" }
    catch { Write-InstallLog "FAIL $Name : $($_.Exception.Message)"; $script:failed += $Name }
}

# Runs gl-probe.ps1 in a child Windows PowerShell, killed after 60 s; its stdout and stderr go to files, so no stderr
# line reaches this session's error stream. Returns the output lines joined with ' | ' and the child's exit code.
function Get-GlProbeResult([string]$Probe) {
    $outFile = 'C:\build-box\gl-probe.out.log'
    $errFile = 'C:\build-box\gl-probe.err.log'
    $p = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Probe) `
        -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $null = $p.Handle
    if (-not $p.WaitForExit(60000)) {
        $p.Kill()
        return 'timed out after 60 s'
    }
    $code = $p.ExitCode
    $lines = @(Get-Content -Path $outFile) + @(Get-Content -Path $errFile)
    return "$($lines -join ' | ') (exit $code)"
}

$vswhereExe = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"

# A vswhere property of the first Visual Studio instance with the VCTools workload, or $null when vswhere or the instance
# is missing. vs_BuildTools.exe exits 1 when the workload is already installed, so the install step asks this first.
function Get-VCToolsInstanceProperty([string]$Property) {
    if (-not (Test-Path $vswhereExe)) { return $null }
    $lines = @(& $vswhereExe -products * -requires Microsoft.VisualStudio.Workload.VCTools -property $Property)
    if ($LASTEXITCODE) { throw "vswhere -property $Property exited $LASTEXITCODE" }
    return $lines | Where-Object { $_ } | Select-Object -First 1
}

# Parses the Runtimes key's Version text, which reads v14.51.36247.00 there, and returns $null for anything else.
function ConvertTo-VCRedistVersion([string]$Text) {
    if (-not $Text) { return $null }
    $clean = $Text.Trim()
    if ($clean.StartsWith('v') -or $clean.StartsWith('V')) { $clean = $clean.Substring(1) }
    $parsed = $null
    if (-not [version]::TryParse($clean, [ref]$parsed)) { return $null }
    return $parsed
}

# The installed Visual C++ x64 redistributable's version, or $null when the Runtimes key or its version value is absent.
function Get-VCRedistVersion {
    $key = 'HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\X64'
    if (-not (Test-Path -LiteralPath $key)) { return $null }
    $item = Get-ItemProperty -LiteralPath $key -Name 'Version' -ErrorAction SilentlyContinue
    if (-not $item -or -not $item.PSObject.Properties['Version']) { return $null }
    return ConvertTo-VCRedistVersion $item.Version
}

function Sync-SessionPath {
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
}

function Add-MachinePath([string]$Dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($current -split ';') -notcontains $Dir) { [Environment]::SetEnvironmentVariable('Path', "$current;$Dir", 'Machine') }
}

function Remove-MachinePath {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '', Justification = 'unattended script')]
    param([string]$Dir)
    $entries = [Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';'
    if ($entries -contains $Dir) {
        [Environment]::SetEnvironmentVariable('Path', (@($entries | Where-Object { $_ -ne $Dir }) -join ';'), 'Machine')
    }
}

# The builder user is an administrator, so a read-only grant does nothing; an inherited deny of write, append and delete
# stops an accidental write from a routed command. It is not a security boundary: the user can remove it, as a reinstall
# does. Specific rights only: icacls's W and D also carry SYNCHRONIZE, which a launch, a folder listing and a subfolder
# read need on the gate's restricted token, and a denied DELETE on an exe stops it launching. So DELETE alone (DE) is
# denied, on folders only; a file stays deletable by the user, but not writable or replaceable.
function Add-BuilderWriteDeny([string]$Dir) {
    foreach ($entry in "*${userSid}:(OI)(CI)(WD,AD,DC)", "*${userSid}:(CI)(DE)") {
        & icacls.exe $Dir /deny $entry | Out-Null
        if ($LASTEXITCODE) { throw "icacls $Dir /deny $entry exited $LASTEXITCODE" }
    }
}

function Remove-BuilderWriteDeny {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '', Justification = 'unattended script')]
    param([string]$Dir)
    if (-not (Test-Path -LiteralPath $Dir)) { return }
    & icacls.exe $Dir /remove:d "*$userSid" | Out-Null
    if ($LASTEXITCODE) { throw "icacls $Dir /remove:d *$userSid exited $LASTEXITCODE" }
}

$godotRoot = 'C:\godot'
$playwrightDir = 'C:\ms-playwright'
Invoke-Step 'Git 2.56.0.2' {
    $f = Get-File 'https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.2/Git-2.56.0.2-64-bit.exe'
    Invoke-Installer $f @('/VERYSILENT', '/NORESTART', '/NOCANCEL', '/SP-', '/SUPPRESSMSGBOXES')
}
Invoke-Step 'PowerShell 7.6.6' {
    $f = Get-File 'https://github.com/PowerShell/PowerShell/releases/download/v7.6.6/PowerShell-7.6.6-win-x64.msi'
    Invoke-Installer 'msiexec.exe' @('/i', "`"$f`"", '/qn', '/norestart', 'ADD_PATH=1', 'ENABLE_PSREMOTING=0')
}
Invoke-Step '.NET SDK 10.0.401' {
    $f = Get-File 'https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.401/dotnet-sdk-10.0.401-win-x64.exe'
    Invoke-Installer $f @('/install', '/quiet', '/norestart')
}
Invoke-Step '.NET SDK 8.0.425' {
    $f = Get-File 'https://builds.dotnet.microsoft.com/dotnet/Sdk/8.0.425/dotnet-sdk-8.0.425-win-x64.exe'
    Invoke-Installer $f @('/install', '/quiet', '/norestart')
}
# UnrealBuildTool refuses to build an editor target without a .NET Framework SDK of 4.6 or later. The direct URL is where
# Microsoft's fwlink 2088517 redirects; Get-File names the download from the URL path. The installer is Authenticode-signed
# by Microsoft Corporation, and the hash pins the file.
$netFxSdkDir = 'C:\Program Files (x86)\Windows Kits\NETFXSDK\4.8'
Invoke-Step '.NET Framework 4.8 Developer Pack' {
    if (Test-Path -LiteralPath $netFxSdkDir) {
        Write-InstallLog ".NET Framework 4.8 SDK found at $netFxSdkDir; install skipped"
        return
    }
    $sha = 'B37882CDA5610B291B2A984E4C2270F67A448F79D6B1F78337736C95B35C5A7F'
    $f = Get-File 'https://download.microsoft.com/download/6/4/2/642ec242-448b-49a1-8371-5d9c202eaa46/NDP48-DevPack-ENU.exe'
    $got = (Get-FileHash -Path $f -Algorithm SHA256).Hash
    if ($got -ne $sha) { throw "NDP48-DevPack-ENU.exe SHA-256 is $got, expected $sha" }
    Invoke-Installer $f @('/q', '/norestart')
    if (-not (Test-Path -LiteralPath $netFxSdkDir)) { throw "the Developer Pack installed but $netFxSdkDir does not exist" }
}
Invoke-Step '.NET workload wasm-tools' {
    & 'C:\Program Files\dotnet\dotnet.exe' workload install wasm-tools
    if ($LASTEXITCODE) { throw "dotnet workload install wasm-tools exited $LASTEXITCODE" }
}
Invoke-Step 'Node.js 24.20.0' {
    $f = Get-File 'https://nodejs.org/dist/v24.20.0/node-v24.20.0-x64.msi'
    Invoke-Installer 'msiexec.exe' @('/i', "`"$f`"", '/qn', '/norestart')
}
Invoke-Step 'uv 0.12.18 and prek 0.5.0' {
    foreach ($url in 'https://github.com/astral-sh/uv/releases/download/0.12.18/uv-x86_64-pc-windows-msvc.zip',
        'https://github.com/j178/prek/releases/download/v0.5.0/prek-x86_64-pc-windows-msvc.zip') {
        Expand-Archive -Path (Get-File $url) -DestinationPath $tools -Force
    }
    Get-ChildItem $tools -Recurse -Filter *.exe | Where-Object DirectoryName -ne $tools | Move-Item -Destination $tools -Force
    Add-MachinePath $tools
}
# Without a real python on the machine PATH, `python` and `python3` find the Microsoft Store alias stubs in the user's
# WindowsApps, which the machine PATH comes before. The installer makes no python3.exe, so a copy of python.exe is it.
Invoke-Step 'Python 3.14.7' {
    $f = Get-File 'https://www.python.org/ftp/python/3.14.7/python-3.14.7-amd64.exe'
    Invoke-Installer $f @('/quiet', 'InstallAllUsers=1', 'PrependPath=1', 'Include_test=0')
    $dir = Join-Path $env:ProgramFiles 'Python314'
    Copy-Item -Path (Join-Path $dir 'python.exe') -Destination (Join-Path $dir 'python3.exe') -Force
}
# An Unreal 5.8 cook refused the builder's redistributable 14.44.35211 as outdated, and Build Tools installs whatever its
# release carries. Microsoft's aka.ms/vc14 link always serves the latest supported 14.x x64 redistributable. A cached download of
# the same name could be older, so it goes before the fetch. Exit 1638 means a newer one is already installed, which
# satisfies the floor.
$vcRedistFloor = [version]'14.51.36247'
Invoke-Step 'Visual C++ Redistributable 14.51' {
    $have = Get-VCRedistVersion
    if ($null -ne $have -and $have -ge $vcRedistFloor) {
        Write-InstallLog "Visual C++ redistributable $have found; install skipped"
        return
    }
    $cached = Join-Path $dl 'vc_redist.x64.exe'
    if (Test-Path -LiteralPath $cached) { Remove-Item -LiteralPath $cached -Force }
    $f = Get-File 'https://aka.ms/vc14/vc_redist.x64.exe'
    try { Invoke-Installer $f @('/install', '/quiet', '/norestart') }
    catch {
        if ($_.Exception.Message -notmatch 'exited 1638$') { throw }
        Write-InstallLog 'Visual C++ redistributable: the installer reports a newer one already installed'
    }
    $after = Get-VCRedistVersion
    if ($null -eq $after -or $after -lt $vcRedistFloor) {
        $shown = if ($null -eq $after) { 'missing' } else { $after }
        throw "Visual C++ redistributable is $shown after the install; need $vcRedistFloor or later"
    }
}
Invoke-Step 'Visual Studio Build Tools 2022 (C++)' {
    $installed = Get-VCToolsInstanceProperty 'installationPath'
    if ($installed) {
        Write-InstallLog "Visual Studio Build Tools with VCTools found at $installed; install skipped"
        return
    }
    $f = Get-File 'https://aka.ms/vs/17/release/vs_BuildTools.exe'
    Invoke-Installer $f @('--quiet', '--wait', '--norestart', '--nocache', '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended')
}
Invoke-Step 'Rust (stable, 1.98.1, 1.96.1, 1.89.0)' {
    $f = Get-File 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
    Invoke-Installer $f @('-y', '--default-toolchain', 'stable', '--profile', 'minimal', '-c', 'clippy', '-c', 'rustfmt')
    $rustup = Join-Path $env:USERPROFILE '.cargo\bin\rustup.exe'
    foreach ($tc in '1.98.1', '1.96.1', '1.89.0') {
        & $rustup toolchain install $tc --profile minimal -c clippy -c rustfmt
        if ($LASTEXITCODE) { throw "rustup toolchain install $tc exited $LASTEXITCODE" }
    }
    & $rustup target add x86_64-unknown-linux-gnu
    if ($LASTEXITCODE) { throw "rustup target add exited $LASTEXITCODE" }
}
Invoke-Step 'cargo-nextest 0.9.148' {
    $f = Get-File 'https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.148/cargo-nextest-0.9.148-x86_64-pc-windows-msvc.zip'
    Expand-Archive -Path $f -DestinationPath (Join-Path $env:USERPROFILE '.cargo\bin') -Force
}
Invoke-Step 'just 1.58.0' {
    $f = Get-File 'https://github.com/casey/just/releases/download/1.58.0/just-1.58.0-x86_64-pc-windows-msvc.zip'
    $tmp = Join-Path $dl 'just-extract'
    Expand-Archive -Path $f -DestinationPath $tmp -Force
    Copy-Item (Join-Path $tmp 'just.exe') -Destination (Join-Path $env:USERPROFILE '.cargo\bin') -Force
    Remove-Item $tmp -Recurse -Force
}
# A routed command runs under the box's restricted token, with which WSL works only when its non-elevated namespace was
# first made from the builder user's console session: a VM an elevated (ssh) caller started first makes the box's
# wsl.exe time out with Wsl/Service/0x8007274c, and a VM the box cold-starts has no drvfs mounts. This task holds a
# console-session WSL open from the user's logon on, so it has to start before any elevated wsl.exe call that starts
# the VM, and the console needs the user logged on (autologon). The VM then never idles out.
$wslKeepaliveTask = 'gate-wsl-keepalive'
function Start-WslKeepaliveTask {
    $action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\wsl.exe" -Argument '-d Ubuntu -u root -- sleep infinity'
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $User
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
        -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -DontStopOnIdleEnd `
        -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName $wslKeepaliveTask -Action $action -Trigger $trigger -Principal $principal `
        -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName $wslKeepaliveTask
    Start-Sleep -Seconds 3
    $task = Get-ScheduledTask -TaskName $wslKeepaliveTask
    if ($task.State -ne 'Running') {
        $result = ($task | Get-ScheduledTaskInfo).LastTaskResult
        throw ("task $wslKeepaliveTask is $($task.State) after Start-ScheduledTask (last result $result): it runs in $User's " +
            "console session, so log $User on at the console (autologon), then rerun -Only WSL")
    }
    Write-InstallLog "task $wslKeepaliveTask running: wsl.exe -d Ubuntu -u root -- sleep infinity as $User (Interactive, Limited)"
    # --list --running does not start the VM, so it can wait for the task's distro before any elevated call starts it.
    # It exits non-zero while no distro runs, so its exit code is no answer here.
    $deadline = (Get-Date).AddSeconds(120)
    while (@(& wsl.exe --list --running --quiet | ForEach-Object { $_.Trim() }) -notcontains 'Ubuntu') {
        if ((Get-Date) -gt $deadline) {
            throw "task $wslKeepaliveTask is running but Ubuntu was not listed by wsl.exe --list --running within 120 s"
        }
        Start-Sleep -Seconds 2
    }
    Write-InstallLog "Ubuntu running under task $wslKeepaliveTask"
}
# An existing Ubuntu is left alone, so a rerun only refreshes its packages; this step never reboots or shuts WSL down.
# wsl.exe --version, --list and --install --no-launch do not start the VM; the keep-alive task does, before the bash call.
Invoke-Step 'WSL Ubuntu (Linux build deps, Rust 1.96.1)' {
    $env:WSL_UTF8 = '1'
    $missing = 'wsl.exe is not installed; the box''s provisioning installs the WSL platform, then the box must reboot'
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { throw $missing }
    & wsl.exe --version
    if ($LASTEXITCODE) { throw $missing }
    $distros = @(& wsl.exe --list --quiet | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($distros -notcontains 'Ubuntu') {
        & wsl.exe --install Ubuntu --no-launch
        if ($LASTEXITCODE) { throw "wsl.exe --install Ubuntu --no-launch exited $LASTEXITCODE" }
    }
    Start-WslKeepaliveTask
    $setup = Join-Path $PSScriptRoot 'wsl-setup.sh'
    if (-not (Test-Path -LiteralPath $setup)) { throw "$setup is missing; copy it to the builder beside install-toolchains.ps1" }
    $full = [IO.Path]::GetFullPath($setup)
    $unix = '/mnt/' + $full.Substring(0, 1).ToLowerInvariant() + ($full.Substring(2) -replace '\\', '/')
    & wsl.exe -d Ubuntu -u root -e bash $unix
    if ($LASTEXITCODE) { throw "wsl.exe -d Ubuntu -u root -e bash $unix exited $LASTEXITCODE" }
}
# The spellings of the keys build and rustc-wrapper in a cargo config.toml: bare, basic-quoted or literal-quoted.
$cargoBuildKey = '(?:build|"build"|''build'')'
$cargoWrapperKey = '(?:rustc-wrapper|"rustc-wrapper"|''rustc-wrapper'')'
# Records a line of the build table: the table ends after it, and it is the rustc-wrapper line when it matches $Pattern.
function Add-CargoBuildLine([hashtable]$At, [int]$Index, [string]$Line, [string]$Pattern) {
    $At.End = $Index + 1
    if ($Line -notmatch $Pattern) { return }
    if ($At.Wrapper -ge 0) { throw "cargo config.toml sets build.rustc-wrapper twice; line $($Index + 1): $Line" }
    $At.Wrapper = $Index
}
# Finds the build table of a cargo config.toml's lines, as a [build] table or as top-level build.<key> dotted keys: the
# index of its rustc-wrapper line, the index after its last key line (comments and blanks before the next table stay
# outside it), and which form it has. Throws on a form the merge does not rewrite, naming the line.
function Find-CargoBuildWrapper([string[]]$Lines) {
    $at = @{ Wrapper = -1; End = -1; Table = $false; Dotted = $false }
    $section = ''
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        $line = $Lines[$i]
        if ($line -match '^\s*\[\s*([^\]]*?)\s*\]') {
            $section = $Matches[1].Trim('"', "'")
            if ($section -eq 'build') { $at.Table = $true; $at.End = $i + 1 }
        }
        elseif ($section -eq '' -and $line -match "^\s*$cargoBuildKey\s*=") {
            throw "cargo config.toml line $($i + 1) sets build as an inline table, which this step does not rewrite: $line"
        }
        elseif ($section -eq '' -and $line -match "^\s*$cargoBuildKey\s*\.") {
            $at.Dotted = $true
            Add-CargoBuildLine $at $i $line "^\s*$cargoBuildKey\s*\.\s*$cargoWrapperKey\s*="
        }
        elseif ($section -eq 'build' -and $line -match '^\s*[^#\s]') { Add-CargoBuildLine $at $i $line "^\s*$cargoWrapperKey\s*=" }
    }
    if ($at.Table -and $at.Dotted) { throw 'cargo config.toml has both a [build] table and top-level build.<key> keys' }
    return $at
}
# Returns the lines of a cargo config.toml with build.rustc-wrapper set to $Wrapper, every other line kept: an existing
# rustc-wrapper line is replaced, else the key goes after the build table's last key, else a [build] table is appended.
function Merge-CargoBuildWrapper([string[]]$Lines, [string]$Wrapper) {
    $at = Find-CargoBuildWrapper $Lines
    $entry = if ($at.Dotted) { "build.rustc-wrapper = '$Wrapper'" } else { "rustc-wrapper = '$Wrapper'" }
    $merged = New-Object System.Collections.Generic.List[string]
    $merged.AddRange($Lines)
    if ($at.Wrapper -ge 0) { $merged[$at.Wrapper] = $entry }
    elseif ($at.End -ge 0) { $merged.Insert($at.End, $entry) }
    else {
        if ($merged.Count) { $merged.Add('') }
        $merged.AddRange([string[]]@('[build]', $entry))
    }
    return , $merged.ToArray()
}
# Rust builds share compiled dependencies across the gate's fresh work folders through sccache's disk cache, in the
# -SccacheDir folder.
# The sccache server runs in a task of its own, so no gate run's job owns it: a client in a routed run would otherwise start
# the server inside that run's job, and the job's end would kill it under every concurrent run using it.
$sccacheTaskPath = '\build-box\'
$sccacheTaskName = 'sccache-server'
$pruneTaskName = 'gate-prune-work'
$sccacheServerScript = Join-Path $scriptFiles 'sccache-server.ps1'
$sccacheUserVariables = 'SCCACHE_IGNORE_SERVER_IO_ERROR', 'SCCACHE_CLIENT_SIDE'
function Get-SccacheServerTask { return Get-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $sccacheTaskName -ErrorAction SilentlyContinue }
function Get-SccacheProcess([string]$Exe) { return @(Get-Process sccache -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe }) }
# Stops the server task and every server of $Exe, so the exe can be replaced and the task's server can take the port.
function Stop-SccacheServer([string]$Exe) {
    if (Get-SccacheServerTask) { Stop-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $sccacheTaskName }
    if (-not (Test-Path $Exe)) { return }
    # It exits non-zero when no server runs; cmd keeps its stderr out of this session's error stream.
    & cmd.exe /c "`"$Exe`" --stop-server >nul 2>&1"
    for ($try = 1; $try -le 10 -and (Get-SccacheProcess $Exe); $try++) { Start-Sleep -Seconds 1 }
    $left = Get-SccacheProcess $Exe
    if ($left) { throw "sccache pid $(($left.Id) -join ',') still running 10 s after --stop-server; end it and rerun -Only sccache" }
}
# Registers and starts the task that keeps sccache's server running in the foreground for as long as the box is up.
function Start-SccacheServerTask([string]$Exe, [string]$Log) {
    # "SCCACHE_IDLE_TIMEOUT how long the local daemon process waits for more client requests before exiting, in seconds. Set
    # to 0 to run sccache permanently" and "SCCACHE_NO_DAEMON set to 1 to disable putting the server to the background"
    # (docs/Configuration.md). The README's foreground form is "SCCACHE_START_SERVER=1 SCCACHE_NO_DAEMON=1 sccache";
    # "sccache --start-server" alone would background the server and end the task, which then no longer tracks it.
    # The loop restarts a server that exits, so a gate run's client finds one rather than starting its own in its job.
    $wrapper = @(
        '# Written by install-toolchains.ps1: runs the sccache server in the foreground, never idling out, restarting it when it exits.'
        "`$env:SCCACHE_IDLE_TIMEOUT = '0'"
        "`$env:SCCACHE_START_SERVER = '1'"
        "`$env:SCCACHE_NO_DAEMON = '1'"
        "`$env:SCCACHE_CLIENT_SIDE = '1'"
        'while ($true) {'
        "    & '$Exe'"
        "    Add-Content -LiteralPath '$Log' -Value ('{0:s} sccache server exited {1}; restarting in 2 s' -f (Get-Date), `$LASTEXITCODE)"
        '    Start-Sleep -Seconds 2'
        '}'
    ) -join "`n"
    [IO.File]::WriteAllText($sccacheServerScript, $wrapper + "`n", (New-Object Text.UTF8Encoding $false))
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File $sccacheServerScript"
    $action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument $arguments
    # S4U runs the task as the builder user whether or not it is logged on, with no stored password; it reaches local
    # resources only, which is all the server needs. The logon trigger restarts a server that died before a logon.
    $triggers = @((New-ScheduledTaskTrigger -AtStartup), (New-ScheduledTaskTrigger -AtLogOn -User $User))
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType S4U -RunLevel Limited
    # Task Scheduler's restart interval is at least a minute; it covers the wrapper itself dying.
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -DontStopOnIdleEnd -MultipleInstances IgnoreNew `
        -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $sccacheTaskName -Action $action -Trigger $triggers -Principal $principal `
        -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $sccacheTaskName
    Start-Sleep -Seconds 3
    $task = Get-SccacheServerTask
    if ($task.State -ne 'Running') {
        $result = ($task | Get-ScheduledTaskInfo).LastTaskResult
        throw "task $sccacheTaskPath$sccacheTaskName is $($task.State) after Start-ScheduledTask (last result $result)"
    }
    $ids = @(Get-SccacheProcess $Exe | ForEach-Object { $_.Id }) -join ','
    if (-not $ids) { throw "task $sccacheTaskPath$sccacheTaskName is running but no $Exe process is; see $Log" }
    Write-InstallLog "task $sccacheTaskPath$sccacheTaskName running: $sccacheServerScript as $User (S4U, Limited); sccache pid $ids"
}
Invoke-Step 'sccache 0.18.0' {
    # The task runs as the builder user, who cannot write C:\build-box, so its log sits in that user's own profile; -User
    # is the running account, so this LOCALAPPDATA is its. Set here, an unset LOCALAPPDATA fails only this step, and
    # first, so a refused path fails before the server is stopped or cargo is pointed at sccache.
    $sccacheServerLog = Join-Path $env:LOCALAPPDATA 'build-box\sccache-server.log'
    if ($sccacheServerLog -match "['\[\]\u2018-\u201B]") {
        $problem = 'the path holds a quote or bracket, which the wrapper script cannot carry; set a plain LOCALAPPDATA'
        throw "sccache server log ${sccacheServerLog}: $problem"
    }
    $exe = Join-Path $tools 'sccache.exe'
    # A routed run whose client finds no server while this step runs starts one inside its own job: rerun only while
    # the gate is idle.
    Stop-SccacheServer $exe
    $have = if (Test-Path $exe) { (& $exe --version 2>&1) -join ' ' } else { '' }
    if ($have -eq 'sccache 0.18.0') {
        Write-InstallLog 'sccache 0.18.0 already installed'
    }
    else {
        # From https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-x86_64-pc-windows-msvc.zip.sha256
        $sha = '8965C74D5E8A225244F741E18AD2F3F504F48228DC1BAC948FC22761A348363D'
        $f = Get-File 'https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-x86_64-pc-windows-msvc.zip'
        $got = (Get-FileHash -Path $f -Algorithm SHA256).Hash
        if ($got -ne $sha) { throw "sccache-v0.18.0-x86_64-pc-windows-msvc.zip SHA-256 is $got, expected $sha" }
        $tmp = Join-Path $dl 'sccache-extract'
        Expand-Archive -Path $f -DestinationPath $tmp -Force
        $found = Get-ChildItem $tmp -Recurse -Filter sccache.exe | Select-Object -First 1
        if (-not $found) { throw "sccache.exe is not in $f" }
        Copy-Item $found.FullName -Destination $exe -Force
        Remove-Item $tmp -Recurse -Force
    }
    $cargoConfig = Join-Path $env:USERPROFILE '.cargo\config.toml'
    [string[]]$lines = @()
    if (Test-Path -LiteralPath $cargoConfig) { $lines = [IO.File]::ReadAllLines($cargoConfig) }
    $merged = Merge-CargoBuildWrapper $lines $exe
    New-Item -ItemType Directory -Force (Split-Path $cargoConfig) | Out-Null
    [IO.File]::WriteAllText($cargoConfig, (($merged -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))
    Write-InstallLog "$cargoConfig [build] rustc-wrapper -> $exe"
    New-Item -ItemType Directory -Force $sccacheDir | Out-Null
    # sccache reads %APPDATA%\Mozilla\sccache\config\config on Windows when SCCACHE_CONF is unset
    # (https://github.com/mozilla/sccache/blob/v0.18.0/docs/Configuration.md). The cap is -SccacheCapBytes; its default
    # is about a quarter of the cache drive's free space when it was set.
    $sccacheConfig = Join-Path $env:APPDATA 'Mozilla\sccache\config\config'
    New-Item -ItemType Directory -Force (Split-Path $sccacheConfig) | Out-Null
    $toml = "[cache.disk]`ndir = '$sccacheDir'`nsize = $SccacheCapBytes`n"
    [IO.File]::WriteAllText($sccacheConfig, $toml, (New-Object Text.UTF8Encoding $false))
    Write-InstallLog "$sccacheConfig -> dir $sccacheDir, size $SccacheCapBytes bytes"
    # "By default, sccache will fail your build if it fails to successfully communicate with its associated server. To have
    # sccache instead gracefully failover to the local compiler without stopping, set the environment variable
    # SCCACHE_IGNORE_SERVER_IO_ERROR=1." (https://github.com/mozilla/sccache/blob/v0.18.0/README.md)
    # "SCCACHE_CLIENT_SIDE set to 1 to run the compile in the client process and use the daemon only as a gateway to the
    # cache storage" (docs/Configuration.md): rustc and a repo's proc macros then run inside the gate run's job and token.
    foreach ($name in $sccacheUserVariables) {
        [Environment]::SetEnvironmentVariable($name, '1', 'User')
        $value = [Environment]::GetEnvironmentVariable($name, 'User')
        if ($value -ne '1') { throw "user variable $name reads '$value' after setting it; expected 1" }
        Write-InstallLog "user $name -> 1"
    }
    New-Item -ItemType Directory -Force (Split-Path -Parent $sccacheServerLog) | Out-Null
    Start-SccacheServerTask -Exe $exe -Log $sccacheServerLog
}
# just runs recipe lines through sh, and recipes call bash, wc and tr. The user Path reaches new ssh sessions without an
# sshd restart, which a machine Path change needs; appended last, so the System32 names still win.
Invoke-Step 'Git usr\bin on the builder user Path' {
    $dir = 'C:\Program Files\Git\usr\bin'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) { [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User') }
}
Invoke-Step 'Bun 1.4.2' {
    # From https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/SHASUMS256.txt
    $sha = 'CE4C17497B2F29712A99D3D53F028DE28CD42E3BACB8589599E7F000E49B6405'
    $f = Get-File 'https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-windows-x64.zip'
    $got = (Get-FileHash -Path $f -Algorithm SHA256).Hash
    if ($got -ne $sha) { throw "bun-windows-x64.zip SHA-256 is $got, expected $sha" }
    $tmp = Join-Path $dl 'bun-extract'
    Expand-Archive -Path $f -DestinationPath $tmp -Force
    $exe = Get-ChildItem $tmp -Recurse -Filter bun.exe | Select-Object -First 1
    Copy-Item $exe.FullName -Destination (Join-Path $tools 'bun.exe') -Force
    Copy-Item $exe.FullName -Destination (Join-Path $tools 'bunx.exe') -Force
    Remove-Item $tmp -Recurse -Force
}
# The gate runs a repo's pinned Godot from GATE_GODOT_ROOT\<x.y.z>\Godot_console.exe, one folder per version, and refuses
# a root, version folder or exe that is a reparse point. The console exe starts its sibling Godot.exe by name.
$godotVersionDir = Join-Path $godotRoot '4.7.2'
Invoke-Step 'Godot 4.7.2 mono' {
    # From https://github.com/godotengine/godot-builds/releases/download/4.7.2-stable/SHA512-SUMS.txt
    $sha = '79229FD112B0C9CBEAB82363A4EF7BE18EA70F1CAF86BF912789335B136FBE7E01DB0053A33461438C5DA1C680C17BBB10040BD09BEDC51221CD4423D0367757'
    $f = Get-File 'https://github.com/godotengine/godot-builds/releases/download/4.7.2-stable/Godot_v4.7.2-stable_mono_win64.zip'
    $got = (Get-FileHash -Path $f -Algorithm SHA512).Hash
    if ($got -ne $sha) { throw "Godot_v4.7.2-stable_mono_win64.zip SHA-512 is $got, expected $sha" }
    $tmp = Join-Path $dl 'godot-extract'
    Expand-Archive -Path $f -DestinationPath $tmp -Force
    $src = Get-ChildItem $tmp -Directory | Select-Object -First 1
    Remove-BuilderWriteDeny $godotRoot
    New-Item -ItemType Directory -Force $godotVersionDir | Out-Null
    Copy-Item -Path (Join-Path $src.FullName '*') -Destination $godotVersionDir -Recurse -Force -Exclude 'Godot_v*.exe'
    $exes = Get-ChildItem $src.FullName -Filter 'Godot_v*.exe'
    Copy-Item -Path ($exes | Where-Object Name -Like '*_console.exe').FullName -Destination (Join-Path $godotVersionDir 'Godot_console.exe') -Force
    Copy-Item -Path ($exes | Where-Object Name -NotLike '*_console.exe').FullName -Destination (Join-Path $godotVersionDir 'Godot.exe') -Force
    # The earlier layout put one install flat in the root; only the names it wrote there are deleted.
    $flat = @(Get-ChildItem $src.FullName | Where-Object Name -NotLike 'Godot_v*.exe' | ForEach-Object Name) + @('Godot.exe', 'Godot_console.exe')
    foreach ($name in $flat) {
        $path = Join-Path $godotRoot $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
    }
    Remove-Item $tmp -Recurse -Force
    Remove-MachinePath 'C:\Godot'
    $env:GATE_GODOT_ROOT = $godotRoot
    [Environment]::SetEnvironmentVariable('GATE_GODOT_ROOT', $godotRoot, 'Machine')
    foreach ($item in $godotRoot, $godotVersionDir, (Join-Path $godotVersionDir 'Godot_console.exe')) {
        if ((Get-Item -LiteralPath $item -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "$item is a reparse point; the gate refuses a Godot install that is one"
        }
    }
    Add-BuilderWriteDeny $godotRoot
}
Invoke-Step 'FFmpeg 9.0.2' {
    $sha = '759D0A9831C436A0EB331AD36F236C06CB04AAA0005DA46571F0E9D3D9206F6B'
    $f = Get-File 'https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-full_build.zip'
    $got = (Get-FileHash -Path $f -Algorithm SHA256).Hash
    if ($got -ne $sha) { throw "ffmpeg-9.0.2-full_build.zip SHA-256 is $got, expected $sha" }
    $tmp = Join-Path $dl 'ffmpeg-extract'
    Expand-Archive -Path $f -DestinationPath $tmp -Force
    $src = Get-ChildItem $tmp -Directory | Select-Object -First 1
    foreach ($exe in 'ffmpeg.exe', 'ffprobe.exe') { Copy-Item (Join-Path $src.FullName "bin\$exe") -Destination $tools -Force }
    Remove-Item $tmp -Recurse -Force
}
Sync-SessionPath
Invoke-Step 'pnpm 12.5.1' {
    & npm.cmd install -g pnpm@12.5.1
    if ($LASTEXITCODE) { throw "npm install -g pnpm exited $LASTEXITCODE" }
}
# A routed repo pins @playwright/test 1.63.0; its browsers live in one machine-wide folder every user's process finds.
Invoke-Step 'Playwright 1.63.0 browsers' {
    Sync-SessionPath
    Remove-BuilderWriteDeny $playwrightDir
    New-Item -ItemType Directory -Force $playwrightDir | Out-Null
    $env:PLAYWRIGHT_BROWSERS_PATH = $playwrightDir
    [Environment]::SetEnvironmentVariable('PLAYWRIGHT_BROWSERS_PATH', $playwrightDir, 'Machine')
    $dryRun = @(& npx.cmd -y playwright@1.63.0 install --dry-run chromium webkit)
    if ($LASTEXITCODE) { throw "npx playwright@1.63.0 install --dry-run exited $LASTEXITCODE" }
    $locations = @($dryRun | ForEach-Object { if ($_ -match '^\s*Install location:\s+(\S.*?)\s*$') { $Matches[1] } })
    if (-not $locations) { throw "npx playwright@1.63.0 install --dry-run printed no install location: $($dryRun -join ' | ')" }
    if (@($locations | Where-Object { -not (Test-Path -LiteralPath $_) })) {
        & npx.cmd -y playwright@1.63.0 install chromium webkit
        if ($LASTEXITCODE) { throw "npx playwright@1.63.0 install chromium webkit exited $LASTEXITCODE" }
    }
    else {
        Write-InstallLog "Playwright 1.63.0 browsers found at $($locations -join ', '); install skipped"
    }
    Add-BuilderWriteDeny $playwrightDir
}

$gitSystemConfig = [ordered]@{ 'core.autocrlf' = 'false'; 'core.longpaths' = 'true'; 'gc.pruneExpire' = 'never' }
$gateWorkSafeDir = ($GateRoot.TrimEnd('\') -replace '\\', '/') + '/work/*'
Invoke-Step 'Git system config for gate runs' {
    foreach ($key in $gitSystemConfig.Keys) {
        & git config --system $key $gitSystemConfig[$key]
        if ($LASTEXITCODE) { throw "git config --system $key exited $LASTEXITCODE" }
    }
    # The gate's work folders are owned by Administrators (the gate runs elevated) while git runs as the builder user.
    if (@(& git config --system --get-all safe.directory) -notcontains $gateWorkSafeDir) {
        & git config --system --add safe.directory $gateWorkSafeDir
        if ($LASTEXITCODE) { throw "git config --system --add safe.directory exited $LASTEXITCODE" }
    }
}
# An sh.exe killed in the first moments of its start leaves the builder's shared MSYS section locked for every later one,
# unless an MSYS process from before stays alive. Rerunning the step restarts the task, as a Git for Windows update needs.
$keeperTask = 'build-box-msys-keeper'
$keeperExe = 'C:\Program Files\Git\usr\bin\sleep.exe'
Invoke-Step "keeper: MSYS sleep task $keeperTask" {
    if (-not (Test-Path -LiteralPath $keeperExe)) { throw "$keeperExe does not exist; install Git for Windows first" }
    $action = New-ScheduledTaskAction -Execute $keeperExe -Argument 'infinity'
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType S4U -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -DontStopOnIdleEnd
    Register-ScheduledTask -TaskName $keeperTask -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force |
        Out-Null
    Stop-ScheduledTask -TaskName $keeperTask
    for ($try = 1; $try -le 10 -and (Get-ScheduledTask -TaskName $keeperTask).State -eq 'Running'; $try++) { Start-Sleep -Seconds 1 }
    Start-ScheduledTask -TaskName $keeperTask
    Start-Sleep -Seconds 2
    $task = Get-ScheduledTask -TaskName $keeperTask
    if ($task.State -ne 'Running') {
        $result = ($task | Get-ScheduledTaskInfo).LastTaskResult
        throw "task $keeperTask is $($task.State) after Start-ScheduledTask (last result $result)"
    }
    Write-InstallLog "task $keeperTask running: $keeperExe infinity as $User (S4U, Limited)"
}
# Throws unless $Path is a plain folder: New-Item -Force returns an existing junction or symbolic link, and icacls and
# Get-Acl would then act on its target.
function Assert-GateRootNotLink([string]$Path) {
    $attributes = (Get-Item -LiteralPath $Path -Force).Attributes
    if ($attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "gate work root ${Path}: it is a link (reparse point), not a folder; remove the link and rerun -Only ""Gate work root"""
    }
    if (-not ($attributes -band [IO.FileAttributes]::Directory)) {
        throw "gate work root ${Path}: it is a file, not a folder; remove it and rerun -Only ""Gate work root"""
    }
}
# One ACE of an SDDL, its text without parentheses as $Body: Normal is it with the SID aliases SY, BA, AU and BU written
# as SIDs, Fix the way to remove it when it does not belong.
function Get-GateRootAce([string]$Body, [string]$Retry) {
    $aliases = @{ SY = 'S-1-5-18'; BA = 'S-1-5-32-544'; AU = 'S-1-5-11'; BU = 'S-1-5-32-545' }
    $fields = $Body -split ';'
    $sid = if ($fields.Count -eq 6) { $fields[5] } else { '' }
    if ($aliases.ContainsKey($sid)) { $sid = $aliases[$sid] }
    $fix = if ($fields[1] -match 'ID') { "it is inherited; $Retry, whose icacls /inheritance:r drops it" }
    elseif ($fields[0] -eq 'D') { "remove it with icacls on the root, /remove:d *$sid, and $Retry" }
    else { "remove it with icacls on the root, /remove:g *$sid, and $Retry" }
    return [pscustomobject]@{ Normal = (@($fields[0..4]) + $sid) -join ';'; Fix = $fix }
}
# Throws when icacls's listing of the root shows a mandatory label.
function Assert-GateRootNoLabel([string[]]$IcaclsLines, [string]$Retry) {
    $label = @($IcaclsLines | Where-Object { $_ -like '*Mandatory Label\*' })
    if ($label.Count) {
        $shown = $label[0].Trim()
        throw "icacls shows a mandatory label on it, '$shown', which the root must not carry; clear the label (the ML ACE in its SACL) and $Retry"
    }
}
# The gate work root's security: Administrators as owner, a protected DACL of exactly SYSTEM, Administrators and the builder user $UserSid, each
# full control inherited by objects and containers, and no mandatory label. $Sddl is Get-Acl's SDDL for the root and
# $IcaclsLines icacls's listing of it, the only one of the two that shows a label. Throws naming what it found and the fix.
function Assert-GateRootSecurity([string]$Sddl, [string[]]$IcaclsLines, [string]$UserSid) {
    $retry = 'rerun -Only "Gate work root"'
    # An owner can always rewrite the DACL, so only Administrators may own the root.
    $owner = [regex]::Match($Sddl, '^O:(S-1(?:-\d+)+|[A-Z]{2})').Groups[1].Value
    if ($owner -notin 'BA', 'S-1-5-32-544') { throw "its owner is $owner, not Administrators; $retry" }
    $dacl =[regex]::Match($Sddl, 'D:(?<flags>[A-Z]*)(?<aces>(?:\([^()]*\))*)(?:S:|$)')
    if (-not $dacl.Success) { throw "its security descriptor $Sddl holds no DACL this check can read; $retry" }
    $flags = $dacl.Groups['flags'].Value
    if ($flags -notmatch 'P') {
        throw "its DACL is not protected (flags '$flags'), so it takes its parent's ACEs; $retry, whose icacls /inheritance:r protects it"
    }
    $wanted = @('A;OICI;FA;;;S-1-5-18', 'A;OICI;FA;;;S-1-5-32-544', "A;OICI;FA;;;$UserSid")
    $found = @()
    foreach ($match in [regex]::Matches($dacl.Groups['aces'].Value, '\(([^()]*)\)')) {
        $ace = Get-GateRootAce -Body $match.Groups[1].Value -Retry $retry
        if ($ace.Normal -notin $wanted) {
            throw "its DACL holds $($match.Value), which is not SYSTEM, Administrators or $UserSid full control; $($ace.Fix)"
        }
        $found += $ace.Normal
    }
    foreach ($ace in $wanted) {
        if ($ace -notin $found) { throw "its DACL lacks ($ace), full control for $($ace.Split(';')[-1]); $retry, whose icacls /grant:r adds it" }
    }
    Assert-GateRootNoLabel -IcaclsLines $IcaclsLines -Retry $retry
}
# Reads the root's security with Get-Acl and icacls and returns what Assert-GateRootSecurity finds wrong, or ''.
function Get-GateRootSecurityProblem([string]$Root, [string]$UserSid) {
    $sddl = (Get-Acl -LiteralPath $Root).Sddl
    $icaclsLines = @(& icacls.exe $Root /L)
    if ($LASTEXITCODE) { throw "icacls $Root exited $LASTEXITCODE reading its security" }
    try {
        Assert-GateRootSecurity -Sddl $sddl -IcaclsLines $icaclsLines -UserSid $UserSid
        return ''
    }
    catch {
        return $_.Exception.Message
    }
}
# The root's owner, DACL and label are set only when the check finds them wrong, so a rerun leaves a correct tree alone.
Invoke-Step "Gate work root $GateRoot" {
    $drive = Split-Path -Qualifier $GateRoot
    if (-not (Test-Path "$drive\")) { throw "drive $drive does not exist; the gate work root $GateRoot belongs on a Dev Drive" }
    New-Item -ItemType Directory -Force $GateRoot | Out-Null
    Assert-GateRootNotLink -Path $GateRoot
    $before = Get-GateRootSecurityProblem -Root $GateRoot -UserSid $userSid
    if (-not $before) {
        Write-InstallLog "gate work root ${GateRoot}: already protected"
        return
    }
    Write-InstallLog "gate work root ${GateRoot}: $before; setting its owner and DACL"
    # /L: icacls acts on a link itself, never its target, and the read-back below refuses a link swapped in meanwhile.
    Assert-GateRootNotLink -Path $GateRoot
    & icacls.exe $GateRoot /L /setowner '*S-1-5-32-544' /q | Out-Null
    if ($LASTEXITCODE) { throw "icacls $GateRoot /setowner exited $LASTEXITCODE" }
    # No /t: the root's children take the change by inheritance, and its protected children keep their own security.
    & icacls.exe $GateRoot /L /inheritance:r /grant:r'*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${userSid}:(OI)(CI)F" /q | Out-Null
    if ($LASTEXITCODE) { throw "icacls $GateRoot /inheritance:r /grant:r exited $LASTEXITCODE" }
    Assert-GateRootNotLink -Path $GateRoot
    $after = Get-GateRootSecurityProblem -Root $GateRoot -UserSid $userSid
    if ($after) { throw "gate work root ${GateRoot}: $after" }
    Write-InstallLog "gate work root ${GateRoot}: protected DACL SYSTEM, Administrators, $User full control; no label"
}
# Every 6 hours a task runs gate box prune on the work root and writes its lines to the Application event log. The whole
# command sits in the task's own action, which only an administrator can change, so no script file backs it. The task is
# not started here: gate.exe arrives with the gate push that follows this installer.
Invoke-Step 'Work-folder prune task' {
    $eventSource = 'gate-prune'
    if ([Diagnostics.EventLog]::SourceExists($eventSource)) {
        Write-InstallLog "event source ${eventSource}: already registered"
    }
    else {
        New-EventLog -LogName Application -Source $eventSource
        Write-InstallLog "event source ${eventSource}: registered in the Application log"
    }
    $gateExe = 'C:\Program Files\gate\bin\gate.exe'
    $drive = Split-Path -Qualifier $GateRoot
    # The closing event is Information on exit 0 and Warning on any other exit, or on none when gate.exe did not run.
    $closingCommand = "`$code = `$LASTEXITCODE; " +
        "`$free = [math]::Round([System.IO.DriveInfo]::new('$drive').AvailableFreeSpace / 1GB, 1); " +
        "`$type = if (`$code -eq 0) { [System.Diagnostics.EventLogEntryType]::Information } " +
        "else { [System.Diagnostics.EventLogEntryType]::Warning }; " +
        "`$shown = if (`$null -eq `$code) { '(no exit code)' } else { `$code }; " +
        "[System.Diagnostics.EventLog]::WriteEntry('$eventSource', 'gate box prune exited ' + `$shown + '; $drive free ' + `$free + ' GB', `$type, 1)"
    # Windows PowerShell auto-loads a module for a cmdlet from the user's module path, which the builder user (and so a
    # routed command) can write. Auto-loading is off and the events and free space come from .NET calls, so the task runs
    # no code from there. $GateRoot and $drive are refused above when they hold a quote, so single quotes keep them whole.
    $pruneCommand = "`$PSModuleAutoLoadingPreference = 'None'; " +
        "& '$gateExe' box prune --root '$GateRoot' 2>&1 | ForEach-Object { `$line = [string]`$_; if (`$line) { " +
        "[System.Diagnostics.EventLog]::WriteEntry('$eventSource', `$line, [System.Diagnostics.EventLogEntryType]::Information, 1) } }; " +
        $closingCommand
    $arguments = "-NoProfile -NonInteractive -Command `"$pruneCommand`""
    $action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument $arguments
    # No -RepetitionDuration: the repetition runs indefinitely, which the read-back below confirms.
    $trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).Date.AddMinutes(17) -RepetitionInterval (New-TimeSpan -Hours 6)
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType S4U -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Hours 2) -MultipleInstances IgnoreNew -StartWhenAvailable
    Register-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $pruneTaskName -Action $action -Trigger $trigger -Principal $principal `
        -Settings $settings -Force | Out-Null
    $repetition = @((Get-ScheduledTask -TaskPath $sccacheTaskPath -TaskName $pruneTaskName).Triggers)[0].Repetition
    if ($repetition.Interval -ne 'PT6H' -or $repetition.Duration) {
        throw ("task $sccacheTaskPath$pruneTaskName reads back with repetition interval '$($repetition.Interval)' and duration " +
            "'$($repetition.Duration)', expected PT6H and none (indefinite)")
    }
    Write-InstallLog "task $sccacheTaskPath$pruneTaskName registered: every 6 h, gate box prune --root $GateRoot as $User (S4U, Limited)"
    if (-not (Test-Path -LiteralPath $gateExe)) {
        Write-InstallLog "task $sccacheTaskPath$pruneTaskName will fail until a gate push installs $gateExe"
    }
}

Sync-SessionPath
$env:Path += ';' + (Join-Path $env:USERPROFILE '.cargo\bin')
Write-InstallLog '--- versions'
$godotConsole = Join-Path $godotVersionDir 'Godot_console.exe'
$checks = [ordered]@{
    'git' = @('--version'); 'pwsh' = @('--version'); 'dotnet' = @('--list-sdks'); 'node' = @('--version')
    'pnpm.cmd' = @('--version'); 'uv' = @('--version'); 'prek' = @('--version')
    'python' = @('--version'); 'python3' = @('--version'); 'rustup' = @('toolchain', 'list')
    'cargo' = @('--version'); 'cargo-nextest' = @('nextest', '--version'); 'just' = @('--version'); $godotConsole = @('--version', '--headless')
    'ffmpeg' = @('-version'); 'ffprobe' = @('-version'); 'bun' = @('--revision'); 'sccache' = @('--version')
}
foreach ($exe in $checks.Keys) {
    $out = try { (& $exe @($checks[$exe]) 2>&1) -join ' | ' } catch { "error: $($_.Exception.Message)" }
    Write-InstallLog "$exe $($checks[$exe] -join ' ') -> $out"
}
$out = try {
    $task = Get-ScheduledTask -TaskName $wslKeepaliveTask -ErrorAction SilentlyContinue
    if ($task) { $task.State } else { 'not registered' }
}
catch { "error: $($_.Exception.Message)" }
Write-InstallLog "task $wslKeepaliveTask -> $out"
# This elevated wsl.exe call would start the VM itself when the keep-alive task has not, so it runs only after the task.
$out = try {
    $env:WSL_UTF8 = '1'
    $distros = @(& wsl.exe --list --quiet | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($distros -notcontains 'Ubuntu') { 'not installed' }
    elseif ((Get-ScheduledTask -TaskName $wslKeepaliveTask -ErrorAction SilentlyContinue).State -ne 'Running') {
        "not run: task $wslKeepaliveTask is not running"
    }
    else { (@(& wsl.exe -d Ubuntu -u root -- bash -lc '. ~/.cargo/env && cargo --version && uname -r') -join ' | ') }
}
catch { "error: $($_.Exception.Message)" }
Write-InstallLog "WSL Ubuntu -> $out"
$out = try { $task = Get-SccacheServerTask; if ($task) { $task.State } else { 'not registered' } } catch { "error: $($_.Exception.Message)" }
Write-InstallLog "task $sccacheTaskPath$sccacheTaskName -> $out"
# --show-stats starts a server when none answers, so it runs only while the task's server is up.
$out = try {
    $task = Get-SccacheServerTask
    if ($task -and $task.State -eq 'Running') { (@(& sccache --show-stats 2>&1) -match '^Cache location') -join ' | ' }
    else { 'not run: the server task is not running' }
}
catch { "error: $($_.Exception.Message)" }
Write-InstallLog "sccache --show-stats -> $out"
$out = try { @(Get-Process sccache -ErrorAction SilentlyContinue | ForEach-Object { $_.Id }) -join ',' } catch { "error: $($_.Exception.Message)" }
Write-InstallLog "sccache server pid -> $out"
foreach ($name in $sccacheUserVariables) {
    $out = try { [Environment]::GetEnvironmentVariable($name, 'User') } catch { "error: $($_.Exception.Message)" }
    Write-InstallLog "user $name -> $out"
}
$out = try { (& dotnet workload list 2>&1) -join ' | ' } catch { "error: $($_.Exception.Message)" }
Write-InstallLog "dotnet workload list -> $out"
$out = try {
    $vcVersion = Get-VCToolsInstanceProperty 'installationVersion'
    if ($vcVersion) { $vcVersion } else { 'no instance with VCTools' }
}
catch { "error: $($_.Exception.Message)" }
Write-InstallLog "vswhere VCTools installationVersion -> $out"
foreach ($key in $gitSystemConfig.Keys) {
    $out = try { (& git config --system --get $key 2>&1) -join ' | ' } catch { "error: $($_.Exception.Message)" }
    Write-InstallLog "git config --system --get $key -> $out"
}
$out = try { (& git config --system --get-all safe.directory 2>&1) -join ' | ' } catch { "error: $($_.Exception.Message)" }
Write-InstallLog "git config --system --get-all safe.directory -> $out"
Write-InstallLog "Test-Path $GateRoot -> $(Test-Path $GateRoot)"
Write-InstallLog "Test-Path $netFxSdkDir -> $(Test-Path -LiteralPath $netFxSdkDir)"
$glProbe = 'C:\build-box\gl-probe.ps1'
if (Test-Path $glProbe) {
    $out = try { Get-GlProbeResult $glProbe } catch { "error: $($_.Exception.Message) (exit n/a)" }
    Write-InstallLog "GL probe -> $out"
}
else {
    Write-InstallLog "GL probe -> skipped: $glProbe does not exist (the box's provisioning copies it)"
}
Write-InstallLog ("COMPLETE failed={0} {1}" -f $failed.Count, ($failed -join ', '))
