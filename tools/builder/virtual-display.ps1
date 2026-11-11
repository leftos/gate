# virtual-display.ps1: the Windows builder's virtual monitor. The machine has no physical display: its only one is the
# Amyuni usbmmidd IddCx driver's virtual monitor, which RustDesk installs and which adds a monitor only when asked, so a
# console session after a reboot has no attached display and a capture records a flat frame.
#
#   -Action ensure : \build-box\gate-virtual-display runs it as SYSTEM at boot and every 5 minutes. It waits for the
#                    driver's adapter and device interface, then plugs in one monitor only when none is plugged in, so a
#                    repeat run is silent.
#   -Action mode   : \build-box\gate-display-mode runs it as the builder user at logon. The driver first brings a
#                    monitor up at 1024x768; once set, Windows keeps 1920x1080 for that monitor, and a monitor plugged in
#                    again comes back at it.
#   -Action status : prints the adapter and its monitor count, for the installer's summary.
#
# It never restarts the machine and never restarts, disables or enables a device. It reports through the Application
# log under gate-virtual-display and writes no file: a file the builder user could write is an escalation path for the
# SYSTEM task. Both tasks run Windows PowerShell 5.1, so nothing here uses newer syntax.
param(
    [ValidateSet('ensure', 'mode', 'status')]
    [string]$Action
)
$ErrorActionPreference = 'Stop'

$eventSource = 'gate-virtual-display'
$virtualDisplayName = 'USB Mobile Monitor Virtual Display'
# How long ensure waits for the driver after a boot, and for a plugged monitor to appear.
$adapterWaitSeconds = 180
$adapterPollSeconds = 2
$monitorWaitSeconds = 15
$monitorPollSeconds = 0.5
# How long mode waits for the monitor to reach the console session, and the size it sets.
$modeWaitSeconds = 300
$modePollSeconds = 2
$modeWidth = 1920
$modeHeight = 1080
# ChangeDisplaySettingsEx constants: DM_PELSWIDTH, DM_PELSHEIGHT and CDS_UPDATEREGISTRY. The setting is persisted for
# this user's session, never globally.
$dmPelsWidth = 0x80000
$dmPelsHeight = 0x100000
$cdsUpdateRegistry = 1

function Get-Now { return [DateTime]::UtcNow }

function Wait-Seconds([double]$Seconds) { [Threading.Thread]::Sleep([int]($Seconds * 1000)) }

# The driver's plug IOCTL and the display enumeration both actions use.
$virtualDisplayTypes = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
public struct DISPLAY_DEVICE {
    public int cb;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string DeviceName;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
    public int StateFlags;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey;
}

