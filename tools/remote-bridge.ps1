# The bridge gate.exe starts a routed run's remote attempt through: reads the request gate.exe writes to its stdin,
# dot-sources the remote.ps1 named by -RemoteScript, runs Invoke-RemoteCommand with the request's inputs and the host
# given as -HostAlias, and answers gate.exe with control lines on stdout. The command's output and remote.ps1's own
# lines go to the log and to stderr, as remote.ps1 writes them.
#
# Request (stdin, UTF-8 JSON, version 2): nonce, root, repo, key, subfolder, outputs (array), words (array), log,
# timeoutSeconds, stallSeconds, tail. Control lines (stdout, UTF-8, one JSON object a line, each written after a newline
# and carrying the request's nonce, so none is written before the nonce is read): {"started": true} right before
# Invoke-RemoteCommand is called; then {"status": n, "killed": bool, "reason": text or null}, the reason given only with
# status 75; or {"error": text} when anything stops the script first.
param(
    [Parameter(Mandatory)][string]$RemoteScript,
    [Parameter(Mandatory)][string]$HostAlias
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Written before remote.ps1 is dot-sourced, under names it does not use, so nothing it defines replaces them.
$bridgeHost = $HostAlias
$bridgeScript = $RemoteScript
$bridgeNonce = $null
$bridgeOut = [System.IO.StreamWriter]::new([Console]::OpenStandardOutput(), [System.Text.UTF8Encoding]::new($false))
$bridgeOut.AutoFlush = $true

# Writes one control line, `fields` and the request's nonce, as one line of ASCII JSON after a newline, so it starts a
# line even after output that did not end its own. Nothing is written while the nonce is not read: gate.exe would not
# take the line.
function Write-BridgeControl {
    param([System.Collections.Specialized.OrderedDictionary]$Fields)
    if ($null -eq $bridgeNonce) { return }
    $line = [ordered]@{ nonce = $bridgeNonce }
    foreach ($key in $Fields.Keys) { $line[$key] = $Fields[$key] }
    $json = ConvertTo-Json -InputObject $line -Compress -EscapeHandling EscapeNonAscii
    $bridgeOut.Write("`n$json`n")
}

# The request's property `name`, which must be of JSON kind `kind`.
function Get-BridgeProperty {
    param([System.Text.Json.JsonElement]$Request, [string]$Name, [string]$Kind)
    $value = [System.Text.Json.JsonElement]::new()
    if (-not $Request.TryGetProperty($Name, [ref]$value)) { throw "remote-bridge: the request has no $Name" }
    if ($value.ValueKind -ne $Kind) { throw "remote-bridge: the request's $Name is not a $Kind" }
    return $value
}

# The request's string `name`.
function Get-BridgeString {
    param([System.Text.Json.JsonElement]$Request, [string]$Name)
    return (Get-BridgeProperty -Request $Request -Name $Name -Kind 'String').GetString()
}

# The strings of the request's array `name`, each item a JSON string: no value is turned into a date or a number.
function Get-BridgeStrings {
    param([System.Text.Json.JsonElement]$Request, [string]$Name)
    $strings = [System.Collections.Generic.List[string]]::new()
    foreach ($item in (Get-BridgeProperty -Request $Request -Name $Name -Kind 'Array').EnumerateArray()) {
        if ($item.ValueKind -ne 'String') { throw "remote-bridge: an item of the request's $Name is not a String" }
        $strings.Add($item.GetString())
    }
    return , $strings.ToArray()
}

# The request's whole number `name`, at least `least`.
function Get-BridgeNumber {
    param([System.Text.Json.JsonElement]$Request, [string]$Name, [long]$Least)
    $number = [long]0
    if (-not (Get-BridgeProperty -Request $Request -Name $Name -Kind 'Number').TryGetInt64([ref]$number)) {
        throw "remote-bridge: the request's $Name is not a whole number"
    }
    if ($number -lt $Least) { throw "remote-bridge: the request's $Name is $number, below $Least" }
    return $number
}

try {
    $bridgeStrict = [System.Text.UTF8Encoding]::new($false, $true)
    $bridgeText = [System.IO.StreamReader]::new([Console]::OpenStandardInput(), $bridgeStrict).ReadToEnd()
    $bridgeCall = [System.Text.Json.JsonDocument]::Parse($bridgeText).RootElement
    if ($bridgeCall.ValueKind -ne 'Object') { throw 'remote-bridge: the request is not a JSON object' }
    $bridgeNonce = Get-BridgeString -Request $bridgeCall -Name 'nonce'
    $bridgeVersion = Get-BridgeNumber -Request $bridgeCall -Name 'version' -Least 0
    if ($bridgeVersion -ne 2) { throw "remote-bridge: request version $bridgeVersion is not 2" }
    if (-not [System.IO.Path]::IsPathFullyQualified($bridgeScript) -or -not $bridgeScript.EndsWith('.ps1', 'OrdinalIgnoreCase')) {
        throw "remote-bridge: -RemoteScript $bridgeScript is not a full path to a .ps1 file"
    }
    $bridgeOptions = @{
        Root           = Get-BridgeString -Request $bridgeCall -Name 'root'
        Repo           = Get-BridgeString -Request $bridgeCall -Name 'repo'
        Key            = Get-BridgeString -Request $bridgeCall -Name 'key'
        Subfolder      = Get-BridgeString -Request $bridgeCall -Name 'subfolder'
        Outputs        = Get-BridgeStrings -Request $bridgeCall -Name 'outputs'
        Log            = Get-BridgeString -Request $bridgeCall -Name 'log'
        TimeoutSeconds = Get-BridgeNumber -Request $bridgeCall -Name 'timeoutSeconds' -Least 1
        StallSeconds   = Get-BridgeNumber -Request $bridgeCall -Name 'stallSeconds' -Least 1
        Tail           = Get-BridgeNumber -Request $bridgeCall -Name 'tail' -Least 0
    }
    $bridgeWords = Get-BridgeStrings -Request $bridgeCall -Name 'words'

    . $bridgeScript

    if (-not (Test-RemoteHostAlias -Value $bridgeHost)) { throw "remote-bridge: -HostAlias $bridgeHost is not a host remote.ps1 takes" }
    $bridgeOptions.HostAlias = $bridgeHost
    $bridgeOptions.Clock = [System.Diagnostics.Stopwatch]::StartNew()
    Write-BridgeControl ([ordered]@{ started = $true })
    $bridgeRun = Invoke-RemoteCommand -Options $bridgeOptions -Command $bridgeWords
    $bridgeReason = if ($bridgeRun.Status -eq 75 -and $bridgeRun.Reason) { [string]$bridgeRun.Reason } else { $null }
    Write-BridgeControl ([ordered]@{ status = [long]$bridgeRun.Status; killed = [bool]$bridgeRun.Killed; reason = $bridgeReason })
}
catch {
    $message = $_.Exception.Message
    if ($null -eq $bridgeNonce) { [Console]::Error.WriteLine("remote-bridge: $message") }
    Write-BridgeControl ([ordered]@{ error = $message })
    exit 1
}
exit 0
