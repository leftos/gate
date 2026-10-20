# macOS (Apple-silicon) builder: toolchains and server setup, for GATE-591

Researched: 2026-10-20. Each fact links its source; anything marked **UNCONFIRMED** was not traced to a primary source in this pass and should be checked on the machine itself.

## 1. The machine: Mac mini with M5 Pro

| Item | Fact | Source |
|------|------|--------|
| Announced / shipped | Announced 2026-08-25, available 2026-09-22 | [Apple Newsroom](https://www.apple.com/newsroom/2026/08/apple-unveils-a-more-powerful-mac-mini-featuring-the-all-new-m6-and-m5-pro/) |
| Base CPU | 15-core: 5 "super" cores + 10 "performance" cores; no efficiency cores listed | [Apple tech specs](https://support.apple.com/en-us/128108), [apple.com/mac-mini/specs](https://www.apple.com/mac-mini/specs/) |
| Upgrade CPU | 18-core CPU / 20-core GPU. Apple does not publish the split; third-party lists 6 super + 12 performance (**UNCONFIRMED**, check with `sysctl hw.perflevel0.physicalcpu hw.perflevel1.physicalcpu`) | Apple specs above; [cpu-monkey](https://www.cpu-monkey.com/en/compare_cpu-amd_ryzen_9_9950x-vs-apple_m5_pro_18_cpu_20_gpu) |
| Memory | 24 GB base; 48 GB or 64 GB configure-to-order; 307 GB/s bandwidth; not upgradeable | Apple tech specs |
| Storage | 512 GB base; 1, 2, 4 or 8 TB | Apple tech specs |
| Network | 2.5 GbE standard, 10 GbE option | Apple Newsroom |
| macOS shipped | macOS 27 "Golden Gate" | Apple Newsroom; [EveryMac](https://everymac.com/systems/apple/mac_mini/specs/mac-mini-m5-pro-15-core-cpu-16-core-gpu-2026-specs.html) lists "macOS 27.0" pre-installed |
| SMT | None: 1 thread per core, so 15 or 18 build jobs, not 2x | cpu-monkey (third-party) |

### Multi-core performance versus desktop x86

No compile-specific benchmark (Rust, MSBuild) from a credible reviewer was found; only synthetic aggregates, which disagree with each other. Treat as rough.

| Benchmark (M5 Pro 18-core vs Ryzen 9 9950X) | M5 Pro | 9950X | Source |
|---|---|---|---|
| Geekbench 6 multi-core | ~28,400-29,200 | ~20,500-22,400 | [nanoreview](https://nanoreview.net/en/cpu/apple-m5-pro-18-core), cpu-monkey |
| Geekbench 6 "Clang compilation" subtest | 262.5 Klines/s | not given | nanoreview |
| Cinebench 2026 multi | 9,440 | 9,725 | cpu-monkey |
| PassMark CPU Mark | 57,203 | 65,707 | [technical.city](https://technical.city/en/cpu/Apple-M5-Pro-18-Core-vs-Ryzen-9-9950X) |

Reading: Geekbench (which favours burst, single-threaded and memory bandwidth) puts the M5 Pro ahead; sustained all-core tests (Cinebench, PassMark) put it 3-15% behind a 16-core Zen 5 desktop. Expect a heavy parallel `cargo build` to land near, or slightly under, a 9950X. Measure with the repo's own builds on day one. The Mac mini's sustained thermals under a long all-core load are **UNCONFIRMED**.

## 2. Toolchains on macOS arm64

| Tool | Status | Source |
|------|--------|--------|
| Rust | Stable 1.99.1 (2026-10-15); 1.99.1 fixes `rust-lld` failing to load `libLLVM` on macOS when rustc runs without the rustup proxy. `aarch64-apple-darwin` is Tier 1 with host tools (**tier not re-read this pass**; check [platform-support](https://doc.rust-lang.org/rustc/platform-support.html)) | [Rust 1.99.1](https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/) |
| cargo-nextest | Ships macOS arm64 binaries (**version UNCONFIRMED**) | [nexte.st](https://nexte.st/docs/installation/pre-built-binaries/) |
| .NET | SDK 10.0.402 (LTS, runtime 10.0.13, 2026-10-13) has an osx-arm64 installer; .NET 11 is at RC2 (go-live). .NET 8 and 9 end support 2026-11-10 | [dotnet download 10.0](https://dotnet.microsoft.com/en-us/download/dotnet/10.0), [all versions](https://dotnet.microsoft.com/en-us/download/dotnet) |
| PowerShell 7 | 7.6.6 LTS osx-arm64 pkg (signed and notarized since May 2026); supported on macOS 27 arm64. macOS 27 runs only on Apple silicon | [Install on macOS](https://learn.microsoft.com/en-us/powershell/scripting/install/install-powershell-on-macos?view=powershell-7.6) |
| Godot | 4.7.2 stable (2026-08-18), macOS universal and .NET universal builds; 4.8 at beta1 | [Godot macOS download](https://godotengine.org/download/macos/), [archive](https://godotengine.org/download/archive/) |
| Playwright | Supports macOS 14+ for the runner, but WebKit dropped macOS 13 and then macOS 14; browsers live in `~/Library/Caches/ms-playwright`; Node 22, 24 or 26 | [Playwright intro](https://playwright.dev/docs/intro), [browsers](https://playwright.dev/docs/browsers), [release notes](https://playwright.dev/docs/release-notes) |
| Node / pnpm / bun | All publish darwin-arm64 builds (**versions UNCONFIRMED this pass**) | [nodejs.org](https://nodejs.org/en/download), [pnpm](https://pnpm.io/installation), [bun](https://bun.sh/docs/installation) |
| uv / Python | uv ships `aarch64-apple-darwin` and installs arm64 CPython (**version UNCONFIRMED**) | [uv install](https://docs.astral.sh/uv/getting-started/installation/) |
| Git LFS | darwin-arm64 release and Homebrew formula (**version UNCONFIRMED**) | [git-lfs releases](https://github.com/git-lfs/git-lfs/releases) |
| Unreal Engine | Editor is a universal binary since 5.2; a source build must ask for `x64+arm64` (command-line builds defaulted to x64). Needs Xcode. Current-version macOS/Xcode minimums **not read this pass** (the cited page is 5.2, 2023). Nanite, hardware ray tracing and groom strands were limited on macOS in 5.2 | [UE 5.2 Apple silicon blog](https://www.unrealengine.com/tech-blog/unreal-engine-5-2-brings-native-support-for-apple-silicon-and-other-developments-for-macos), [UE 5.2 release notes](https://dev.epicgames.com/documentation/unreal-engine/unreal-engine-5.2-release-notes) |

### .NET: what a Mac cannot do

- A project with a `-windows` TFM (`net10.0-windows`, WinForms, WPF) fails restore/build with NETSDK1100 unless `EnableWindowsTargeting=true` is set (project, `Directory.Build.props`, or `/p:EnableWindowsTargeting=true`). With it set, the Windows targeting packs are downloaded and the project **compiles** on macOS. [NETSDK1100](https://learn.microsoft.com/en-us/dotnet/core/tools/sdk-errors/netsdk1100)
- Microsoft's note: output built off Windows is not the same: the exe is not marked a Windows (GUI) application and gets no embedded icon; release builds should be made on Windows. ([msbuild-props EnableWindowsTargeting](https://learn.microsoft.com/en-us/dotnet/core/project-sdk/msbuild-props#enablewindowstargeting), quoted in the [Stack Overflow answer](https://stackoverflow.com/questions/74026084/what-exactly-does-enablewindowstargeting-do))
- `dotnet test` on a `-windows` TFM test project builds but cannot run: the Microsoft.WindowsDesktop runtime does not exist for macOS (**UNCONFIRMED by a primary doc this pass**; inferred from the desktop runtime being Windows-only on the download page). Tests that touch WinForms/WPF/WinUI, the registry, or P/Invoke into Windows DLLs need the Windows builder.
- WinUI 3 / Windows App SDK projects: build on macOS **UNCONFIRMED** (they depend on Windows-only MSBuild tasks such as the XAML compiler; assume no).
- Precedent: ReactiveUI/Splat builds `net8/9/10-windows` TFMs on Linux and macOS with `EnableWindowsTargeting` ([splat PR 1581](https://github.com/reactiveui/splat/pull/1581)).

### Godot headless

- `--headless` is required where there is no GPU access and stops a window opening otherwise; `--import`, `--export-release`, `--export-debug`, `--export-pack` work from the CLI, with export templates installed. On macOS the binary is `Godot.app/Contents/MacOS/Godot`. [Command line tutorial](https://docs.godotengine.org/en/stable/tutorials/editor/command%5Fline%5Ftutorial.html)
- Export of macOS `.app` bundles from macOS keeps the executable bit (Windows exports lose it), and DMG packing only works on macOS. [Exporting for macOS](https://docs.godotengine.org/en/stable/tutorials/export/exporting_for_macos.html)
- gdUnit4 ships `addons/gdUnit4/runtest.sh` for macOS/Linux, reading `GODOT_BIN`; for a .NET Godot it runs `dotnet build` first. Note the stock script runs the test pass **without** `--headless` (only the log-copy step has it), so add `--headless` for an ssh session. [gdUnit4 CLI docs](https://godot-gdunit-labs.github.io/gdUnit4/latest/advanced_testing/cmd/)
- GUT has its own `gut_cmdln.gd` CLI (**not re-read this pass**).
- Godot .NET on arm64 needs a separately installed .NET SDK (Godot macOS download page).

## 3. Cross-compiling Windows targets from macOS

| Path | What works | Source |
|------|-----------|--------|
| Rust `x86_64-pc-windows-msvc` via cargo-xwin 0.23.1 | `brew install llvm`, `rustup target add x86_64-pc-windows-msvc`, `cargo xwin build --target ...`. Downloads the MSVC CRT and Windows SDK (cacheable with `cargo xwin cache xwin`); CMake deps need `ninja`. `cargo xwin test` works only with Wine installed | [cargo-xwin README](https://github.com/rust-cross/cargo-xwin), [crates.io](https://crates.io/crates/cargo-xwin) |
| Rust `x86_64-pc-windows-gnu` | Needs a mingw-w64 toolchain (Homebrew `mingw-w64`); Tier 1 target (**UNCONFIRMED this pass**) | — |
| Running Windows test binaries | Not natively. Wine on macOS arm64 runs x86_64 Windows binaries only through Rosetta 2, and is not a faithful Windows (no real registry semantics, services, Job Objects, ConPTY) (**UNCONFIRMED**). Accepting Microsoft's CRT/SDK licence terms is the user's job when using xwin | — |
| .NET Windows-targeted assemblies | Build with `EnableWindowsTargeting` (section 2); pure `net10.0` assemblies with `-r win-x64` publish fine from any OS since the runtime pack is downloaded (**UNCONFIRMED this pass**). Running them needs Windows | NETSDK1100 doc |

Verdict: the Mac can produce Windows artifacts for a compile check, but every Windows-target test run belongs on the Windows builder.

## 4. Running a build server over ssh

- **sshd**: System Settings > General > Sharing > Remote Login, optionally "Allow full disk access for remote users"; or `sudo systemsetup -setremotelogin on`. [Apple: Allow a remote computer](https://support.apple.com/guide/mac-help/allow-a-remote-computer-to-access-your-mac-mchlp1066/mac), [systemsetup](https://support.apple.com/guide/remote-desktop/about-systemsetup-apd95406b8d/mac).

  Without full disk access, ssh sessions get TCC "Operation not permitted" on `~/Desktop`, `~/Documents`, `~/Downloads` and removable volumes (**UNCONFIRMED by primary doc**); keep repos elsewhere, e.g. `~/dev`.
- **Stay awake**: `sudo pmset -a sleep 0 disksleep 0`, `sudo pmset -a autorestart 1` (power-loss restart), `womp 1` (wake on network); per-job `caffeinate -i <cmd>`. FileVault blocks automatic login, so after a reboot nothing runs as a GUI user until someone logs in. [macrun.dev guide](https://macrun.dev/mac-mini-server) (secondary); `man pmset`, `man caffeinate` on the machine.
- **No GUI session** (**UNCONFIRMED, check on the machine**): the login Keychain is locked in an ssh session (`security unlock-keychain` or a dedicated keychain for codesign/notarytool); WindowServer-dependent work (Playwright headed, Godot without `--headless`, UE editor without `-nullrhi`/`-unattended`) fails or hangs. Metal itself is usually reachable from a non-GUI process, but this is not documented by Apple. An HDMI dummy plug is a common workaround for apps that want a display.
- **launchd** for periodic cleanup: a `LaunchDaemon` (root) or `LaunchAgent` (user, runs only while that user is logged in) plist with `StartCalendarInterval`; plists can set `SoftResourceLimits`/`HardResourceLimits` `NumberOfFiles`. [`man launchd.plist`]; example in the [Quilibrium doc](https://docs.quilibrium.com/docs/run-node/node-provisioning/manual-install/macos/).
- **Open files**: default soft limit is 256 (`launchctl limit maxfiles`); hard limit for processes is effectively `OPEN_MAX` = 10240 unless `kern.maxfilesperproc` is raised. Big Rust/Node builds hit 256.

  Fix: `ulimit -n 65536` in the session's shell init plus `sudo sysctl kern.maxfiles=524288 kern.maxfilesperproc=524288` (persist in `/etc/sysctl.conf`) and a `limit.maxfiles` LaunchDaemon. [Stack Overflow (Ken Thomases, quoting setrlimit man page)](https://stackoverflow.com/questions/42738899/setting-maxfile-on-os-x-macos-programmatically), [Apple SE 453050](https://apple.stackexchange.com/questions/453050/how-to-increase-global-maxfiles-ulimit-on-osx-13-1-ventura)
- **Process groups**: POSIX `setsid()`/`setpgid()` and `killpg()` behave as on Linux; there is no `setsid` command-line tool in base macOS (**UNCONFIRMED**; util-linux is not shipped), so start the job group from the gate binary itself (`std::os::unix::process::CommandExt::process_group(0)` in Rust) and kill with `killpg`.

  There is no cgroup equivalent and no `prctl(PR_SET_PDEATHSIG)`: children survive a dead ssh session unless the group is killed. When sshd's session ends, the shell gets SIGHUP; a process that ignores it keeps running.
- **Case-insensitive APFS**: the default volume is case-insensitive, case-preserving. git sets `core.ignorecase=true` on clone; two paths differing only in case collide, and a rename that only changes case needs `git mv`. A case-sensitive APFS volume (Disk Utility > Add Volume, "APFS (Case-sensitive)") for the build tree avoids this (**UNCONFIRMED by a primary doc this pass**; see [git-config core.ignoreCase](https://git-scm.com/docs/git-config#Documentation/git-config.txt-coreignoreCase)).
- **File watching**: macOS uses FSEvents (no inotify watch limit); kqueue-based watchers consume one fd per file and hit the 256 limit above (**UNCONFIRMED**).

## 5. Why macOS is a poor builder for Windows-first repos, with mitigations

| Problem | Mitigation |
|---------|-----------|
| Windows-only test targets (WinForms/WPF/WinUI, Win32 P/Invoke, registry, services) cannot run | Route those tests to the Windows builder; tag them by TFM/OS |
| `.ps1` wrappers: run under `pwsh` but Windows-only cmdlets are missing (`Get-Service`, `Get-Acl`, `Get-ComputerInfo`, CIM, `Out-GridView`, PSRemoting); aliases `ls`, `cp`, `rm`, `cat`, `ps`, `kill`, `sleep`, `sort` resolve to native Unix tools; execution policy is ignored | Use full cmdlet names; guard with `$IsWindows`. [PowerShell differences on non-Windows](https://learn.microsoft.com/en-us/powershell/scripting/whats-new/unix-support?view=powershell-7.6) |
| `.bat`/`.cmd`, `cmd /c`, `where.exe`, `robocopy`, `icacls` | No equivalent; route to Windows |
| Paths: `\` separators, drive letters, absolute Windows paths, `%LOCALAPPDATA%` | Map to `~/dev/...`; avoid hard-coded Windows paths in commands sent to the Mac |
| Case sensitivity: a Windows repo that `#include`s or `using`-loads `Foo.h` as `foo.h` works on case-insensitive APFS but breaks on a case-sensitive volume | Pick one volume type deliberately; case-insensitive is closer to Windows |
| Line endings: repos with LF in the index (the owner's rule) are fine; `.bat`/`.cmd` CRLF is harmless. `core.autocrlf=true` copied from Windows config would rewrite files | Leave `core.autocrlf` unset on the Mac |
| Path length: macOS `PATH_MAX` 1024, filename 255 bytes, no MAX_PATH 260 problem (**UNCONFIRMED**) | None needed |
| Open-file limit 256 | See section 4 |
| Executable bit: scripts committed from Windows lack `+x` | `git update-index --chmod=+x`, or invoke via `bash script.sh` / `pwsh -File` |
| One thread per core (no SMT) | Size job counts to physical cores (15 or 18) |
