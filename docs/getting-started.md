# Getting started

This guide takes a Windows PC from nothing to gated commands and then, if you want them, to commands routed to builders: a second Windows machine, and optionally a Mac beside it, that run this PC's builds and tests. Calling gate from an agent is [agents.md](agents.md). Every flag, line and file format is in [design.md](design.md), and the terms are in the [glossary](GLOSSARY.md).

## Prerequisites

- Windows 11 on x64, with winget (App Installer).
- PowerShell 7.3 or later, `pwsh`: `winget install Microsoft.PowerShell`. Windows PowerShell 5.1 runs neither the install script nor the launcher.
- git: `winget install --id Git.Git`.

The Rust toolchain and the Visual Studio C++ Build Tools are not prerequisites: the install script finds what is missing and offers to install it.

## Install

```
git clone https://github.com/leftos/gate.git
cd gate
pwsh tools/install.ps1
```

The script first checks for rustup with the stable MSVC toolchain and for the Visual Studio C++ Build Tools. Each missing piece gets its own `install: missing: ...` line, and the script asks once: `install: install them now with winget? [y/N]`.

A yes installs the Build Tools (about 6 GB; Windows asks for admin rights), rustup and the stable MSVC toolchain, then checks again. `-Yes` answers the question in advance, and `-WhatIf` lists every step without taking it.

It then builds `gate.exe` and its desktop helper in release, installs both in `%LOCALAPPDATA%\gate\bin` and adds that folder to your user PATH. Open a new terminal, then check the install:

```
gate --version
```