[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
public struct DEVMODE {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
    public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
    public int dmFields, dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
    public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
    public short dmLogPixels;
    public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
    public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
}

public static class DisplayApi {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern bool EnumDisplayDevicesW(string device, int index, ref DISPLAY_DEVICE info, int flags);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern bool EnumDisplaySettingsW(string device, int mode, ref DEVMODE settings);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int ChangeDisplaySettingsExW(string device, ref DEVMODE settings, IntPtr hwnd, int flags, IntPtr param);

    public static DEVMODE Current(string device) {
        var settings = new DEVMODE();
        settings.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
        EnumDisplaySettingsW(device, -1, ref settings);
        return settings;
    }
}

public static class UsbmmIdd {
    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVICE_INTERFACE_DATA { public int cbSize; public Guid InterfaceClassGuid; public int Flags; public IntPtr Reserved; }

    [DllImport("setupapi.dll", SetLastError = true)]
    static extern IntPtr SetupDiGetClassDevsW(ref Guid g, IntPtr enumerator, IntPtr hwnd, int flags);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr devInfo, ref Guid g, int index, ref SP_DEVICE_INTERFACE_DATA data);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiGetDeviceInterfaceDetailW(IntPtr set, ref SP_DEVICE_INTERFACE_DATA data, IntPtr detail, int size,
        out int required, IntPtr devInfo);
    [DllImport("setupapi.dll")]
    static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr sa, uint disposition, uint flags,
        IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(SafeFileHandle h, uint code, byte[] inBuf, int inLen, IntPtr outBuf, int outLen,
        out int returned, IntPtr overlapped);

    static Guid InterfaceGuid = new Guid("b5ffd75f-da40-4353-8ff8-b6daf6f1d8ca");
    const uint PlugIoctl = 2307084;

    public static string InterfacePath() {
        IntPtr set = SetupDiGetClassDevsW(ref InterfaceGuid, IntPtr.Zero, IntPtr.Zero, 0x12);
        if (set == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error(), "SetupDiGetClassDevsW");
        try {
            var data = new SP_DEVICE_INTERFACE_DATA();
            data.cbSize = Marshal.SizeOf(typeof(SP_DEVICE_INTERFACE_DATA));
            if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref InterfaceGuid, 0, ref data)) {
                int code = Marshal.GetLastWin32Error();
                if (code == 259) return null;
                throw new Win32Exception(code, "SetupDiEnumDeviceInterfaces");
            }
            int required;
            int sizeCode = 0;
            if (!SetupDiGetDeviceInterfaceDetailW(set, ref data, IntPtr.Zero, 0, out required, IntPtr.Zero))
                sizeCode = Marshal.GetLastWin32Error();
            if (sizeCode != 0 && sizeCode != 122)
                throw new Win32Exception(sizeCode, "SetupDiGetDeviceInterfaceDetailW size");
            if (required < 8) throw new Win32Exception(sizeCode, "SetupDiGetDeviceInterfaceDetailW size");
            IntPtr buf = Marshal.AllocHGlobal(required);
            try {
                Marshal.WriteInt32(buf, IntPtr.Size == 8 ? 8 : 6);
                if (!SetupDiGetDeviceInterfaceDetailW(set, ref data, buf, required, out required, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "SetupDiGetDeviceInterfaceDetailW");
                return Marshal.PtrToStringUni(new IntPtr(buf.ToInt64() + 4));
            } finally { Marshal.FreeHGlobal(buf); }
        } finally { SetupDiDestroyDeviceInfoList(set); }
    }

    public static void Plug() {
        string path = InterfacePath();
        if (path == null) throw new InvalidOperationException("usbmmidd device interface is not present");
        using (SafeFileHandle h = CreateFileW(path, 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
            if (h.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateFileW " + path);
            byte[] cmd = { 0x10, 0, 0, 0 };
            int returned;
            if (!DeviceIoControl(h, PlugIoctl, cmd, cmd.Length, IntPtr.Zero, 0, out returned, IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "DeviceIoControl");
        }
    }
}
'@

# Compiles the interop the first time an action needs it: dot-sourcing this script compiles nothing and touches no
# device.
function Initialize-VirtualDisplayTypes {
    if (('UsbmmIdd' -as [type]) -and ('DisplayApi' -as [type])) { return }
    Add-Type -TypeDefinition $virtualDisplayTypes
}

# The Application log under the source the installer registers.
function Write-DisplayEvent([string]$Message, [string]$Type) {
    [Diagnostics.EventLog]::WriteEntry($eventSource, $Message, [Diagnostics.EventLogEntryType]$Type)
}

# The usbmmidd display adapter, or $null when the driver is not installed or not present.
function Get-VirtualAdapter {
    Get-PnpDevice -Class Display -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -contains 'usbmmidd' } | Select-Object -First 1
}

# True when the driver's adapter and its device interface are both present. This one compiles the interop.
function Test-VirtualAdapter {
    if (-not (Get-VirtualAdapter)) { return $false }
    Initialize-VirtualDisplayTypes
    return [bool]([UsbmmIdd]::InterfacePath())
}

# The monitors plugged into the adapter with instance id $AdapterId. A monitor whose parent cannot be read is not counted.
function Get-VirtualMonitorCount([string]$AdapterId) {
    $monitors = @(Get-PnpDevice -Class Monitor -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
            $property = @(Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_Parent `
                    -ErrorAction SilentlyContinue)[0]
            $property -and $property.Data -and $property.Data -eq $AdapterId
        })
    return [int]$monitors.Length
}

# Plugs in one virtual monitor: the driver's add IOCTL, the call RustDesk's own module makes. It never removes one.
function Invoke-MonitorPlug {
    Initialize-VirtualDisplayTypes
    [UsbmmIdd]::Plug()
}

# The attached virtual display of this session, as an object with its device name and current size, or $null.
function Get-VirtualDisplay {
    Initialize-VirtualDisplayTypes
    for ($index = 0; ; $index++) {
        $info = New-Object DISPLAY_DEVICE
        $info.cb = [Runtime.InteropServices.Marshal]::SizeOf($info)
        if (-not [DisplayApi]::EnumDisplayDevicesW([NullString]::Value, $index, [ref]$info, 0)) { return $null }
        if (($info.StateFlags -band 1) -eq 0) { continue }
        if ($info.DeviceString -ne $virtualDisplayName) { continue }
        $settings = [DisplayApi]::Current($info.DeviceName)
        return [pscustomobject]@{ Device = $info.DeviceName; Width = $settings.dmPelsWidth; Height = $settings.dmPelsHeight }
    }
}

# Sets the display's size, keeping its other settings: the ChangeDisplaySettingsEx return code, 0 on success.
function Set-VirtualDisplayMode([string]$Device, [int]$Width, [int]$Height) {
    Initialize-VirtualDisplayTypes
    $settings = [DisplayApi]::Current($Device)
    $settings.dmPelsWidth = $Width
    $settings.dmPelsHeight = $Height
    $settings.dmFields = $dmPelsWidth -bor $dmPelsHeight
    return [int][DisplayApi]::ChangeDisplaySettingsExW($Device, [ref]$settings, [IntPtr]::Zero, $cdsUpdateRegistry, [IntPtr]::Zero)
}

# Calls $Probe at once and returns its first truthy result; otherwise waits $PollSeconds between tries until $Deadline
# passes, then returns the last result.
function Wait-Until([scriptblock]$Probe, [datetime]$Deadline, [double]$PollSeconds) {
    $result = & $Probe
    while (-not $result -and (Get-Now) -lt $Deadline) {
        Wait-Seconds $PollSeconds
        $result = & $Probe
    }
    return $result
}

# The monitor count once it reaches one, or the last count read when $monitorWaitSeconds pass. The probe returns the
# count itself, so a timeout still reports the count it last saw.
function Wait-VirtualMonitorCount([string]$AdapterId) {
    $deadline = (Get-Now).AddSeconds($monitorWaitSeconds)
    return Wait-Until -Probe { Get-VirtualMonitorCount $AdapterId } -Deadline $deadline -PollSeconds $monitorPollSeconds
}

# ensure: plugs in one monitor only when none is plugged in, so a steady-state run compiles nothing and stays silent.
function Invoke-VirtualDisplayEnsure {
    $deadline = (Get-Now).AddSeconds($adapterWaitSeconds)
    $missing = ("virtual-display: the usbmmidd virtual display driver is missing: its adapter or device interface " +
        "is not present after $adapterWaitSeconds s")
    $adapter = Wait-Until -Probe { Get-VirtualAdapter } -Deadline $deadline -PollSeconds $adapterPollSeconds
    if (-not $adapter) {
        Write-DisplayEvent $missing 'Error'
        return 2
    }
    $before = Get-VirtualMonitorCount $adapter.InstanceId
    if ($before -gt 0) { return 0 }
    if (-not (Wait-Until -Probe { Test-VirtualAdapter } -Deadline $deadline -PollSeconds $adapterPollSeconds)) {
        Write-DisplayEvent $missing 'Error'
        return 2
    }
    try { Invoke-MonitorPlug }
    catch {
        Write-DisplayEvent "virtual-display: plugging in a virtual monitor failed: $($_.Exception.Message)" 'Error'
        return 1
    }
    $after = Wait-VirtualMonitorCount $adapter.InstanceId
    if ($after -lt 1) {
        Write-DisplayEvent "virtual-display: no virtual monitor appeared after the plug (count $after)" 'Error'
        return 1
    }
    Write-DisplayEvent "virtual-display: plugged in a virtual monitor ($before -> $after)" 'Information'
    return 0
}

# mode: waits for the monitor to reach the console session and sets it to the recognised size.
function Invoke-VirtualDisplayMode {
    $deadline = (Get-Now).AddSeconds($modeWaitSeconds)
    $display = Wait-Until -Probe { Get-VirtualDisplay } -Deadline $deadline -PollSeconds $modePollSeconds
    if (-not $display) {
        Write-DisplayEvent "virtual-display: no virtual monitor is attached in this session after $modeWaitSeconds s" 'Error'
        return 1
    }
    if ($display.Width -eq $modeWidth -and $display.Height -eq $modeHeight) { return 0 }
    $code = Set-VirtualDisplayMode -Device $display.Device -Width $modeWidth -Height $modeHeight
    if ($code -ne 0) {
        Write-DisplayEvent "virtual-display: ChangeDisplaySettingsEx on $($display.Device) returned $code" 'Error'
        return 1
    }
    $was = '{0}x{1}' -f $display.Width, $display.Height
    Write-DisplayEvent ('virtual-display: set {0} to {1}x{2} (was {3})' -f $display.Device, $modeWidth, $modeHeight, $was) 'Information'
    return 0
}

# status: the line the installer's summary logs. It reads the adapter and the count only, so it compiles nothing.
function Invoke-VirtualDisplayStatus {
    $adapter = Get-VirtualAdapter
    if (-not $adapter) {
        Write-Host 'virtual-display: usbmmidd adapter not present'
        return 0
    }
    Write-Host ('virtual-display: adapter present, {0} virtual monitor(s)' -f (Get-VirtualMonitorCount $adapter.InstanceId))
    return 0
}

function Invoke-VirtualDisplayAction([string]$Action) {
    switch ($Action) {
        'ensure' { return Invoke-VirtualDisplayEnsure }
        'mode' { return Invoke-VirtualDisplayMode }
        'status' { return Invoke-VirtualDisplayStatus }
        default { throw "virtual-display: -Action must be ensure, mode or status, not '$Action'" }
    }
}

# Runs one action. A throw no action handles (a device or display call that fails) becomes an Error event and exit 1,
# so a task run that nobody watches still leaves its reason in the Application log.
function Invoke-VirtualDisplay([string]$Action) {
    try { return Invoke-VirtualDisplayAction -Action $Action }
    catch {
        $message = "virtual-display: $Action failed: $($_.Exception.Message)"
        try { Write-DisplayEvent $message 'Error' }
        catch { [Console]::Error.WriteLine("$message (and the event could not be written: $($_.Exception.Message))") }
        return 1
    }
}

if ($MyInvocation.InvocationName -ne '.') { exit (Invoke-VirtualDisplay -Action $Action) }
