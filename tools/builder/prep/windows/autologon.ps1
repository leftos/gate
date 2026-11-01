#Requires -Version 7.3
# Logs -User on at the console after each boot, which the gate's desktop runs need. It asks for the account's password
# twice, checks it with a logon, stores it as the LSA secret DefaultPassword, sets Winlogon's AutoAdminLogon,
# DefaultUserName and DefaultDomainName, and removes any DefaultPassword registry value and AutoLogonCount. The
# password never reaches a file, a log or the console. It needs a terminal: the console or ssh -t.
#   pwsh -File autologon.ps1 -User <name> [-DryRun]
# Exits 0 when done or already done, 1 on a failure, 2 on a bad argument or a session with no console.
[CmdletBinding()]
param(
    [string]$User,
    [switch]$DryRun
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
$autologonSource = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class GatePrepAutologon {
    [StructLayout(LayoutKind.Sequential)]
    private struct LsaUnicodeString {
        public ushort Length;
        public ushort MaximumLength;
        public IntPtr Buffer;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct LsaObjectAttributes {
        public int Length;
        public IntPtr RootDirectory;
        public IntPtr ObjectName;
        public uint Attributes;
        public IntPtr SecurityDescriptor;
        public IntPtr SecurityQualityOfService;
    }

    private const uint PolicyGetPrivateInformation = 0x4;
    private const uint PolicyCreateSecret = 0x20;
    private const uint StatusObjectNameNotFound = 0xC0000034;
    private const int Logon32LogonInteractive = 2;
    private const int Logon32ProviderDefault = 0;

    [DllImport("advapi32.dll")]
    private static extern uint LsaOpenPolicy(IntPtr systemName, ref LsaObjectAttributes attributes, uint access, out IntPtr policy);

    [DllImport("advapi32.dll")]
    private static extern uint LsaStorePrivateData(IntPtr policy, ref LsaUnicodeString key, ref LsaUnicodeString data);

    [DllImport("advapi32.dll")]
    private static extern uint LsaRetrievePrivateData(IntPtr policy, ref LsaUnicodeString key, out IntPtr data);

    [DllImport("advapi32.dll")]
    private static extern uint LsaFreeMemory(IntPtr buffer);

    [DllImport("advapi32.dll")]
    private static extern uint LsaClose(IntPtr policy);

    [DllImport("advapi32.dll")]
    private static extern int LsaNtStatusToWinError(uint status);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool LogonUserW(string user, string domain, IntPtr password, int logonType, int provider, out IntPtr token);

    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    // Compares two NUL-terminated UTF-16 buffers.
    public static bool Same(IntPtr a, IntPtr b) {
        for (int i = 0; ; i += 2) {
            short x = Marshal.ReadInt16(a, i);
            if (x != Marshal.ReadInt16(b, i)) { return false; }
            if (x == 0) { return true; }
        }
    }

    public static void Validate(string user, string domain, IntPtr password) {
        IntPtr token;
        if (!LogonUserW(user, domain, password, Logon32LogonInteractive, Logon32ProviderDefault, out token)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        CloseHandle(token);
    }

    // The byte length of the LSA secret name, 0 when there is none. The secret's text is never read: its buffer is
    // zeroed and freed unseen.
    public static int SecretLength(string name) {
        IntPtr keyBuffer = Marshal.StringToHGlobalUni(name);
        IntPtr policy = IntPtr.Zero;
        IntPtr data = IntPtr.Zero;
        try {
            policy = OpenPolicy(PolicyGetPrivateInformation);
            LsaUnicodeString key = Key(name, keyBuffer);
            uint status = LsaRetrievePrivateData(policy, ref key, out data);
            if (status == StatusObjectNameNotFound) { return 0; }
            Check(status, "LsaRetrievePrivateData");
            if (data == IntPtr.Zero) { return 0; }
            LsaUnicodeString secret = Marshal.PtrToStructure<LsaUnicodeString>(data);
            if (secret.Buffer != IntPtr.Zero) { Marshal.Copy(new byte[secret.MaximumLength], 0, secret.Buffer, secret.MaximumLength); }
            return secret.Length;
        }
        finally {
            if (data != IntPtr.Zero) { LsaFreeMemory(data); }
            if (policy != IntPtr.Zero) { LsaClose(policy); }
            Marshal.FreeHGlobal(keyBuffer);
        }
    }

    public static void StoreSecret(string name, IntPtr password, int chars) {
        if (chars > 32766) { throw new ArgumentException("the password is longer than an LSA string holds"); }
        IntPtr keyBuffer = Marshal.StringToHGlobalUni(name);
        IntPtr policy = IntPtr.Zero;
        try {
            policy = OpenPolicy(PolicyCreateSecret);
            LsaUnicodeString key = Key(name, keyBuffer);
            LsaUnicodeString data = new LsaUnicodeString();
            data.Buffer = password;
            data.Length = (ushort)(chars * 2);
            data.MaximumLength = (ushort)(chars * 2 + 2);
            Check(LsaStorePrivateData(policy, ref key, ref data), "LsaStorePrivateData");
        }
        finally {
            if (policy != IntPtr.Zero) { LsaClose(policy); }
            Marshal.FreeHGlobal(keyBuffer);
        }
    }

    private static IntPtr OpenPolicy(uint access) {
        LsaObjectAttributes attributes = new LsaObjectAttributes();
        attributes.Length = Marshal.SizeOf(typeof(LsaObjectAttributes));
        IntPtr policy;
        Check(LsaOpenPolicy(IntPtr.Zero, ref attributes, access, out policy), "LsaOpenPolicy");
        return policy;
    }

    private static LsaUnicodeString Key(string name, IntPtr buffer) {
        LsaUnicodeString key = new LsaUnicodeString();
        key.Buffer = buffer;
        key.Length = (ushort)(name.Length * 2);
        key.MaximumLength = (ushort)(name.Length * 2 + 2);
        return key;
    }

    private static void Check(uint status, string call) {
        if (status != 0) {
            int code = LsaNtStatusToWinError(status);
            throw new Win32Exception(code, call + " failed: " + new Win32Exception(code).Message + " (" + code + ")");
        }
    }
}
'@

function Exit-Step([int]$Code, [string]$Message) {
    [Console]::Error.WriteLine("autologon: $Message")
    Write-Output "REASON $Message"
    exit $Code
}

function Get-WinlogonValue([string]$Name) {
    $item = Get-ItemProperty -LiteralPath $winlogon -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return $item.$Name
}

# Whether the LSA secret DefaultPassword holds a password; only its length is read.
function Test-AutologonSecret {
    if (-not ('GatePrepAutologon' -as [type])) { Add-Type -TypeDefinition $autologonSource }
    return [GatePrepAutologon]::SecretLength('DefaultPassword') -gt 0
}

# Asks for the password twice (at most 3 tries for a match), checks it logs $name on, and stores it as the LSA secret
# DefaultPassword. The password exists only as a SecureString and in unmanaged memory that is zeroed afterwards.
function Save-AutologonSecret {
    if (-not ('GatePrepAutologon' -as [type])) { Add-Type -TypeDefinition $autologonSource }
    $marshal = [Runtime.InteropServices.Marshal]
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $first = Read-Host -AsSecureString "password for $name"
        $second = Read-Host -AsSecureString "password for $name again"
        $a = [IntPtr]::Zero
        $b = [IntPtr]::Zero
        try {
            $a = $marshal::SecureStringToGlobalAllocUnicode($first)
            $b = $marshal::SecureStringToGlobalAllocUnicode($second)
            if (-not [GatePrepAutologon]::Same($a, $b)) { Write-Warning 'autologon: the passwords differ; type them again'; continue }
            try { [GatePrepAutologon]::Validate($name, $env:COMPUTERNAME, $a) }
            catch { Exit-Step 1 "the password did not log $name on: $($_.Exception.InnerException.Message)" }
            [GatePrepAutologon]::StoreSecret('DefaultPassword', $a, $first.Length)
            return
        }
        finally {
            if ($a -ne [IntPtr]::Zero) { $marshal::ZeroFreeGlobalAllocUnicode($a) }
            if ($b -ne [IntPtr]::Zero) { $marshal::ZeroFreeGlobalAllocUnicode($b) }
            $first.Dispose()
            $second.Dispose()
        }
    }
    Exit-Step 1 'the passwords did not match 3 times; autologon was not set'
}

if (-not $User) { Exit-Step 2 '-User is required: the builder account to log on, e.g. -User builder' }
$name = $User
if ($User -match '^([^\\]+)\\(.+)$') {
    if ($Matches[1] -ne '.' -and $Matches[1] -ne $env:COMPUTERNAME) { Exit-Step 2 "-User '$User' is not a local account; give the name alone" }
    $name = $Matches[2]
}
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { Exit-Step 1 'not elevated; run it from an elevated PowerShell (an administrator''s ssh session is one)' }

$on = (Get-WinlogonValue 'AutoAdminLogon') -eq '1' -and (Get-WinlogonValue 'DefaultUserName') -eq $name -and
"$(Get-WinlogonValue 'DefaultDomainName')" -ne '' -and $null -eq (Get-WinlogonValue 'AutoLogonCount') -and
$null -eq (Get-WinlogonValue 'DefaultPassword')
if ($on -and (Test-AutologonSecret)) {
    Write-Output 'autologon: already done'
    exit 0
}
if ($DryRun) {
    Write-Output "autologon: would ask for $name's password twice, check it by a logon and store it as the LSA secret DefaultPassword"
    Write-Output "autologon: would set Winlogon AutoAdminLogon 1, DefaultUserName $name, DefaultDomainName $env:COMPUTERNAME"
    Write-Output 'autologon: would remove any Winlogon DefaultPassword and AutoLogonCount values'
    exit 0
}
if ([Console]::IsInputRedirected) { Exit-Step 2 'autologon asks for the password; run it at the console or over ssh -t' }
Save-AutologonSecret
Set-ItemProperty -LiteralPath $winlogon -Name AutoAdminLogon -Value '1' -Type String
Set-ItemProperty -LiteralPath $winlogon -Name DefaultUserName -Value $name -Type String
Set-ItemProperty -LiteralPath $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME -Type String
# Winlogon counts AutoLogonCount down at each autologon and turns autologon off at 0; Setup can leave one behind.
foreach ($value in 'DefaultPassword', 'AutoLogonCount') {
    if ($null -ne (Get-WinlogonValue $value)) { Remove-ItemProperty -LiteralPath $winlogon -Name $value }
}
$readBack = "AutoAdminLogon '$(Get-WinlogonValue 'AutoAdminLogon')', DefaultUserName '$(Get-WinlogonValue 'DefaultUserName')', " +
"DefaultDomainName '$(Get-WinlogonValue 'DefaultDomainName')'"
$right = (Get-WinlogonValue 'AutoAdminLogon') -eq '1' -and (Get-WinlogonValue 'DefaultUserName') -eq $name -and
(Get-WinlogonValue 'DefaultDomainName') -eq $env:COMPUTERNAME
if (-not $right) { Exit-Step 1 "Winlogon reads $readBack after setting it" }
foreach ($value in 'DefaultPassword', 'AutoLogonCount') {
    if ($null -ne (Get-WinlogonValue $value)) { Exit-Step 1 "Winlogon still holds $value after removing it" }
}
Write-Output "autologon: on for $name (password in the LSA secret DefaultPassword)"
exit 0