It prints `gate <version>+<build id>`. Run `pwsh tools/install.ps1` again after a `git pull` to rebuild and replace the installed pair; this is safe while gates are running ([design.md, Install](design.md#install)).

## The first gated command

From a scratch folder, in the new terminal:

```
gate run --slot light --timeout 60 --log .tmp/hello.log -- git --version
```

`--slot`, `--timeout`, `--log` and the command after `--` are all required. gate waits for a free slot of the kind asked for and runs the command at below-normal priority with its output going to the log. It then prints the log's last lines, here git's `git version ...` line, and a verdict line, `gate: passed in <s> s (...). Full output: <log>`, and exits with the command's status.

Typing `gate` directly suits a person at a terminal. Scripts, git hooks and agents call gate through a repo's launcher instead ([Wiring a repo](#wiring-a-repo), [agents.md](agents.md)).

`gate --help` prints every subcommand and option.

### Slots

- `heavy`: a command that keeps several cores busy, such as a build or a whole test suite.
- `light`: a command that keeps one or two cores busy, such as a filtered test run or a script.
- `none`: a command that only dispatches or watches remote work, such as `gh run watch`. It takes no slot and never waits.
- `critical`: a timing or profiling run that needs the machine to itself. It waits for the gates ahead of it, takes every heavy slot and all but a couple of light ones, and needs `--reason "<why>"`. Never use it for a test run.

Each kind has a fixed number of slots: `max(1, floor((logical processors - 1) / threads))`, with 4 threads per heavy slot and 2 per light slot. `%LOCALAPPDATA%\gate\v1\slot-counts.json`, `{"v": 1, "heavy": <n>, "light": <n>}`, overrides either count, each from 1 to 64 ([design.md, Slots](design.md#slots)).

### The log

The command's whole output goes to the log, one line per output line, each stamped and tagged: `<time> out <text>` for its standard output, `<time> err <text>` for its standard error and `<time> gat <text>` for gate's own lines. The console shows only the last lines of a local run (`--tail <lines>`, default 20); a routed run shows its output as it goes. gate creates the log's folder and replaces an earlier log at the same path.

### The verdict

A command that exits 0 still fails when its log holds a failure marker: a line `Build FAILED.`, `error CS<number>`, `: error `, `Test run summary: Failed!`, `No test projects were found`, a line `failed: <n>` with `n` above 0, or one of gate's own kill lines. gate then says `gate: command exited 0 but its log reports a failure -> <log>`, prints the marker lines and exits 1. `--no-markers` makes the command's status the whole verdict.

A failed run ends with `gate: FAILED (status <n>). Full output: <log>`. The exit codes are in [design.md, Exit codes](design.md#exit-codes).

### The cores hint

When a run's slot did not fit the command, gate says so before the verdict, for example `gate: used 7.4 cores on average (peak 15, 9.3 free) on a light slot; work that keeps more than 2 busy belongs on heavy`, or `... on a heavy slot; work that keeps under 1.5 busy fits on light`. Run the command on the slot the line names from then on.

### Timeouts

`--timeout <seconds>` is the run's ceiling, counted in load-adjusted seconds: time on a busy machine counts only by the share of the cores other work left free, so a command slowed by other work is not killed for it. Past the ceiling gate kills the command's whole process tree, says `gate: TIMED OUT ...` and exits 124. A backstop of five times the ceiling in wall time kills it whatever the load (`gate: BACKSTOP ...`).

`--timeout auto:<seconds>` makes the number a floor. Once the command has passing runs on record, the ceiling rises to three times its load-adjusted p99 when that is longer, and gate says so: `gate: ceiling <n> (3x ... p99 of ..., above the given <floor>)`.

With 5 or more passing runs on record, gate also says what to expect as the run takes its slot, `gate: expected about <time> (usual <p50>, p90 <p90>, <n> runs)`, and says once when a run passes three times its usual p90, `gate: still running after ...`.

### Stalls

A command with no output, no CPU use and no new process for the stall window, 120 s by default, is killed with `gate: STALLED: ...` and exit 124. `--stall <seconds>` sets the window. Give a test suite whose own hang guards are long a window above its longest guard, so a hung test fails with its own message.

### Stopping a run

Ctrl+C stops a queued gate or the running command. `gate stop-tree <pid>` stops a process tree and every gate inside it.

## Wiring a repo

A repo calls gate through its launcher, a committed copy of this repo's `tools/gate.ps1`:

1. Copy `tools/gate.ps1` from this repo into the repo's `tools/` folder, unchanged, and commit it.
2. Call it from the repo's scripts, docs and hooks:

```
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/test.log --timeout 900 --slot heavy -- cargo nextest run
```

The launcher hands every word to the installed `gate.exe`, never searching PATH.

It takes the file `GATE_BOX_EXE` names when that variable is set (a command inside a routed run on a builder); otherwise the first that exists of `C:\Program Files\gate\bin\gate.exe` (a builder's install) and `%LOCALAPPDATA%\gate\bin\gate.exe` (a user's install). On a machine without one it says `gate: no gate.exe found; running the command without slots or watchdog` and runs the command itself, so a clone still works where gate is not installed.

The launcher needs pwsh 7.3 or later. It also takes the PowerShell spelling, `-Log <path> -TimeoutSeconds <n> -Slot <kind> -- <command>`. Keep `.tmp/` in the repo's `.gitignore`, since the logs go there.

### Git hooks

A hook that builds or tests calls the launcher too, never a bare `gate`, since a hook runs on every machine that commits. A gated commit holds a slot for all of its hooks:

```
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/commit.log --timeout 1200 --slot light -- git commit -F {message file}
```

Write the message to a file new to that commit, so a failed write can never ship an earlier commit's message.

Use `--slot light` when the hooks only check the staged files and `--slot heavy` when they build or test. A commit on `--slot none` takes a light slot and says `gate: a commit's hooks take a slot: pass --slot heavy or --slot light`. A gate inside the hooks that asks for `--slot critical` or `--alone` is refused.

### The `.gate` files

A repo describes itself to gate in a tracked `.gate` folder. Every file is optional:

| File | What it declares |
|---|---|
| `toolchain.toml` | The tools (`[tools]`) and the .NET `workloads` a routed run's builder must have. A tool's version is a minimum (`rust`, `cargo-nextest`, `node` and the others), or `""` for any version; `godot` and `unreal` are exact pins instead. `gate check` checks this PC's tools against it and says the workloads are `not checked yet`. |
| `siblings.toml` | The sibling worktrees beside the repo that a routed run carries with it. |
| `warmup.toml` | The commands `gate warm` runs to warm a builder's work folder. |
| `desktop.toml` | The routed commands that need the builder's logged-on desktop (`patterns`), only its console session (`session`) or a desktop of their own that no one sees (`hidden`). |
| `local.toml` | The commands that always run on this PC (`patterns`). |
| `env.toml` | The prefixes of the repo's own environment variables (`prefixes`), so a routed run names the ones it does not send. |
| `windows.toml` | The paths whose change keeps a run off a macOS builder (`paths`). |

A repo that builds with Rust and tests with cargo-nextest might declare:

```toml
[tools]
rust = "1.89"
cargo-nextest = ""
```

The formats are in [design.md, remote.json](design.md#remotejson) and [design.md, Routing](design.md#routing); [routing-a-repo.md](routing-a-repo.md) says when a repo needs each file.

### `gate check`

Run `gate check` in the repo. It says what this PC lacks for it: each tool `.gate/toolchain.toml` names, the .NET SDK a root `global.json` pins and each declared sibling, one `check: here: ...` line per item. A missing or too-old item gets an `install: here: <item>: <command>` hint. Workloads the manifest lists get one line, `workloads <ids> not checked yet`, which counts as neither missing nor skipped.

It ends with `check: here: nothing missing` or `check: here: <n> missing, <n> too old`. It installs nothing, and exits 1 when anything is missing or a file could not be read ([design.md, gate check](design.md#gate-check)).

## Builders

A builder is another machine that runs this PC's heavy and light commands over ssh, each in a work folder of its own, and brings their results back. Routing is optional: with no `%LOCALAPPDATA%\gate\remote.json`, everything runs on this PC.

A Windows builder is required: `remote.json`'s `host`, which every routing setup names, is always a Windows builder. A Mac builder, or a further Windows builder, joins as an entry of the `builders` array beside it.

The `host` builder's [work root](GLOSSARY.md) is `D:\gate-remote`: the client fixes it there, and the toolchain installer puts its sccache cache in `D:\sccache` and refuses to make the work root on a drive that does not exist. So the `host` builder needs a D: drive. The prep's Dev Drive step can make one from an empty disk.

The order is: prepare the machine, reach it over ssh from this PC, install its toolchains, then point this PC at it, push gate to it and route repos.

### Prepare the machine

The builder prep is a checklist of step scripts, one per item below, run from a master script that shows a menu with the defaults ticked. Every step can also be run alone, and running it again on a machine where it is done changes nothing.

#### A Windows builder

1. On this PC, make an ssh key ssh can use without asking, since gate runs ssh in batch mode: `ssh-keygen -t ed25519`, with no passphrase or with the key loaded in ssh-agent. Copy the public key file (`.pub`) to the builder.
2. At the builder's console, in an elevated terminal, run `winget install Microsoft.PowerShell` and `winget install --id Git.Git`. Open a new elevated pwsh, then clone this repo (or copy its `tools/builder` folder) onto the machine.
3. From an elevated pwsh in the clone:

```
pwsh -File tools/builder/prep-windows.ps1 -User {builder user} -PublicKeyFile {path to the .pub file}
```

The menu lists the steps with the defaults ticked: type step numbers to toggle them, `a` to tick every step, `d` to go back to the defaults, Enter to run and `q` to quit.

Without the menu, `-Defaults` runs the defaults and `-Steps <id>,<id>` the named steps. At a console both first ask `prep: run <ids>? [y/N]`, and `-Yes` skips the question. `-List` prints the steps and their caveats. `-DryRun` has each step say what it would change and change nothing; it still needs an elevated session and `-User`.

`-GateRoot` is the gate's work root, and its default, `D:\gate-remote`, is the one the `host` builder must use; the Defender exclusion and the Dev Drive steps read it. `-DiskNumber` is the empty disk for the Dev Drive. A step that needs a parameter you did not give stops the run before anything changes: `prep: step <id> needs -<parameter>; give it or leave the step out`.

The script must run elevated, and the menu and the steps that ask for a password need a console or `ssh -t`. Each finished step is recorded in `C:\build-box\prep-state.json` and logged to `C:\build-box\prep.log`. After the WSL step the run stops with `prep: reboot, then run prep again; finished steps show as done`.

Each step is also a script of its own in `tools/builder/prep/windows/`, run as `pwsh -File <id>.ps1` with the parameters its header names; `-DryRun` shows what it would change. The prep exits 0 when no step failed, 1 when one did or the session is not elevated, and 2 on a bad argument.

| Step | Id | Ticked by default | What skipping it costs |
|---|---|---|---|
| Builder account | `builder-account` | yes | No account to run routed work as. The step creates `-User` in Administrators, its password typed twice; an existing account keeps its password. |
| OpenSSH, key-only ssh | `openssh` | yes | No routing at all: the PC cannot reach the builder. The step installs sshd for `-PublicKeyFile` with password login off. |
| SSH from the LAN only | `ssh-lan-only` | yes | ssh answers on every network. The step sets the network to Private and lets TCP 22 in from the local subnet only. |
| Tailscale (optional) | `tailscale` | no | The PC reaches the builder on the LAN only. The step installs Tailscale and signs in through a link it prints; no auth key is ever taken. |
| SSH from the tailnet only (optional) | `ssh-tailnet-only` | no | ssh stays reachable on the LAN. The step adds a tailnet-only TCP 22 rule and turns the stock ones off; disable the `gate-prep-lan-ssh` rule by hand to close LAN ssh. |
| `C:\build-box` ACL | `build-box-acl` | yes | A routed command could replace a script that an elevated run starts from `C:\build-box`. |
| WSL platform | `wsl-platform` | yes | No `wsl` command can be routed. The step enables the WSL features and asks for a reboot. |
| Developer Mode | `developer-mode` | yes | Runs and tests that make a symbolic link fail with error 1314. |
| Event log ACL | `event-log-acl` | yes | gate's own events are lost: the step lets `-User` write the Application log. |
| Autologon | `autologon` | yes | Runs that need the builder's desktop run on the PC instead. The step logs `-User` on at boot, its password kept as an LSA secret. |
| Defender exclusion | `defender-exclusion` | yes | Slower builds: Defender scans every file a build writes. The step excludes the work root and `C:\build-box`. |
| Real-time scanning off (optional) | `realtime-off` | no | Nothing; the step lowers the builder's protection, and Tamper Protection must be turned off first. |
| Dev Drive | `dev-drive` | no | The work root stays on a plain NTFS disk. The step erases the empty disk `-DiskNumber` into a ReFS Dev Drive at `-GateRoot`'s letter, D: for the `host` builder, after you type FORMAT, and refuses a disk that holds data. |
| Never sleep | `never-sleep` | yes | A sleeping builder drops its runs. The step sets the High performance plan, no standby on AC power and hibernate off. |

#### A Mac builder

Install the Xcode Command Line Tools (`xcode-select --install`), which bring git and the C compiler `gate push` builds with, and clone this repo. Then, as an admin account, at the console or over `ssh -t`:

```
sudo bash tools/builder/prep-macos.sh --user {builder user} --public-key-file {path to the PC's .pub file}
```

The menu, `--list`, `--defaults`, `--steps <id>,<id>`, `--dry-run` (which needs no root) and `--yes` work as on Windows. Each step is logged to `/Library/Application Support/gate-prep/prep.log`, each finished one is recorded in `/Library/Application Support/gate-prep/state`, and each is a script of its own in `tools/builder/prep/macos/`.

| Step | Id | Ticked by default | What skipping it costs |
|---|---|---|---|
| Builder account | `builder-account` | yes | No account to run routed work as. The step creates a standard (not admin) account, and `sysadminctl` asks for its password; an existing account is left alone. |
| Remote Login (ssh) | `remote-login` | yes | No routing at all. The step turns Remote Login on and admits the builder account to ssh. |
| The PC's ssh key | `authorized-key` | yes | The PC cannot log in. The step appends `--public-key-file` to the builder's `~/.ssh/authorized_keys`. |
| Tailscale (optional) | `tailscale` | no | The PC reaches the builder on the LAN only. The step installs Homebrew's tailscale and signs in through a link it prints; no auth key is ever taken. |
| SSH for the builder user only | `ssh-allow-users` | yes | Any account may log in over ssh. The step allows ssh for the builder and the admin running the prep only. |
| Xcode license accepted | `xcode-license` | yes | Builds that call Xcode's tools fail. The step skips itself when Xcode is not installed. |
| FileVault off | `filevault-off` | no | After a reboot the Mac waits at the unlock screen, and automatic login cannot work. The step lowers the disk's security: whoever holds the Mac can read it. |
| Automatic login | `autologin` | yes | After a reboot, routed runs wait for a person to log in. The step needs FileVault off and stores the password, typed twice, only in `/etc/kcpassword`. |

Automatic login is ticked by default and FileVault off is not. With FileVault on, a run of the defaults ends the autologin step with `SKIP FileVault is on; turn it off (step filevault-off) and run this step again`: tick `filevault-off` too, or leave the Mac to wait for a person after each reboot.

After a password change, run the autologin step again on its own: `sudo bash tools/builder/prep/macos/autologin.sh --user {builder user} --force`.

### Reach the builder over ssh

Give the builder an ssh alias in this PC's `~/.ssh/config`, and connect once by hand (`ssh {builder}`) to accept its host key; gate's own ssh calls run in batch mode and cannot answer that question:

```
Host {builder}
    HostName {builder address}
    User {builder user}
    IdentityFile ~/.ssh/id_ed25519
```

`ssh {builder}` should now log in without a prompt. The toolchain install below and every routed run go through this alias.

### Install the toolchains

#### On a Windows builder

The installer runs from `C:\build-box\gate\`, which the prep's `C:\build-box ACL` step leaves writable by Administrators and SYSTEM only, so the files go there in two steps ([routing-a-repo.md, Builder setup](routing-a-repo.md#builder-setup), Copy):

1. From this PC's clone, copy the two files into the builder user's home: `scp tools/builder/install-toolchains.ps1 tools/builder/wsl-setup.sh {builder}:C:/Users/{builder user}/`.
2. In an elevated session on the builder (the builder user's ssh session is one, since the account is in Administrators), from pwsh, make the folder if it does not exist and copy those exact files into it, never by a wildcard: `New-Item -ItemType Directory -Force C:\build-box\gate`, then `Copy-Item ~\install-toolchains.ps1, ~\wsl-setup.sh C:\build-box\gate\`. `wsl-setup.sh` must sit beside the script.

Then run the installer in the builder user's own ssh session, under Windows PowerShell 5.1, while no gate runs on the builder:

```
powershell -NoProfile -ExecutionPolicy Bypass -File C:\build-box\gate\install-toolchains.ps1 -User {builder user}
```

Its `-GateRoot` defaults to `D:\gate-remote` and its `-SccacheDir` to `D:\sccache`, which is what the `host` builder needs; leave both at their defaults there. It installs pinned toolchains (git, PowerShell, .NET SDKs, Node.js, Python, uv and prek, the C++ Build Tools, Rust, cargo-nextest, just, WSL Ubuntu, sccache and more), the gate's work root with its locked-down ACL, and the builder's scheduled tasks. `-Only {step prefix}` runs part of it.

#### On a Mac builder

`tools/builder/install-macos.sh` runs in three parts ([routing-a-repo.md, A macOS builder](routing-a-repo.md#a-macos-builder)):

1. As the builder user, without sudo, either piped from this PC's clone, `ssh {builder} 'bash -s' -- < tools/builder/install-macos.sh`, or from a copy on the Mac, `bash install-macos.sh`. This installs the toolchains under `~/gate-tools`, the login PATH, the git config and the gate's work root, `~/gate-remote` by default (`--root` sets another).
2. Then the System step, once, as an admin account with sudo: `sudo bash install-macos.sh --system --user {builder user} --root {absolute work root} --only System`. It keeps the Mac awake, turns on Remote Login and installs the work-folder prune job, and it checks that the work root from part 1 is the builder's own, so part 1 comes first and `--root` is absolute.
3. Then the Homebrew and FFmpeg steps, once, as an admin account in an interactive terminal (`ssh -t` works): `bash install-macos.sh --admin --only Homebrew,FFmpeg`, from the admin's own copy or piped in, since the builder's home is closed to other accounts.

The Mac's `root` in `remote.json` is that same absolute work root.

### Point this PC at the builder

1. Write `%LOCALAPPDATA%\gate\remote.json`. The minimal file, one Windows builder and one repo's patterns and outputs, is in [design.md, remote.json](design.md#remotejson).

   `host` is required and names the Windows builder whose work root is `D:\gate-remote`. On a Windows PC, also set `"ssh": "C:\\Program Files\\Git\\usr\\bin\\ssh.exe"` (an absolute path to an `ssh.exe`): Git for Windows' bundled OpenSSH sends large transfers several times faster and with far less CPU than the Win32-OpenSSH client; without `ssh`, gate uses the `ssh.exe` along its `PATH`. A Mac, or a further Windows builder with a work root of its own, is an entry of the `builders` array, with its `os`, its `root` and its own `repos`.
   Later changes to a repo's pattern lists need no hand edit: `gate remote add|remove <section> <repo> <pattern>...` changes one list, keeps the rest of the file as it is and backs the file up first ([design.md, `gate remote`](design.md#gate-remote)).
2. Run `gate push`. It copies the installed `gate.exe` and desktop helper to each Windows builder, into `C:\Program Files\gate\bin` (so the builder's ssh session must be an administrator's), registers the desktop task there and writes the builder's slot count, one `push: <host>: ...` line at a time. A Mac builds its [box](GLOSSARY.md), the program that runs routed work there, from the commit the install recorded, so install from a clean tree first.

   Run `gate push` again after every reinstall on this PC: a builder that runs another build of gate refuses routed runs, which then go to the next builder or run here.
3. Run `gate check --builder {builder}` from a repo. It runs the builder's preflight once and prints `check: {builder}: ...` lines and `install: {builder}: ...` hints, like `gate check` does for this PC. It exits 1 when the builder lacks something, cannot be reached, runs another gate build, or `remote.json` is missing or unusable.

### Route a repo

A heavy or light gated command routes when its repo is listed in `remote.json` and its words match one of the repo's patterns. A routed run that passes ends with `gate: passed in <s> s on <builder> (...)`. A command a pattern matches that gate keeps here says why, `gate: not routed: <why>`; a command no pattern matches stays here silently.

Adding a repo and running its routing trial are in [routing-a-repo.md](routing-a-repo.md#adding-a-repo), and trialling a new builder before it takes routed runs is in [Trialling a new builder](routing-a-repo.md#trialling-a-new-builder).

### Desktop runs and warm-ups

A routed command that needs the builder's logged-on desktop, such as a GUI test run, needs the autologon step, a `.gate/desktop.toml` pattern in the repo and the same pattern under `desktop` in `remote.json` ([routing-a-repo.md](routing-a-repo.md#a-repo-with-a-command-that-needs-the-builders-desktop)).

A new worktree's first routed run meets a cold work folder, and gate raises its ceiling for it. A repo whose short runs would still time out declares warm-up builds in `.gate/warmup.toml`, consents under `warmup` in `remote.json`, and runs `gate warm` in each new worktree before its first routed run ([routing-a-repo.md](routing-a-repo.md#a-repo-whose-short-runs-a-cold-folder-would-push-past-their-ceiling)).

## Troubleshooting

| Line | What it means and what to do |
|---|---|
| `install: missing: <piece>` then `install: no console to ask; pass -Yes to install them` | The toolchain is missing and the script cannot ask. Run it again with `-Yes`, or at a console. |
| `install: winget is not available; ...` | Install App Installer from the Microsoft Store, or install Rust and the Build Tools by hand from the links the line gives. |
| `install: the Build Tools need a restart to finish; restart Windows, then run install.ps1 again` | Restart, then run the script again. |
| `install: cargo build --release failed; see .tmp/install-build.log` | The build failed; the log in the clone says why. |
| `gate: no gate.exe found; running the command without slots or watchdog` | The launcher found no installed gate. Run `pwsh tools/install.ps1` in this repo. |
| `gate: --local needs --reason "<why>"`, `gate: --slot critical needs --reason "<why>"` | A local or critical run states why it runs that way. Add `--reason "<why>"`. |
| `gate: command exited 0 but its log reports a failure -> <log>` | A failure marker matched; the marker lines follow. A command whose passing output holds `: error ` takes `--no-markers`. |
| `gate: STALLED: ...` | The command made no progress for the stall window. Raise `--stall` for a command that is quiet for long stretches. |
| `gate: TIMED OUT ...` | The command ran past its ceiling. Raise `--timeout`, or give `--timeout auto:<seconds>`. |
| `gate: refused: the script that started this gate lost its caller (...)` | The shell running a background script was killed, and gate refuses the script's later steps. Start the script again from a live shell. |
| `gate: ignored <path>: <reason>; running locally` | `remote.json` cannot be used, so nothing routes. Fix what the reason names. |
| `gate: remote <host> version; running here` | The builder runs another gate build. Run `gate push`. |
| `gate: remote <host> unreachable; running here` | ssh to the builder failed. Check that `ssh {builder}` logs in without a prompt. |
| `gate: the snapshot push to <host> failed; running here` | The builder answered but the push of the snapshot failed for a reason other than the connection; the push's own lines above say why. With `(a partial clone lacks objects the push needs; git backfill fetches them)`, run `git backfill` in the repo. |
| `push: no installed gate at <path>; run tools/install.ps1` | Install gate on this PC before pushing it. |
| `push: <host> failed: <reason>` | That builder was not updated; the reason says why. The builder's ssh session must be an administrator's. |
| `check: not in a git repository` | `gate check` runs inside a repo. |
| `gate check: <host> is not a builder remote.json lists` | Use a `host`, `hosts` or `builders` alias from `remote.json`. |
| `prep: not elevated; run it from an elevated PowerShell (an administrator's ssh session is one)` | Run the prep from an elevated pwsh. |
| `prep: no console for the menu; run it at the console, over ssh -t, or with -Defaults or -Steps` | Use `ssh -t`, or name the steps. |

## Feedback

Report problems and questions as [GitHub issues](https://github.com/leftos/gate/issues).
