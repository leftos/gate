# Routing a repo

How a caller repo's heavy gates come to run on a builder: what goes in `remote.json`, how a routing trial runs, and what stays on the PC. The format of `remote.json` and the router's rules are in [design.md](design.md#remotejson); this page is the procedure.

## Which repos and which commands

Every repo that runs heavy gates is in scope, and every heavy command family in the gate journal ends in one of three states:

- it routes;
- it is booked with what blocks it (a GATE issue, High when it blocks the repo's adoption);
- it stays local, but only for one of two reasons, named beside the family.

  Either it cannot run anywhere but this PC (a command that names this PC's own WSL state, such as a `/mnt/x` path or a distro folder only this PC has, an install into this PC's own folders, or a git operation that writes this checkout's refs), or its run time is measured as shorter than what routing adds: the push, the outputs, the compression and the call overhead, taken from the repo's routed runs in the journal.

  Any command whose first word is `git` or `prek` stays local silently. A `git commit` of a repo that `remote.json` lists in `commits` first routes its pre-commit stage (below) when it is eligible, and then the commit itself runs here. Otherwise the commit stays local, its hooks' gates route by their own patterns (`hooks` in `remote.json`), and a hook command that runs without a gate is never seen by routing.

  A reason that work can remove does not keep a family local. It is a blocker to book. Examples: a tool or SDK the builder lacks, a window or GL the desktop route can serve, a `--local` whose recorded reason shows a routing gap, an outputs list that would need to grow (a routed `dotnet build`'s own project bin needs no entry: it comes back with the repo's list). A command that writes tracked files (a generated cache, a data file, committed screenshots) needs no `outputs` entry either: tracked files come back by the sync-back, and an outputs glob over them only adds manifest cost. Its trial checks that the routed run writes the same bytes as a local one, since a tool that reads this PC's own installs (a config folder, a user profile) can write different ones on the builder.

Coverage is judged from the journal's history of gated commands per repo, not from the commands guessed to matter. A light-slot run routes by the same entries, and stays here when it is provably faster here (its learned local p50 under 0.8 times the chosen builder's expected finish), so a repo's short light checks need an entry only when they should run on a builder.

A caller never adds `--local` (`-Local`) to a gate unless that command has been tried routed and failed. It then reruns locally only what failed and always reports the failure to the gate repo's inbox with the command, the gate's lines and the log path.

The gate repo fixes the routing or rules that the command stays local, and a command ruled local is kept local by the router through the repo's `.gate/local.toml` ([A repo with a command that stays on this PC](#a-repo-with-a-command-that-stays-on-this-pc)), so callers do not carry the flag.

A script that gates its own commands runs bare, not wrapped in an outer gate: a gate under a parent gate runs on this PC with it, never routed. Wrapped, each step that matches a routing pattern stays here and says `gate: this command matches routing pattern '<pattern>' but runs on this PC because its parent gate runs here; run the outer command without a gate around it to route this one` ([design.md, Routing](design.md#routing)).

Every `--local` run must give `--reason "<why>"` (the gate refuses it otherwise). It is journalled as `notRouted: --local` with the reason on its `queue` event, a light run's included, which is how stray uses are found (a command kept here by `.gate/local.toml` is journalled `notRouted: declared local`, never `--local`; a light run kept here for being faster is `notRouted: faster here`; a light run whose dotnet `--no-build` would be dropped is decided as a heavy run first, so it never says that and can route; a command none of the repo's patterns matches is `notRouted: no pattern`, said nowhere else). Its `queue` and `release` events carry `sentWords`, the text the patterns were asked about, whenever the gate asked them. The reason is also in its claim and ticket and on ops-tower's Gates page.

A command run from one worktree that names a path in another worktree of a listed repo stays local, saying `gate: not routed: the command works in <tree>; run it with --cd <tree>`. Run it again with `--cd <tree>` to route it from that worktree ([design.md, Routing](design.md#routing)). It does so even when none of the repo's patterns matches the command as written, provided one matches it with its paths rewritten against that worktree; otherwise it stays local silently, journalled `notRouted: no pattern`. A path in an unlisted repo, on a network drive or in no worktree keeps the plain `is outside the worktree` line.

A command whose word names an existing gitignored project input (a project, solution, `.targets`, `.props`, `.rsp` or `.runsettings` file, or a dotnet folder whose project files are all ignored) that no `inputs` glob covers routes and carries those files to the builder as inputs, with no `inputs` entry needed.

A gitignored `.targets` or `.props` file a word names also carries each file its text names by a quoted relative path, and each file those name in turn through imports; a project or solution file and a folder named that way are not carried, and a wildcard keeps the run here, since the gate does not follow one. A gitignored file of any other extension that a dotnet property value names (`-p:ProbeSource=.tmp/a.cs`) carries itself; one that a run's own logger or output option names does not.

A property value (`-p:`, `/p:`, `-property:`, `/property:`, `--property:`, or the switch and its value as two words) that names a carried file reaches the builder as `{gate-folder}/<path>`, and the box replaces the mark with the folder the command runs in, since MSBuild reads such a value from the project's folder.

A gitignored folder that a command names only by one file inside it (a page's `index.html` and its assets, say) is carried whole when the PC's `remote.json` lists a matching glob in `carriedFolders`: `"carriedFolders": {"<repo>": [".tmp/mocks/*-src"]}`, each entry a `/`-separated folder below the worktree root where `*` is any run of characters within one name and `?` one character, matched ignoring letter case as PowerShell `-like` does.

A mock capture such as `pwsh -NoProfile -File tools/mock-shoot.ps1 -Page .tmp/mocks/<name>-src/index.html -Out .tmp/mock-out/<name>.png`, with that glob and an `outputs` entry `.tmp/mock-out/**`, sends only the named page's folder, where an `inputs` glob would send every mock folder on every routed run.

The outermost matching folder is carried, gitignored files included; its files count under the `inputs` caps (not the 64 MiB and 2000 files below), and a folder past a cap keeps the run here. Only the main tree's folders are carried.

A file inside a carried folder travels only with its folder and counts toward neither of those caps.

A bad entry turns the feature off for every repo; a routed run says `gate: remote.json carriedFolders cannot be used: <problem>; no folder carried` and still routes, and `gate check` says so too ([design.md, remote.json](design.md#remotejson)).

A command whose gitignored files that its words name hold more than 64 MiB or 2000 files (a sibling pair's two trees counted together), or whose files cannot be sent (a file the inputs never send, such as a `.env`, among them) stays local, saying `gate: not routed: <path> is gitignored ...` with the reason, and is journalled `notRouted: gitignored input` ([design.md, Routing](design.md#routing)).

A repo whose scripts live in an initialized git submodule needs nothing declared: a routed run carries the submodule's files (tracked and untracked, not ignored) to the builder, a sibling pair's run those of each of its trees. A change a routed run makes inside the submodule is not brought back, and a submodule too large for one put keeps the run here ([design.md, Routing](design.md#routing), Inputs).

A sibling pair (a run carrying siblings; declared siblings that are all absent make a single repo) routes to a Windows or a macOS builder and sends the `inputs` globs of both its repos, each applying to its own tree, and carries a gitignored file the command names in either tree, a sibling's included (`..\<sibling>\x`). A carried file's build that reads a gitignored file in the other tree stays local unless that file is sent there too.

Both boxes take a pair's `inputs`: the macOS builder gets the same manifest a Windows one does, an initialized submodule's files included, and `--builder` naming it routes a pair that carries inputs there.

A routed run that fails on its builder names, in a `gate: hint:` line before the failure line, each gitignored file (never a folder) its words name that no `inputs` glob covers and that was not carried (a file in a carried folder included); the fix is an `inputs` glob for it (in the sibling repo's `remote.json` for a file of a sibling, named `../<sibling>/<path>`), or `--local`.

A gitignored project, solution or dotnet folder also stays local when its build reads a gitignored file the run would not send. That is a build file such as `Directory.Build.props`, `Directory.Packages.props`, `nuget.config` or `global.json` in a folder above it (the worktree root included), or a relative path in one of its carried project, props, targets or solution files that points outside its own folder ([design.md, Routing](design.md#routing) lists both).

The line ends `the build reads <file> above its folder, which is not sent` or `the build reads <file> outside its folder, which is not sent`. A throwaway build under `.tmp/` that leans on ignored build files around it therefore stays on the PC by itself, and its caller needs no `--local`; one whose `.targets` or `.props` names the files it compiles routes and carries them.

List a repo in `remote.json`'s `requireSiblings` when its build cannot work without its declared sibling (career-driver's cannot without godot-pad-claim). Its matched command then stays local, saying `gate: not routed: required sibling <name> is not beside the worktree` and journalled `notRouted: sibling absent`, instead of routing as a single repo that fails on the builder, and `gate check` counts the absent sibling missing. A value of `requireSiblings` that cannot be used requires every repo's siblings.

A repo not listed keeps the rule above: declared siblings that are all absent make a single repo. The worktrees of a listed repo must be cut as pairs, `<repo>.wt/<slug>/<repo>` with the sibling beside it, through the repo's nextup profile `siblings:` line ([design.md, Formats](design.md#formats)).

A routed command sees the PC's `HEAD` commits and `origin/<default>` (by name) for each repo of the run. The builder's checkout holds the gate's snapshot of the PC worktree, a commit with no parent, and the push also carries the PC's `HEAD` and the commit at its origin default branch, which the builder fetches as `origin/<default>`. A revision on neither (another branch, a tag, a stash) still fails there with `bad revision` or `bad object`. Such a command resolves the revision on the PC first and passes what it selects as words (godot-mcp's itest passes `-Groups a,b,...` picked on the PC), or is declared local ([A repo with a command that stays on this PC](#a-repo-with-a-command-that-stays-on-this-pc)).

A repo's first routed run to a builder pushes its reachable history in one pack, which for a large repo is big (yaat measured about 1.92 GiB), inside the push's 30-minute transfer ceiling. A shallow or partial clone sends no history refs, and a repo with no origin default branch sends no base.

## Adding a repo

1. Read the repo's own gate commands (its CLAUDE.md, nextup profile, justfile or package scripts) and the tools each needs: toolchains and their pins (`.gate/toolchain.toml`, `rust-toolchain.toml`, `global.json`), shells and utilities, package managers.
2. Set the builder up ([Builder setup](#builder-setup)). A session installs what a routed repo needs, at the repo's pinned version, and changes the builder's configuration (PATH, ACLs, environment) directly over ssh.

   A change to the builder's toolchains, tools, work root or git config is also made in `tools/builder/install-toolchains.ps1` in the same item, so a new builder gets the same setup. A change to the machine's provisioning goes to claude-config as a repo inbox note with what changed and the exact commands. Run `gate check --builder <alias>` from the repo (the builder's `remote.json` host).
3. Add the repo's patterns with `gate remote add repos <repo> <pattern>...`, plus `outputs`, `failureOutputs`, `inputs`, `siblings`, `desktop` and `desktopSession` entries the same way where the repo needs them ([design.md, `gate remote`](design.md#gate-remote)). It backs `remote.json` up beside it (`remote.json.bak-[<tag>-]<UTC stamp>`), re-reads the file just before writing and changes only that one list: other sessions edit it too. `warmup`, `hooks`, `commits` and `sendBuild` consent are flat lists it does not edit; add those by hand, after a backup of your own. `gate remote match <repo> "<text>"` prints the first pattern of the repo that matches a text (a journal line's `sentWords`) and exits 0, or prints nothing and exits 1.

   Routing is opt-out: a repo routes each test, lint, check and format verb whole (`cargo nextest run *`, `dotnet test *`), and what must stay here goes in its `.gate/local.toml` ([A repo with a command that stays on this PC](#a-repo-with-a-command-that-stays-on-this-pc)). A build routes whole when `outputs` bring back everything this PC uses from it; otherwise its pattern names the builds that qualify.

   `sendBuild` is an array of repo names: a routed `dotnet test <project> -c <configuration> --no-build` of a listed repo sends the PC's build of that project to the builder and keeps `--no-build` there, whether or not it was built here. An unlisted repo has the word dropped and the builder builds, unless the command is a `dotnet test` or `run --no-build` of a project whose last build ran on this PC (the built-here record): that run sends the PC's build too, or runs here keeping its word when it cannot be sent. A listed repo's other `test` and `run --no-build` runs follow the same record ([design.md, Routing](design.md#routing)).

   A test report under a `TestResults` folder (an MTP TRX) comes back only through a glob that names the `TestResults` folder, such as `tests/*/bin/*/*/TestResults/*.trx`. `**` and `*TestResults*` bring back nothing under it.

   A routed run copies back its `outputs` only after a passing command. A failed command (exit other than 0) copies back only its `TestResults` globs' files and the files under `failureOutputs`, an object of repo to globs beside `outputs` (the same glob rules and `[`/`]` ban; a `builders` entry's own list wins for a run on it; absent by default, no derived globs), such as `{"yaat": ["**/*.dmp"]}`.

   An `outputs` or `failureOutputs` glob brings back a file below a `.tmp` folder only when its folder segment is exactly `.tmp` (`.tmp/dumps/**`, `**/.tmp/x.json`; `**` and `.t*p/x` do not reach), and only a file this run created or changed. The gate's own `.tmp` files never come back.

  A failure output over 256 MiB, or past 1 GiB in all, stays on the builder and the box says so (`gate: remote failure output left on the builder: ...`). A run whose command the builder kills at its ceiling brings back the same set as a failed command (its `TestResults` globs' files and the `failureOutputs` files, never the full `outputs`) and still ends 124. A run the PC cuts by a stall or the PC backstop brings the same set back after its stop and still ends as the cut's verdict. A run whose connection is lost after its command started, before the builder said its own end, brings the same set back and still ends lost. A builder backstop kill, a restarted or unreachable builder, a lost connection after the builder's own end lines and Ctrl+C bring nothing back.

  The gate cannot tell a whole file from one half-written at the kill, so a command whose outputs are worth bringing back after a kill writes each one to a temporary name and renames it into place when it is whole.
4. Run one routing trial (below). The repo counts as routed once its trial passes.

## Builder setup

A builder's setup has two halves. The machine half is the builder prep, `tools/builder/prep-windows.ps1` (or `prep-macos.sh` for a Mac): a checklist of step scripts for the builder account, OpenSSH, the network, the `C:\build-box` ACL, the WSL platform and its reboot, Developer Mode, the Application event log ACL, autologon, Defender, the Dev Drive and sleep.

Each step, its default and what skipping it costs are in [getting-started.md, Prepare the machine](getting-started.md#prepare-the-machine).

The owner's builders were provisioned by a private variant of the same half, which also covers machine policy gate does not need (the Windows Update policy, remote desktop and diagnostic tools).

This repo's `tools/builder/install-toolchains.ps1` does the rest: the toolchains and tools, the sccache server and WSL keep-alive tasks, the git config for gate runs, the MSYS keeper task, the gate work root and the work-folder prune task.

- **Order on a new builder:** the prep, then the installer, then `gate push`.
- **Copy:** `scp tools/builder/* {builder}:C:/Users/{builder user}/` puts the scripts in the builder user's home, since the prep's `C:\build-box ACL` step leaves `C:\build-box` writable by Administrators and SYSTEM only. From the elevated session, copy the exact files (`install-toolchains.ps1`, `wsl-setup.sh`, `windows-update.ps1` and `virtual-display.ps1`, never a wildcard) into `C:\build-box\gate\` before running the installer. The other three must sit beside the script.
- **Run:** over ssh, elevated (an administrator's ssh session), under Windows PowerShell 5.1: `powershell -NoProfile -ExecutionPolicy Bypass -File C:\build-box\gate\install-toolchains.ps1 -User {builder user} [-GateRoot D:\gate-remote] [-SccacheDir D:\sccache] [-SccacheCapBytes N] [-Only {step prefix},...]`.
- **Parameters:** `-User` is required and names the builder user: the scheduled tasks run as it and the write-deny ACLs name it. `-GateRoot` is the gate's work root, `-SccacheDir` sccache's disk cache and `-SccacheCapBytes` its size cap. `-Only` runs just the steps whose names start with one of the given prefixes, then the version summary (`-Only Summary` runs only the summary).
- **As the builder user:** run it in the builder user's own ssh session, since the user-profile installs go to the running user. Run it only while no gate is running on the builder.
- **Exit codes:** a missing or unknown `-User` exits 2, a `-Only` prefix that matches no step exits 2 before anything is written, and a session that is not elevated exits 1 with `install-toolchains: not elevated; ...`.
- **Steps**, each a valid `-Only` prefix, run in this order: Git, PowerShell, .NET SDK 10.0.401, .NET SDK 8.0.425, .NET Framework 4.8 Developer Pack, .NET workload wasm-tools, Node.js, uv and prek, Python, Visual C++ Redistributable 14.51, Visual Studio Build Tools 2022 (C++), Rust, cargo-nextest and just.

  The remaining steps are WSL Ubuntu (registers and starts the `gate-wsl-keepalive` logon task and waits for the VM before the elevated setup), sccache, Git usr\bin on the builder user Path, Bun, Godot, VB-CABLE (a virtual audio output: the builder's sound cards have nothing plugged in, and Godot falls back to its Dummy audio driver without an active render endpoint), Blender (in the PC's install folder, so a job that names the `blender.exe` path still runs; it also sets the machine variable `GATE_BLENDER_ROOT` to `C:\Program Files\Blender Foundation`, where the box finds the `Blender 5.2` folder).

  After them come FFmpeg, zstd (for ad-hoc compressed copies to the builder; the gate's own transfers compress without it), NSIS (rustling-tulip's `rt.ps1 installer` runs its `makensis`), pnpm, Playwright browsers (1.63.0, then 1.64.1's Chromium), Git system config for gate runs, Smart App Control off, keeper: MSYS sleep task, keeper: MSYS console sleep task (a second MSYS keeper in the console session, where desktop runs start their commands), Gate work root, Work-folder prune task, Windows Update drained install, Virtual display and Summary.
- **Blender config (from the PC, not the installer):** a Blender job reads the builder user's Blender preferences and add-ons, so copy this PC's `%APPDATA%\Blender Foundation\Blender\5.2` to the same path under the builder user's profile (`C:\Users\{builder user}\AppData\Roaming\Blender Foundation\Blender\5.2`). A job run with `--factory-startup` reads none of it. See [A repo that runs headless Blender](#a-repo-that-runs-headless-blender).
- **Tasks:** `\build-box\sccache-server`, `build-box-msys-keeper`, `build-box-msys-console-keeper`, `gate-wsl-keepalive` and `\build-box\gate-prune-work`. These five run as the builder user at the Limited run level: all but `gate-wsl-keepalive` and `build-box-msys-console-keeper` by S4U, whether or not the user is logged on, and those two in the user's interactive logon. The sixth, `\build-box\gate-windows-update`, runs as the builder user by S4U at the Highest run level (see its bullet). `\build-box\gate-virtual-display` runs as SYSTEM, and `\build-box\gate-display-mode` in the builder user's interactive logon at the Limited run level (see the Virtual display bullet).

  The sccache server's log is `%LOCALAPPDATA%\build-box\sccache-server.log` in the builder user's profile, not under `C:\build-box`; the step refuses a `LOCALAPPDATA` that holds a quote or bracket.
- **User tools:** a user-level tool a routed command runs from the PC user's home (claude-config's `tools/mock-three`, say) is kept on each Windows builder by `gate push`, not by the installer.

  List it in the PC's `remote.json`: `"userTools": {"<repo>": [{"path": "~/.claude/tools/mock-three", "commands": ["<pattern>"]}]}`, each `path` a folder below the home and each pattern one of the command patterns that use it ([design.md, remote.json](design.md#remotejson)). `gate push` writes the tool's tracked files (`git ls-files` in its folder on the PC) at the same home-relative path in the builder user's profile, so a caller's tracked wrapper that runs it routes.

  The installer has no step for it, so on a new builder the order is the installer (which installs pnpm and the Playwright pin's Chromium, the `Playwright 1.64.1 browsers` step), then `gate push`.

  A push deletes only the files its last push wrote (the `.installed-files` manifest beside the tool), keeps the copy's `node_modules`, runs `pnpm install --frozen-lockfile` when the lock changed or `node_modules` is missing, and writes `.installed-from` (`<fingerprint> <lock>`) last. A change to the tool on the PC reaches the builders on the next `gate push`.

  A routed run whose command matches an entry compares this PC's fingerprint of the tool with the builder's `.installed-from`. A copy that is missing or differs refuses that builder for `tools`, saying `run gate push`, and another builder may take the run; a copy this PC cannot read keeps the run here. A macOS builder gets no user tools and is refused the same way. A push that rewrites a tool while a routed run uses it is not barred, so push between runs.
- **Work-folder prune task:** the step `Work-folder prune task` (`-Only Work-folder`) registers `\build-box\gate-prune-work`, which runs every 6 hours from 00:17 with a 2 h limit, IgnoreNew and StartWhenAvailable. Its command runs inline, with no script file and module auto-loading off: `C:\Program Files\gate\bin\gate.exe box prune --root <GateRoot>` ([design.md, The box side](design.md#the-box-side), `prune`).

  Under a standing drain barrier the prune touches nothing. After its sweep it runs `git gc --auto` in each bare repository under the work root, which a push no longer starts (a push to the builder runs with `receive.autogc` off).

  Its output goes to the Application log under the event source `gate-prune`, event ID 1: each output line is an Information event, and a closing event `gate box prune exited {code}; {drive}: free {n} GB` (`(no exit code)` when there is none) is Information on exit 0 and Warning otherwise.

  The step logs `event source gate-prune: already registered` or `registered in the Application log`, and `task \build-box\gate-prune-work registered: ...`, adding that the task `will fail until a gate push installs C:\Program Files\gate\bin\gate.exe` when that exe is not there yet.
- **Windows Update drained install:** the step `Windows Update drained install` (`-Only Windows`) makes Windows Update download and never install or restart on its own: it sets the policy `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU` to `AUOptions` 3, `NoAutoUpdate` 0 and `NoAutoRebootWithLoggedOnUsers` 1. Windows Update's own 03:00 install once swapped .NET under a live routed run.

  It copies `tools/builder/windows-update.ps1` to `C:\Program Files\gate-maintenance\` with a `windows-update.root` file naming the work root, and refuses a folder that anyone but SYSTEM, Administrators and TrustedInstaller can write, since the task runs the script at the Highest run level. It registers `\build-box\gate-windows-update`, daily at 03:00 as the builder user (S4U, Highest) with a 12 h limit, IgnoreNew and StartWhenAvailable, running `powershell.exe -File` on that script.

  The script raises the drain barrier through `gate.exe box drain --stdin`, re-raises it every 300 s while it waits for the box to say drained, and gives up after 2 h with nothing installed. It then installs the downloaded Windows updates (no feature upgrades), never reboots, and always lifts the barrier. A pending reboot is a Warning, `a reboot is pending; it waits for the owner`.

  Its output goes to the Application log under the event source `gate-windows-update`, event ID 1. Like the prune task, it will fail until a `gate push` installs `C:\Program Files\gate\bin\gate.exe`.
- **Virtual display:** the step `Virtual display` (`-Only Virtual`) keeps a live display on a builder with no monitor attached. Without one, the console session has no active display, and a desktop run's screen capture records a flat frame. The builder's only display adapter is the Amyuni `usbmmidd` virtual display driver that RustDesk installs (the machine's provisioning), and that driver adds a monitor only when asked. The step refuses a builder without the driver.

  It copies `tools/builder/virtual-display.ps1` to `C:\Program Files\gate-maintenance\`, refusing a folder or file anyone but SYSTEM, Administrators and TrustedInstaller can write, and sets the AC display timeout to never.

  It registers two tasks:
  - `\build-box\gate-virtual-display` runs the script as SYSTEM at startup and every 5 minutes. Its `ensure` plugs one virtual monitor in through the driver's own device call when none is plugged in, and does nothing otherwise. That also puts the monitor back if a RustDesk session removes it.
  - `\build-box\gate-display-mode` runs the script's `mode` at the builder user's logon. It sets that monitor to 1920x1080: the driver first brings a monitor up at 1024x768, and once it is set, Windows keeps 1920x1080 for that monitor, so a monitor plugged in again comes back at it.

  The step then runs the ensure task once and waits for it, starts the mode task, and the version summary logs the script's `status`, `virtual display -> virtual-display: adapter present, N virtual monitor(s)`.

  Nothing in the script restarts the machine or a device. Its events go to the Application log under the event source `gate-virtual-display`, written only when it plugs a monitor in, sets the mode or fails.
- **Gate work root:** the step refuses a root that is a link (reparse point) or a file. When the root's security is not already right it sets the owner to Administrators (`icacls /setowner *S-1-5-32-544`) and applies a protected DACL: SYSTEM, Administrators and the builder user with full control, inherited by objects and containers, no mandatory label.

  The box's preflight judges the same state before it touches anything and refuses a root another account may write in, a folder above it another account may delete, a reparse point at the root or its parent, a `subst` drive and a network volume.

  It says `gate: the builder's work root is not locked down: <why>; run the installer's "Gate work root" step (install-toolchains.ps1 -Only "Gate work root")`, and a routed run then runs here (`gate check --builder` says it as a missing item). A root open in another process with delete access is only busy: retry when it is closed.

  It then reads the security back and fails unless it is exactly that, owner included; when it already is, it logs `already protected` and changes nothing. A routed command's restricted token passes access checks through the builder user's own SID, so neither Authenticated Users nor Users needs a grant.
- **The `C:\build-box` rules:** the prep's `C:\build-box ACL` step gives `C:\build-box` a protected DACL: Administrators and SYSTEM with full control, the builder user with read and execute, inherited by files and folders. The owner's private variant also re-asserts that DACL on every run and strips explicit ACEs under it, so the gate half re-applies anything it needs under `C:\build-box\gate` on every run: an ACE set on the root that inherits survives, an explicit ACE on a child does not.

  The gate half adds no SYSTEM or highest-run-level task whose action names a script under `C:\build-box`. A new one needs that script kept where a routed command cannot replace it, since routed commands run as the builder user.
- **Smart App Control off:** in Evaluation, Smart App Control asks the cloud for the reputation of every unsigned image the first time it is mapped, whatever the Defender exclusions, so each freshly built exe in the work root waited 1.5-16 s before its first start (0.05 s with it off).

  The step sets `VerifiedAndReputablePolicyState` to 0 and refreshes the policies with `CiTool`; Defender's real-time, cloud and behaviour protection stay as they are. A policy still enforced after the refresh asks for a restart of the drained builder.
- **The build volume out of scheduled optimization:** the Windows builder's scheduled storage optimizer (`\Microsoft\Windows\Defrag\ScheduledDefrag`, run by Automatic Maintenance) once froze file I/O on the build volume (ReFS, holding the work root) for up to two minutes. A box process froze while it held the drain lock, and routed runs failed or stalled.

  The task's arguments are set to `-e D: -h -o -$` in place of the default `-c -h -o -$`; `defrag /e` optimizes every volume except those listed, so name the build volume (here `D:`). The "Optimize Drives" app rewrites these arguments when its drive selection is saved, which can bring the volume back, so check them after anyone saves that selection there.

  Evidence for such a freeze on the builder: its Security log audits process creation (events 4688 and 4689), the best record of which box verbs ran when; the ReFS/Operational log's event 147 records file I/Os over 30 s; the box writes nothing to the Application log. A box whose drain lock stays held past its retry refuses the call as `busy` and the run goes to the next builder or here ([design.md, The box side](design.md#the-box-side), `drain`).
- **Test:** `pwsh -NoProfile -File tools/builder/test-install-toolchains.ps1`, `pwsh -NoProfile -File tools/builder/test-windows-update.ps1` and `pwsh -NoProfile -File tools/builder/test-virtual-display.ps1`, each run through `tools/gate.ps1 -Slot light`. They install nothing and need no admin.
- **The Unreal engine install is not in the installer.**

### A macOS builder

A macOS builder (an Apple-silicon Mac, a dedicated builder nobody works on) is set up by `tools/builder/install-macos.sh`, run as the builder user over ssh.

The box gets onto the Mac by `gate push`, which builds it there ([design.md, `gate push`](design.md#gate-push)): the Mac needs git, cc (the Xcode Command Line Tools, `xcode-select --install`) and the Rust step below, and the PC needs a clean, committed `tools/install.ps1` install, whose `source.json` names the commit the Mac builds. The box lands in `<root>/bin/gate-box`, and the build's checkout, target folder and `build.log` live in `<root>/gate-push`.

`remote.json`'s `builders` array (an entry with `os` `macos` and a `root`) is read and routes ([design.md, remote.json](design.md#remotejson)), and the transport, payload layout, path map and PC caches follow the entry's profile.

A Mac entry may carry `outputs`, and its box brings them back like a Windows builder's (a route whose output is a TRX, as yaat's, works); the project bin a Windows builder's routed `dotnet build` adds to a repo's own list is not derived for a Mac.

A run on a builder entry brings back only what that entry's own `outputs` and `failureOutputs` list for the repo: the main lists are not inherited, so a repo the entry leaves out brings nothing back from it. Adding a pattern to an entry's `repos` for a command that makes files therefore adds the files' globs to the same entry's `outputs` and `failureOutputs` (`gate remote add outputs <repo> <glob> --builder <host>`), as career-driver's Blender jobs needed `.tmp/blender-out/**` on the Mac's entry.

A Mac's routed build brings Windows outputs back only when the entry opts the repo in with `crossBuild`, an object of repo name to a Windows runtime identifier (`"crossBuild": {"app": "win-x64"}`), which needs the repo's `outputs` list on the same entry and is for a `macos` entry alone ([design.md, remote.json](design.md#remotejson)).
The Mac then runs `dotnet build <target> -c <configuration> -r <rid>` after a passing `dotnet build` or `dotnet test` (without `--no-build`) and brings `bin/<config>/<tfm>/<rid>/` back as `bin/<config>/<tfm>/`; a command with `-o`, `--output`, `--artifacts-path`, `-a`, `--arch`, `--os`, `--ucr`, `--use-current-runtime`, `-r`, `--runtime` or an option word the router does not know gets none, and says so.
A bad `crossBuild` value drops only that entry's cross builds (`gate check` prints a `check: here: remote.json: builders[N].crossBuild…` line for it and fails), and a cross build that fails by its own exit leaves the command's status, says the Windows outputs were not brought back, and makes the next `--no-build` build first.
A cross build the box kills for its ceiling or backstop ends the run 124, and one that ends by a signal or cannot be seen to its end ends it 1.
A build whose outputs are launched on the PC and that is not opted in stays off the Mac by the `remote.json` entries alone.

The words a macOS builder is sent are spelled for the Mac: the recognised paths use `/`, and a relative word written with `\` (`src\Duel.sln`) is sent with `/` when the snapshot carries that file or folder, in its exact letter case.

A macOS builder takes LFS content like a Windows one: a run whose snapshot holds LFS pointers goes there when it is the first pick, with the same objects sent and checked out.

A word that still holds a `\` (a regex, a list, script text after `bash -c` or `pwsh -Command`, a path the repo does not carry) keeps the command off the Mac: it runs on a Windows builder or here, silently. A Windows builder is sent the words as written. Details: [design.md, Routing](design.md#routing).

- **Private home** runs first, before every toolchain step: it sets the builder's home to mode 700 (logging `<home> had mode <mode>; set to 700` when it changed), since the package restore's caches (`~/.npm`, `~/Library/pnpm/store`, `~/.bun/install/cache`) are made at 755 and 644 and every standard macOS account is in the home's group `staff`. It refuses a home that is a symlink or owned by another account.
- **Toolchain steps** need no sudo and land under `--prefix` (default `~/gate-tools`), Rust under `CARGO_HOME` and `RUSTUP_HOME`: .NET SDK 10.0.401 and 8.0.425, the wasm-tools workload, PowerShell, Node.js, pnpm, uv and prek, Rust, cargo-nextest, just, Git LFS, Godot 4.7.2 mono, Blender 5.2.2 LTS, Bun.
- **Homebrew and FFmpeg** come from Homebrew, since no official source publishes a macOS arm64 FFmpeg build with a checksum. The FFmpeg step also installs zstd, for ad-hoc compressed copies to the builder. Homebrew's installer needs an admin account and refuses root, so these two steps run as an admin account, never the builder user or `sudo`: `bash install-macos.sh --admin --only Homebrew,FFmpeg`, interactively (over `ssh -t`), since the installer may ask for the admin's password.

  The builder's home is mode 700, so the admin cannot read an installer copy kept there: it runs its own copy, or pipes the script in (`ssh <admin> 'bash -s' -- --admin --only Homebrew,FFmpeg < install-macos.sh`). Without `--admin` they print the commands they would run. Auto-update and analytics are off for the builder, and its login PATH puts `/opt/homebrew/bin` after the pinned tools, so a pinned tool always wins.
- **Login PATH** writes a marked block to `~/.zprofile`. The box reads its commands' PATH from the builder user's login shell, since a non-login ssh command lacks the prefix's tools.

  The block also exports `GATE_GODOT_ROOT` (`<prefix>/godot`, one folder per Godot version holding `Godot.app`), which the box reads from the same login shell to probe the builder's Godot and to run a pinned version's install for a Godot console command.

  It exports `GATE_BLENDER_ROOT` (`<prefix>/blender`, one folder per Blender version, `5.2.2`, holding `Blender.app`) the same way, for the box's blender probe and a pinned blender's run.

  The step ends by checking that a fresh `zsh -l` finds `dotnet`, `pwsh`, `node`, `cargo`, `pnpm`, `npm` and `bun`, with `pnpm` and `npm` under `<prefix>/node/bin/` and `bun` under `<prefix>/bin/`.
- **Blender config (from the PC, not the installer):** copy this PC's `%APPDATA%\Blender Foundation\Blender\5.2` to the Mac's `~/Library/Application Support/Blender/5.2` as the builder user; a job run with `--factory-startup` reads none of it. See [A repo that runs headless Blender](#a-repo-that-runs-headless-blender).
- **Git config** sets `core.autocrlf false` and `gc.pruneExpire never`. **Gate work root** (`--root`, default `~/gate-remote`) makes the root, `work` and `bin` at mode 700; the box refuses a root another account may write in.
- **System** runs only as `sudo bash install-macos.sh --system --user <builder user> --only System`: no sleep, restart after power loss, wake on network, the open-file limits, Remote Login, Spotlight indexing off on every volume (`mdutil -a -i off`), and Rosetta 2, which runs the x86_64-only macOS tools some NuGet packages ship (Grpc.Tools' `protoc`, which yaat's build needs). Without `--system` it prints the commands it would run.

  It also installs the launchd job `dev.gate.prune-work` (`/Library/LaunchDaemons/dev.gate.prune-work.plist`), which runs `<root>/bin/gate-box box prune --root <root>` as the builder user at load and every 2 hours and logs to `/Library/Logs/gate/prune-work.log` ([design.md, The Unix box](design.md#the-unix-box), `prune`).

  The step runs as `{builder admin}` with `sudo`; it checks the log folder and log, then that `<root>` and `<root>/bin` are the builder's own folders at mode 700, before it writes the plist, so run the Gate work root step as `{builder user}` first and give an absolute `--root` with `--system`.

  A builder without `<root>/bin/gate-box` yet gets the job and a log line saying it fails until `gate push` installs the box. To check the job after a push, run `sudo launchctl kickstart system/dev.gate.prune-work`, then read `/Library/Logs/gate/prune-work.log`: it ends with `gate: remote prune removed <n> of <m> work folders`.
- **Not in the installer:** FileVault off and automatic login for the builder user, so the Mac logs in and comes back by itself after a reboot or a power cut.

  `sysadminctl -autologin set` fails with error 22 on macOS 27, so automatic login is set through `/etc/kcpassword` and the `autoLoginUser` login-window preference; the prep's `filevault-off` and `autologin` steps (`tools/builder/prep-macos.sh`) do both. After a reboot, `stat -f %Su /dev/console` names the builder user once it has logged in.
- **Flags and exits** follow the Windows installer: `--only <prefix>,...` (case-insensitive prefixes, then the summary; `--only Summary` alone), `--list`, exit 2 on a bad argument or a prefix that matches no step, exit 1 when a step failed. Every step logs `OK`, `SKIP` or `FAIL` to stdout and `<prefix>/install.log`.
- **Slots:** the entry's `slots` is set by memory, not cores. Several copies of yaat-server's whole `dotnet test` suite (about 4,900 tests, 1.2-1.3 GB each), started together on a 15-core, 24 GB Mac mini, gave 1.64x one run's throughput at 2, 2.66x at 4, 3.3x at 6 and 3.74x at 8, with memory pressure normal and no swap throughout; but at 8 the Mac had under 300 MB unused against about 2 GB at 6, and one run took 2.1 times as long as alone (1.8 times at 6).

  The entry uses 6: about 90% of 8's throughput with room for a heavier suite. A cargo build or clippy run uses every core and far more memory per job, so a Mac that takes routed Rust work needs fewer slots; measure again before routing one.

  To measure, start N copies of a routed repo's suite at once, each in its own folder and `TMPDIR`, and sample `memory_pressure -Q`, `sysctl vm.swapusage` and the load every 5 s; count as foreign any `cargo`, `rustc`, `nextest`, `dotnet` or ssh-started shell outside the round's own processes, since a `gate-box` check alone misses direct runs, and take the Mac off routing (`"routed": false`) while it runs.
- **Test:** `bash tools/builder/test-install-macos.sh`, on this PC or the Mac; it installs nothing. `bash tools/builder/test-build-box-macos.sh` runs the script `gate push` sends (`MAC_SCRIPT`) against a scratch root with a real cargo build of the box; it runs on macOS only.

`gate check --builder` hints at the installer for a missing node, pwsh, ffmpeg, netfx-sdk or a rust with no minimum: the hint names `tools/builder/install-toolchains.ps1` and points to this section.

### Several builders

List each further builder in `remote.json`'s `builders` array, beside `host`. A builder that refuses a routed run or cannot be reached hands the run to the next builder that matches the command, then `host`, and the PC runs it only when none takes it (a desktop run fails instead, see below).

Which refusals hand the run on, the order, the one wait shared by every builder and the lines the run says are in [design.md, The next builder](design.md#routing); a repo owner adds nothing beyond the list.

A new builder is trialled before it joins that list ([Trialling a new builder](#trialling-a-new-builder)).

### Trialling a new builder

Add the builder as a `builders` entry with `"routed": false` (its `os`, `root` and `slots`; `repos` may stay empty or missing). No unpinned run picks or tries a not-routed builder, so the repos' ordinary runs are unaffected. Then run `gate push`, and `gate check --builder <alias>` from the repo.

Route trial commands to it with `gate run --builder <alias> --slot heavy -- <command>`. The pin needs the repo listed in `remote.json` and a command matching its patterns there; list the repo under the not-routed entry itself to trial it on patterns of its own, without routing it anywhere else. The builder's own refusal or an unreachable builder fails the run (exit 1) instead of running here, and a pin the decision itself refuses exits 2, so a trial that passes ran on the new builder.

A builder entry that lists a repo under its own `repos` also narrows that repo's pins to it: a `--builder` pin to that builder matches only the patterns listed there, even an empty list, and never the repo's main patterns. To route one command to a single builder in a repo whose other runs are already pinned there (a macOS-only script beside `cargo` runs pinned to the Mac), put its pattern in the repo's main list and pin the command, rather than listing it alone on the builder's entry. Listing it on the entry hides every other pattern from those pins. The builder's `outputs.<repo>` has no such effect.

Once the trials are clean, set `routed` to `true` (or drop the key) and list the repos the builder runs ([design.md, remote.json](design.md#remotejson)).

## The routing trial

A trial runs the repo's full command routed, once, and assumes a local run passes: it never runs the full command locally to compare. If the routed run fails, only the failing steps or tests are rerun locally, with the narrowest command that reproduces them:

- **A step that fails locally too** is the repo's own failure. The pattern stays routed, since routed and local agree, and the failure goes to the repo as an inbox note.
- **A step that passes locally but fails routed** is a routing failure. It is fixed in the builder's setup and that step alone is retried routed, once. If it still fails, the repo's patterns come back out of `remote.json` and the failure is booked as a GATE issue.
- **A routed `dotnet test` that finds no test project** (`No test projects were found.`): `dotnet test` in Microsoft.Testing.Platform mode classifies a project by `IsTestingPlatformApplication`, not `IsTestProject`, so read that property first. It evaluates empty when the project's restore files name a package folder the builder does not have, as a sent build's do until the box restores the project.

A first routed run in a new work folder is cold, as is the first run after the builder cleared what a killed or lost run left in it, and a pair's first routed run (the folder has no pair layout yet). The gate raises its ceiling to `min(2 x ceiling, ceiling + 1800 s)` and says so. The trial still wants a `--timeout` near what a cold build takes, since the raise is at most half an hour more; the repo's ordinary routed runs keep their usual ceilings. Run one heavy trial at a time.

## A repo with a command that needs the builder's desktop

A repo with a routed command that needs the builder's logged-on desktop (a GUI test run) declares its `-like` patterns in a tracked `.gate/desktop.toml`, `patterns = ["dotnet test *Ui*"]`, at most 32, matched against the same rewritten words as the repo's `repos` patterns.

The PC consents in `remote.json`: `"desktop": {"<repo>": ["dotnet test *Ui*"]}`, each pattern listed exactly as the file spells it, letter case included. The first declared pattern a command matches decides, and a command no pattern matches asks for no desktop.

A desktop run never runs on the PC: a reason found after the pattern matched that would keep an ordinary command here (a siblings problem, a word outside the worktree, nothing to send, gitignored inputs it cannot carry, a fact that cannot be read, a word that is not valid Unicode, only macOS builders left) refuses it instead, `gate: not routed: <the reason>; a desktop run never runs here`, exit 2, before anything runs. A light desktop run is never kept here as faster here. `--local`, a `.gate/local.toml` match, a pattern without consent and an unusable `.gate/desktop.toml` still keep it here.

When no builder can run it, a desktop run fails with exit 1 instead of running on the PC: `gate: remote <alias> <reason>; a desktop run never runs here` after the builders that can take it have each been tried (a full pool, a busy work folder key, an unreachable or outdated builder hop on first). A desktop helper of another gate build than the box's, as a `gate push` that landed while the box waited for the desktop leaves, says `gate: remote <alias> replaced; trying <alias> again` and is tried once more on the same builder (a second `replaced` ends the run like the others). A busy desktop that stays busy for the run's ceiling says the box's `gate: not routed: the builder's desktop stayed busy for <n> s` first. A connection lost after the command started says `rebooted` or `unreachable` `after the command started; it may have run there`, and the run tries no other builder. `--builder` keeps its own `--builder given` line.

A locked desktop run waits for the builder's desktop up to its ceiling, then fails; it can wait that long and then run that long, so give it a ceiling that covers both. It first waits its turn in this PC's desktop queue, holding no work folder key and no remote slot, so one locked desktop run at a time of this PC (and no session or hidden run) is past the queue: other repos' runs keep the remote slots and an ordinary run of the same worktree is not refused `busy` meanwhile (`gate: waiting in this PC's desktop queue behind another locked desktop run`). Runs from another PC still wait at the builder.

Its waits in the queue, for the PC's remote pool and slots and for the work folder's key come out of that same budget, where a heavy run waits 60 s for the pool and its ceiling for the key, a light run waits 60 s and 90 s, and it waits out a `gate warm` holding the key. A light run a critical request reroutes to a builder is not treated as a desktop run.

Without the consent the command runs here with `gate: not routed: desktop pattern '<pattern>' not allowed in remote.json`.

A `.gate/desktop.toml` the gate cannot use keeps every routed command of the repo here with `gate: not routed: .gate/desktop.toml: <problem>`, said ahead of any later reason, so fix it before relying on routing. The one locked desktop run past the PC's queue holds a remote slot while it waits for the desktop, up to its own ceiling, and the waiting box names the run that holds the desktop. Format and rules: [design.md](design.md#remotejson).

### A command that needs only the console session

A command that needs the builder's console session (its logon session, where MSYS programs and a GUI-capable process start) but never touches the desktop itself is declared under a second key, `session = ["dotnet test *Headless*"]`, beside `patterns` (either may be absent, at most 32 each, no pattern in both, letter case aside). The PC consents under its own key, `"desktopSession": {"<repo>": ["dotnet test *Headless*"]}`, each pattern exact; without it the command runs here with `gate: not routed: desktop session pattern '<pattern>' not allowed in remote.json desktopSession`. A command matching both a `patterns` and a `session` pattern is a locked run.

A session run is handed to the console session like a desktop run but never takes the desktop lock, so several session runs, and one locked run, can run on the builder at once and none waits for another. In return the repo promises, for every command a `session` pattern matches:

- It opens no window on the console desktop from any descendant. A window on a desktop the command creates itself (godot-mcp's hidden-desktop itests do) is fine.
- It raises no UAC prompt.
- It uses per-run names for credentials and named objects (mutexes, pipes, ports, temp folders), since two worktrees or PCs of one repo can now overlap where the lock queued them.

A locked console or an input desktop other than `Default` still refuses a session run, which then fails like any desktop run rather than running here. Concurrent session runs' MSYS programs start reliably because the builder's console-session MSYS keeper task (`build-box-msys-console-keeper`, in the builder steps above) keeps an MSYS process alive in that session.

Commit a `session` key only after the reinstall that carries this change, on the PC and (through `gate push`) on the builders: an older gate.exe reads the key as a `.gate/desktop.toml` problem (`session is not a key (patterns)`) and keeps all of the repo's runs here.

### A command that runs on a hidden desktop

A command that needs a desktop but not the builder's own can run on a desktop the gate creates for it, which no one sees and nothing else shares: windows create and show, `PrintWindow` and Direct3D give real pixels, and `SendInput` is refused.

A hidden run never takes the desktop lock or touches the console desktop, so it neither waits for a locked run nor shows a window on the builder. Use it by default for a desktop route, and keep `patterns` or `session` only for a command that proves it cannot run there (one that needs `SendInput`, for example).

Declare it under a third key, `hidden = ["dotnet test *Ui*"]`, beside `patterns` and `session` (any may be absent, at most 32 each, no pattern in two, letter case aside); the first declared pattern a command matches decides, `hidden` first, then `patterns`, then `session`.

The PC consents with an exact entry in `desktop.<repo>` or `desktopSession.<repo>`; there is no key of its own. Without it the command runs here with `gate: not routed: desktop hidden pattern '<pattern>' not allowed in remote.json desktop or desktopSession`.

To prove a command there before moving it, run `gate run --desktop hidden -- <command>`: a command matching a declared and consented pattern of any mode runs as a hidden run, this run only, and `--builder <host>` may pin it. The log's hand-off line (`gate: the desktop helper pid=<h> started the command pid=<c> on the hidden desktop gate-<32 hex>`) shows it ran there; a locked or session run says `on the visible desktop`.

It is refused, exit 2, when the command matches no pattern of `.gate/desktop.toml` (`gate: --desktop hidden needs a command matching a pattern of .gate/desktop.toml`), when the pattern is not allowed (`gate: --desktop hidden needs the desktop pattern '<pattern>' allowed in remote.json`), under a parent gate (`gate: --desktop hidden cannot route under a parent gate`), or with `--local`, a `critical` or `none` slot, or any other reason that keeps a routed command here (`gate: --desktop hidden cannot route this run: <reason>`, or `nothing routes it` where the gate gives no reason);

the run never goes on without the desktop, so a builder that no longer proves it is skipped.

A command matching a `hidden` pattern that `remote.json` does not allow, run without `--desktop hidden`, is no desktop run: it runs here with the `not allowed` line above, never the `a desktop run never runs here` refusal of a consented desktop run.

Once the proof run passes, move the pattern from `patterns` or `session` to `hidden` in `.gate/desktop.toml`; the `remote.json` entry stays as it is. A repo that has not moved keeps its locked runs unchanged.

Commit a `hidden` key only after the reinstall that carries this change, on the PC and (through `gate push`) on the builders: an older gate.exe reads the key as a `.gate/desktop.toml` problem (`hidden is not a key (patterns, session)`, or the like) and keeps all of the repo's runs here.

## A repo with a command that stays on this PC

A repo with a command that needs this PC (its WSL state, a service or device only this PC has) declares its `-like` patterns in a tracked `.gate/local.toml`, `patterns = ["dotnet test *Lan*"]`, at most 32, matched against the same rewritten words as the repo's `repos` patterns. No consent in `remote.json` is needed, since the file only keeps commands here.

A command it matches runs here with `gate: not routed: .gate/local.toml keeps it here (<pattern>)` and is journalled `notRouted: declared local`; the check runs right after the repo's patterns match, before the siblings and desktop checks.

A `.gate/local.toml` the gate cannot use keeps every routed command of the repo here with `gate: not routed: .gate/local.toml: <problem>`, so fix it before relying on routing. A repo that cannot commit the file keeps it in its declarations folder ([design.md](design.md#remotejson)). `gate check` does not report it.

A command that matches none of the repo's `repos` patterns stays here whatever the file says, so it needs no entry.

## A repo with a job that reads data outside the worktree

A word naming a path outside the worktree keeps a run on this PC, and no pattern overrides it. A long job over a data folder outside the repo (a large private folder on another drive) routes through the `inputs` route instead, with no gate change:

- A PC-side job command mirrors the folder into an ignored folder of the worktree, `.tmp/blender-data/` (`robocopy /MIR`, a real copy that keeps modified times, since `inputs` skips links and its hash cache reads them), runs the repo's wrapper through the gate, then copies the results back.
- The tracked wrapper reads the mirror by its relative path and writes results to `.tmp/blender-out/`. A script it runs must be tracked too: a gitignored script that no `inputs` glob covers does not reach the builder; a failing routed run names such a script in a `gate: hint:` line, and the fix is an `inputs` glob.
- `remote.json`: `repos` names the wrapper alone, `inputs` gains the mirror's glob, and `outputs` and `failureOutputs` gain the results' glob. A `.tmp` glob must name the folder `.tmp` exactly.

Naming the wrapper rather than the tool keeps ad-hoc runs of the tool on this PC.

The first run sends the whole folder and later runs only what changed. A put past 4 GiB falls back to this PC. Each worktree holding the mirror sends and keeps its own copy, and every routed run from it hashes the mirror, so one worktree should hold it. Every routed command runs as the same builder user, so a copy on a builder is readable by any repo's routed run.

## A repo with Windows-only projects its commands do not name

A routed `dotnet test` of a Windows-only project already stays off a macOS builder, but a script such as `pwsh tools/test-all.ps1` hides which projects it tests.

A repo that wants such a script on the Mac only while no Windows-only project is affected declares the paths of those projects in a tracked `.gate/windows.toml`, `paths = ["tools/Windows.Hook/*", "tests/Windows.Hook.Tests/*"]`, at most 32 `-like` patterns matched against whole root-relative `/` paths, letter case ignored and `*` crossing `/`.

A root entry matches only the worktree's own paths, and a sibling's paths match only the entries starting `../<sibling>/`.

A project of a declared sibling (present or not) is written `../<sibling>/<path>/*`, the sibling being one `.gate/siblings.toml` declares: all three of yaat's Windows-only test projects live in its sibling yaat-server, so yaat lists each project's tool and test folders there: `../yaat-server/tools/Yaat.CrcDriverHook/*`, `../yaat-server/tests/Yaat.CrcDriverHook.Tests/*`, and the same two for `Yaat.ClientDriver.Mcp` and `Yaat.CrcProbeHook`.

No consent in `remote.json` is needed, since the file only keeps runs off the Mac.

A macOS pick (whatever the command) of a change that touches a declared path, or whose change cannot be read, goes to the next builder that is no macOS one, else `host`, else runs here, saying `gate: kept off the macOS builder: the change touches <path> (<file>)` or `gate: kept off the macOS builder: cannot tell whether the change touches <file>'s paths: <why>`, `<file>` being `.gate/windows.toml` or the path of the declarations-folder copy that was read.

A `--builder` pin to the Mac is refused with the same words.

The change is what differs from the merge-base of `HEAD` with `refs/remotes/origin/HEAD` (else `refs/remotes/origin/main`, which also stands in for an `origin/HEAD` that names no commit): committed, staged or unstaged changes and untracked files git does not ignore.

A clone without either ref, a detached `HEAD`, a shallow clone, a merge-base git cannot find, a git call that fails or does not answer in time (10 s in all) or a changed path that is not valid UTF-8 cannot be told, so the run goes to Windows.

A declared sibling with nothing at its name beside the root is absent: the run routes without it, an entry naming it reads as untouched, and no git runs in it.

A present sibling that is a link or junction, is a folder with no `.git`, is not allowed in `remote.json` or is the worktree's own folder is checked before any git runs, and steers nothing: the run stays here with the sibling's own reason, `gate: not routed: sibling <name> is a link or a junction`, and a `--builder` pin to the Mac is refused with `gate: --builder <builder> cannot route this run: sibling <name> is a link or a junction`.

A `.gate/windows.toml` with a problem (an entry naming a sibling but no path in it, for one) also sends the run to Windows, and the log names the problem. The main repo's change is read only when some entry is not a `../<sibling>/` one, and git runs without taking `.git/index.lock`. A repo with no file, or with `paths = []`, asks no git. The file may sit in the declarations folder; `gate check` does not report it ([design.md](design.md#routing), Windows paths).

## A repo whose builds read its own variables

A routed run sends none of the caller's environment but the `--env` pairs, and warns when the caller holds a variable that run drops. The warning knows the build variables (`NuGetAudit`, `RUSTFLAGS` and the `NUGET_`, `MSBUILD`, `CARGO_BUILD_` and `CARGO_PROFILE_` families); a repo whose commands read others declares their name prefixes in a tracked `.gate/env.toml`, `prefixes = ["COLLIE_"]`, at most 32, each ASCII letters, digits and `_`, none starting `GATE_`.

The line `gate: a routed run does not send the caller's <names>; --env NAME sends it` then names the caller's variables starting one too, so a caller adds `--env NAME` where the command needs it: the gate never sends a declared variable by itself, since it may hold a secret.

A file the gate cannot use says `gate: .gate/env.toml: <problem>; only the build variables are watched` and the run still routes. The file may also sit in the declarations folder; `gate check` does not report it ([design.md](design.md#command-environment)).

## Keeping a killed run's finished work on the builder

When a command is ended mid-run (killed at its ceiling, or its client lost), the Windows builder's next run clears every file that command wrote in the work folder. A repo whose builds keep a slow cache there names its folders in a tracked `.gate/spare.toml`, `folders = ["build/cache"]`, at most 32 entries, each a `/`-separated folder below the worktree root written as a `carriedFolders` entry is (`*` and `?` within a name, matched ignoring letter case, no `.`, `..` or `**` name). An entry spares everything below every folder it matches; on a pair run the list is the main repo's.

The list spares files only in that clear. A spared folder must be gitignored, or the checkout's `git clean -fdq` deletes it on every run; the clear after a layout change, `checkout --force` of tracked files and a prune ignore the list. The macOS builder's clear deletes no files, so it needs none. It needs no `remote.json` consent.

A file the gate cannot use says `gate: .gate/spare.toml: <problem>; the box's clear spares no folder` and the run still routes. The file may also sit in the declarations folder ([design.md](design.md#command-environment)).

## A repo whose git hooks run heavy checks

A heavy gate started inside a git hook stays local silently unless `remote.json` names its repo in `hooks`, an array of repo names matched in any letter case: `"hooks": ["<repo>"]`. A value that is not an array of non-empty strings consents to no repo, and the rest of the file is still read ([design.md, remote.json](design.md#remotejson)).

The key is for check-only hook commands (a build, clippy, a format check). A routed fixer's patch comes back to the worktree under prek's stash, so a fixing hook stays local. A commit stage, which runs the whole `prek run --hook-stage pre-commit` on a builder, is the exception: its fixes are applied to the index and the worktree by the stage's own rule (design.md, Sync-back).

A second array, `commits`, names the repos whose commit's pre-commit hook stage may route as one run, read like `hooks` (a bad value consents to no repo and `gate check` says it; [design.md, remote.json](design.md#remotejson)).

The hook entry calls the repo's launcher, `pwsh -NoProfile -File tools/gate.ps1 run ... -- <heavy command>`, never a bare `gate run`, as every other call does (scripts, docs and agents' own commands alike): a hook runs on every machine that commits, and on a clone without gate.exe the launcher runs the command directly. The repo's patterns must match that command.

The `git commit` that runs the hook is gated on `--slot heavy` when its hooks build or run analyzers, else on `--slot light`: the commit holds that slot for all its hooks, and a `--slot none` commit runs on a light slot and says to pass one of the two.

A heavy commit covers its hook's gate, which still routes and takes its own remote slot; a light commit does not cover a heavy hook gate, which takes a heavy slot of its own (and routes). The commit's end-of-run line says whether the hooks fit the slot. The `git commit` itself stays local, after its pre-commit stage when the repo is in `commits` and the commit is eligible.

A repo opts in with `"commits": ["<repo>"]` in `remote.json`. An eligible commit (a plain `git commit` on a heavy or light slot with no covering parent, none of the blocking variables set, a prek shim as its pre-commit hook) runs `prek run --hook-stage pre-commit` on a Windows builder under the commit's own slot, then stages the hooks' fixes and commits. The stage refuses the commit, writing nothing, when `HEAD` or the index changed while the hooks ran (an unrelated `git add` included) or when the fixes clash with unstaged edits; the fixes are then kept at `refs/gate-remote/<key>-back`. A stage that cannot start leaves the commit exactly as without it. Why a commit stays local is in the run's log: `gate: the commit's hooks run here: <reason>`. After a passed stage the commit's pre-commit hook does not run again here while `HEAD` and the index tree are what the builder checked; any change, or a hooks folder gate cannot forward, runs the local hooks, and a commit that differs from the checked one ends the run with 1, the commit kept ([design.md, Commit run](design.md#critical-drain-and-nesting)).

A hook that needs restored output declares its preparation in a tracked `.gate/stage.toml`, which has the schema of `.gate/warmup.toml` (`commands`, `timeout`; the default is 600 s, not 1800). The builder runs its commands in the stage's checkout after the tool and package restores and before the hooks, on commit stages only ([design.md, The box side](design.md#the-box-side), stage preparation).

A C# repo's format hook uses `gate csharp format` instead, which restores the projects it formats itself ([design.md, `gate csharp format`](design.md#gate-csharp-format)), so it needs no `.gate/stage.toml` for it. Its hook entry is `pwsh -NoProfile -File tools/gate.ps1 csharp format` with `language = "system"`, `types = ["c#"]`, `require_serial = true` and `stages = ["pre-commit"]`.

yaat's `dotnet format --no-restore` hook is the case: a cold work folder has no `obj`, and the formatter's fixes come out wrong without it, so yaat's file is `commands = [["dotnet", "restore", "yaat.slnx"]]`. A command that fails, runs past its budget or changes a tracked or untracked file refuses the stage (`tools`), so it moves to the next builder or the hooks run on the PC.

Only hooks that run with `GIT_INDEX_FILE` set (pre-commit and commit-msg) need the `hooks` consent: a pre-push hook's gate routes by the repo's patterns like any other run. Every gated repo carries a committed copy of this repo's `tools/gate.ps1`; claude-config's `tools/sync-launchers.ps1` gives a first copy to each repo it lists that has none and keeps every copy current, and `~/.claude/tools/gate.ps1` is the same wrapper for a caller outside any repo.

## A repo whose short runs a cold folder would push past their ceiling

A repo whose short routed runs (a filtered test of a minute) would meet a cold builder work folder declares warm-up builds in a tracked `.gate/warmup.toml`, and `remote.json` consents to it under `warmup` ([design.md, gate warm](design.md#gate-warm)).

Routed runs go to the builder from a new worktree's first run, warm or not: the first run in a new work folder is cold, with its ceiling raised, and says to prewarm the next new worktree with `gate warm`. Run `gate warm` in a new worktree before its first routed run to find the folder warm.

## A repo that fans out many routed runs at once

A worktree has one work folder on each builder, and its routed runs take turns on it: a second run says `gate: waiting for this worktree's work folder on <host>, which another of its runs holds` until the first ends ([design.md](design.md#routing)). So a script that starts many short routed runs in parallel from one worktree runs them one at a time, each paying its own push and fetch.

To fan out, loop over the runs inside one gate call (a tracked wrapper script the repo routes), which keeps one warm folder and pays one push. A fan-out of long, independent runs can use one worktree per lane instead, each lane a cold folder on its first run. The gate gives one worktree no second work folder, since each would be a cold build and its own disk on the builder.

## A repo that builds with Unreal Engine

The engine is not in the worktree and not on the builder's `PATH`: the builder holds its engines side by side under `GATE_UNREAL_ROOT`, one subfolder per version named `x.y.z`, each with `Engine\Build\Build.version` ([design.md, Tool probes](design.md#the-box-side)). The engine install is not part of `tools/builder/install-toolchains.ps1`: a session installs an engine by hand over ssh. `gate check --builder` shows the versions it lists.

- **Installing an engine on a Windows builder.** An engine is about 30 GB in about 260,000 files, and a plain `tar` or `scp -r` stream of that many small files crawls (4.5 MB/s against the link's 43 MB/s, measured for 5.8.3).
  - Copy the PC's install with claude-config's `tools/builder-copy/builder-copy.ps1` (`-Source <the PC's UE_5.x folder> -Dest C:\unreal\<x.y.z>`), or pack it with 7-Zip on the PC (`-mx=1`, 30.3 GB to 11.8 GB), send the one archive, and unpack it on the builder.
  - A remote unpack run under the gate needs output on its console (7-Zip's `-bsp1`): an ssh command shows no CPU on the PC, so a silent one is killed as `STALLED`, while the remote process runs on.
  - Before the copy, exclude the root from Defender (`Add-MpPreference -ExclusionPath C:\unreal`, as `C:\godot` is). Once the copy holds, set `GATE_UNREAL_ROOT` machine-wide to the root, and give the root the write-deny for the builder user that the installer's `Add-BuilderWriteDeny` puts on the Godot root.
  - Check it with `gate check --builder` from a repo whose `.gate/toolchain.toml` pins the version: `unreal <x.y.z> ok (needs <pin> exactly)`.

- **The pin.** The repo pins the engine in `.gate/toolchain.toml`, `unreal = "5.8"`, an exact pin like godot's: `"5.8"` takes any 5.8.x and `""` any version. A builder without a matching engine keeps the run here with the mismatch line. The gate picks the highest matching engine and the box sets `UE_ROOT` to its folder for every command of the run.

  A repo whose build reads another variable for the engine (ue-mcp's `UE_MCP_TEST_ENGINE_ROOT`) names it in the manifest's `[env]` table, `UE_MCP_TEST_ENGINE_ROOT = "unreal"`, and the box sets it to the same folder; a PC engine path passed with `--env` is not rewritten for the builder, and `UE_ROOT` itself is refused there. `gate check` shows an `[env]` entry the gate cannot use.
- **The routed command is a tracked wrapper script.** It builds absolute `-project=` and `-archivedirectory=` paths itself, because `RunUAT.bat` `pushd`es into the engine and the router sends worktree paths as relative ones. It reads the engine from `UE_ROOT`, with its own fallback for the PC, where nothing sets it.
- **The .NET Framework SDK.** A manifest naming `unreal` at any version also needs a `netfx-sdk` of 4.6.0 or later, the SDK `UnrealBuildTool` looks for, which the gate adds itself. The installer's step `.NET Framework 4.8 Developer Pack` provides it, and a builder without it keeps the run here with the mismatch line. `gate check` here judges it too, unless `GATE_UNREAL_ROOT` is unset on this PC, where it says it is not checked.
- **The archive** goes to a gitignored folder outside `.tmp/`, named in the repo's `outputs`, so it is not left in `.tmp/`, whose files come back only when the run wrote them and a glob names `.tmp`.
- **What stays local:** commandlets that write into ignored content, a packaged-game run that needs a GPU when the builder has none, and the editor.
- **Git LFS.** A repo with LFS-tracked files routes with them: the PC sends the objects the snapshot's pointers name to the builder's per-repo LFS store, and the box checks them out with no download.

  A Windows and a macOS builder both take LFS content.

  The builder needs git-lfs 3.1.3 or later whenever a tracked `.gitattributes` names the LFS filter (a builder without it keeps the run here, `parity`), and the PC must hold the objects: run `git lfs fetch` first, else the run falls back `lfs` with a log line naming the missing object. See [design.md, Routing](design.md#routing) (Git LFS).

## A repo that runs headless Blender

Blender is not on the builder's `PATH`: each builder holds its installs side by side under `GATE_BLENDER_ROOT` ([design.md, Tool probes](design.md#the-box-side)). The Windows builder's installer step `Blender` copies the PC's `Blender 5.2` folder under `C:\Program Files\Blender Foundation` and sets the variable; the Mac installer's `Blender 5.2.2 LTS` step puts `5.2.2/Blender.app` under `<prefix>/blender`, which its login block exports. `gate check --builder` shows the versions each lists.

- **The pin.** The repo pins `blender = "5.2"` in `.gate/toolchain.toml`, an exact pin like godot's (`"5.2"` takes any 5.2.x). A builder with no matching install is skipped for `parity`, so the job routes to either builder that has one and runs here otherwise.
- **The path.** Name an `[env]` variable the job's wrapper reads, `BLENDER_EXE = "blender"`, in place of a Windows path: the box sets it to the chosen install's executable (`blender.exe` on Windows, `Blender.app/Contents/MacOS/Blender` on the Mac), so one tracked wrapper runs on both. A PC path passed with `--env` is not rewritten for the builder.
- **The config.** Copy the PC's Blender config to each builder as the Builder setup steps say. A job run with `--factory-startup` reads none of it.
- **The GPU.** A Mac job that wants the GPU lists `METAL` among its Cycles devices.
- **Mac first.** List the job's pattern on both builders with `gate remote add repos <repo> "<pattern>" --builder <host>` (and the Windows builder's own `repos`), then rank the Mac first with `gate remote add first <repo> "<pattern>" --builder <host>`, `<host>` being the Mac's alias. A free Mac then takes the job ahead of a warmer or faster Windows builder, and Windows takes it when the Mac is down, full or lacks the pinned Blender. A Mac with no free slot ranks by score like any builder.

## Build files that read back as zeros

When a caller reports build files that are the right size but all zero bytes, check for a crash of the PC before reading any gate code. Three things point to one:
- System events 41, 6008 or 1001 near the time.
- A run of NUL bytes in the PC's journal (`events.jsonl`, [design.md](design.md#journal)).
- A gate with a `take` line and no `release`.

A crash loses data that was not yet written to every drive at once. NTFS can keep a file's new size while its contents never reach the disk. The gate never leaves a file like that: the outputs install unpacks into a stage it removes on any error, and it replaces a worktree file only by renaming a complete, verified copy over it.

## Taking a builder out for maintenance

A reboot, an install that needs one, or any other work that would break a run on the builder starts by draining it with `gate drain`, never by waiting for it to go idle while it stays in `remote.json`: new runs keep arriving, so that wait has no end.

1. Run `gate drain`. It raises a barrier on the builder `remote.json`'s `host` names (`gate drain --builder <host>` drains another builder the file lists) and waits, naming on stderr the runs still going there. While the barrier stands, routed runs from every repo wait on their PC for the lift and then start on the builder in arrival order; none falls back to a local run because of the drain, and the wait does not count toward a run's `--timeout`. `remote.json` stays as it is.
2. Wait for `gate: drain: the builder is drained; gate drain --lift lifts it`. There is no cutoff, since each routed run ends at its own ceiling. A run still live past the longest routed ceiling is looked into, never cut off by the reboot. Ctrl+C leaves the barrier standing; `gate drain` again resumes the wait, and `gate drain --status` says how the drain stands.
3. Do the work. Windows updates install through the builder's nightly drained task (`\build-box\gate-windows-update`, see Builder setup), and a reboot it leaves pending waits for the owner. A reboot needs the owner's yes first, asked once the builder is drained, so the owner can watch the machine through it. After a reboot, confirm that the console session logged back on (autologon), which the desktop route needs.
4. Run `gate drain --lift` (with the same `--builder`), and check the builder with `gate check --builder <host>`. If anything blocks the work partway, lift first, so the builder never stays drained. The runs that waited start in arrival order.

The barrier lapses on its own 30 minutes after it was last raised. `gate drain` raises it again every 5 minutes while it waits, but once it has said drained and exited nothing does, so work that outlasts 30 minutes runs `gate drain` again before the lapse. A lapse is said to the drain, to every waiting run and to the next `gate drain`.
