# The Windows builder prep steps, in menu order. prep-windows.ps1 reads this list; each Id is also its step script's
# file name in this folder. Needs names the prep-windows.ps1 parameters a step cannot run without.
@{
    Steps = @(
        @{
            Id          = 'builder-account'
            Name        = 'Builder account'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $true
            Needs       = @()
            Caveat      = 'no account to run routed work as'
            Detail      = 'Creates -User, password typed twice, in Administrators. Skip it when the account already exists.'
        }
        @{
            Id          = 'openssh'
            Name        = 'OpenSSH, key-only ssh'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @('PublicKeyFile')
            Caveat      = 'no routing at all'
            Detail      = 'Installs sshd for -PublicKeyFile, password login off. Skipped, the PC cannot reach the builder.'
        }
        @{
            Id          = 'ssh-lan-only'
            Name        = 'SSH from the LAN only'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'ssh open to every network'
            Detail      = 'Sets the network Private and TCP 22 to the local subnet. Skipped, ssh answers on every network.'
        }
        @{
            Id          = 'tailscale'
            Name        = 'Tailscale (optional)'
            Default     = $false
            Reboot      = $false
            Destructive = $false
            Interactive = $true
            Needs       = @()
            Caveat      = 'reach the builder on the LAN only'
            Detail      = 'Installs Tailscale and signs in through a printed link, no auth key. Skip it on a shared LAN.'
        }
        @{
            Id          = 'ssh-tailnet-only'
            Name        = 'SSH from the tailnet only (optional)'
            Default     = $false
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'ssh also reachable on the LAN'
            Detail      = 'Own tailnet-only TCP 22 rule, stock ones off; disable gate-prep-lan-ssh by hand to close LAN ssh.'
        }
        @{
            Id          = 'build-box-acl'
            Name        = 'C:\build-box ACL'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'routed runs could replace installers'
            Detail      = 'Only admins write C:\build-box. Skipped, a routed command could replace a script an elevated run starts.'
        }
        @{
            Id          = 'wsl-platform'
            Name        = 'WSL platform'
            Default     = $true
            Reboot      = $true
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'no wsl commands routed'
            Detail      = 'Enables the WSL features, then a reboot. Skip it when no routed command runs under wsl.'
        }
        @{
            Id          = 'developer-mode'
            Name        = 'Developer Mode'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'symlink tests fail (error 1314)'
            Detail      = 'Lets a non-elevated token make symbolic links. Skipped, runs and tests that make one fail (1314).'
        }
        @{
            Id          = 'event-log-acl'
            Name        = 'Event log ACL'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'gate''s own events are lost'
            Detail      = 'Lets -User write the Application log, so routed commands can log under any source. Skipped, events are lost.'
        }
        @{
            Id          = 'autologon'
            Name        = 'Autologon'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $true
            Needs       = @()
            Caveat      = 'desktop runs fall back to the PC'
            Detail      = 'Logs -User on at boot, password as an LSA secret. Skipped, runs that need a desktop run on the PC.'
        }
        @{
            Id          = 'defender-exclusion'
            Name        = 'Defender exclusion'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'slower builds'
            Detail      = 'Excludes the work root and C:\build-box from scans. Skipped, Defender scans every file a build writes.'
        }
        @{
            Id          = 'realtime-off'
            Name        = 'Real-time scanning off (optional)'
            Default     = $false
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = '(none; lowers protection)'
            Detail      = 'Turns real-time scanning off, lowering protection; Tamper Protection must be off. Skip it unless needed.'
        }
        @{
            Id          = 'dev-drive'
            Name        = 'Dev Drive'
            Default     = $false
            Reboot      = $false
            Destructive = $true
            Interactive = $false
            Needs       = @('DiskNumber')
            Caveat      = 'work root on a plain NTFS disk'
            Detail      = 'Erases the empty disk -DiskNumber into a ReFS Dev Drive. Skip it with no empty disk; the root stays NTFS.'
        }
        @{
            Id          = 'never-sleep'
            Name        = 'Never sleep'
            Default     = $true
            Reboot      = $false
            Destructive = $false
            Interactive = $false
            Needs       = @()
            Caveat      = 'a sleeping builder drops runs'
            Detail      = 'High performance plan, no standby on AC, hibernate off. Skipped, a sleeping builder drops its runs.'
        }
    )
}
