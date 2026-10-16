<!-- Design for GATE-231 (desktop runs on the builder): the invariant pass before its first brief, with the GATE-279 and GATE-284 spike results settled into GATE-231's description. Deleted by hygiene once nothing open links it. -->

# GATE-231 invariant pass: a routed command on the builder's desktop (shape 3)

## Summary

Shape 3 holds up, with three changes before the first brief:

1. **Say the start marker later.** It should come after the suspended child has been adopted, not before the helper is started. Then every failure of the desktop machinery falls back to the PC, and the "nothing the command prints can forge a tag" rule still holds.
2. **Fix the Task Scheduler defaults.** Out of the box, a task runs below normal priority with low I/O and memory priority, which the command would inherit. It also drops a run while an earlier helper is still alive.
3. **Measure the actual target first.** Window Z-order and foreground behaviour, started through a helper with a hidden console, decide where the helper should live. The spike only measured D3D device creation.

On your position: I agree with the shape and with making the desktop job kill-on-close. That is a change from today, though: the run's job is not kill-on-close now. I rank "a same-user process poses as the helper" lower than you do, because same-user Medium processes are already inside the box's trust boundary. The cross-identity risk sits on the helper's side, not the box's.

Sources: the worktree at b903f50, `docs/README.md`, `docs/ARCHITECTURE.md`, `docs/design.md` (The box side, Kernel object names, Routing), the map, the spike report, GATE-231 and GATE-279 (Linear), GATE-271, and Microsoft Learn (cited inline). Nothing was run on a builder. Every claim about how Windows behaves is either measured by the spike, quoted from the docs, or marked **unverified**.

---

## 1. Actors

| Actor | Identity, session, integrity | What it can do | When |
|---|---|---|---|
| **The box run** (`gate box run`) | builder user, session 0, High, elevated | Create named objects (session 0's `Local\` is the global namespace). Start the task through COM `RunEx`. Open Medium processes of the same user with `PROCESS_DUP_HANDLE`, `PROCESS_TERMINATE` and query rights (measured). Copy handles into and out of the helper. Assign processes to its job, end the job, read its accounting. Query terminal-services (WTS) session state. | From the work-folder lock until it exits |
| **Other box runs** (other `run`s, `stop`, `preflight`, `outputs`, `inflight`) | same user, session 0, High | Connect to the control pipe: the DACL names a user, not a process, so the pid check has to refuse them. Open this run's job by name (`gate box stop` does exactly that, by design). Wait on the desktop mutex. | Any time |
| **Another desktop run** | as above | Contends on the desktop mutex while holding its own work-folder lock and its PC's remote slot. Has its own pipe name, its own task instance and its own job. Never touches this run's helper unless the code is wrong. | While this run holds the desktop |
| **The helper** | builder user, session 1, Medium, filtered token, inside the Task Scheduler's job | Connect to the pipe named in its `Arg0`. Create processes as the user in session 1. Hold the pipe write ends the box gives it. It can open nothing of the box's: High label plus an Administrators/SYSTEM DACL. | From `RunEx` until it exits, a few seconds |
| **Task Scheduler** | SYSTEM service | Starts the helper, or silently doesn't: a disabled task or `AllowDemandStart=false` still returns `S_OK` ([RunEx](https://learn.microsoft.com/windows/win32/api/taskschd/nf-taskschd-iregisteredtask-runex)), and the default `IgnoreNew` drops a run while an instance is alive ([MultipleInstancesPolicy](https://learn.microsoft.com/windows/win32/taskschd/taskschedulerschema-multipleinstancespolicy-settingstype-element)). Picks the session and user from the `RunEx` flags. Owns the job above the command's (measured nesting). Applies task priority, which defaults to 7: below-normal CPU, low I/O, low memory priority ([Priority element](https://learn.microsoft.com/windows/win32/taskschd/taskschedulerschema-priority-settingstype-element)). May end its job on an instance stop, `schtasks /end` or a service restart (**unverified**). | From `RunEx` until the command ends. Its job outlived the helper, measured once. |
| **Other Medium processes of the logged-on user** (explorer, updaters, toasts, earlier leftovers, and the session-0 restricted commands of other routed runs) | same user, Medium, either session | Connect to the control pipe (refused by pid). Open, inject into or kill the helper and the desktop command: same user, same integrity, and the restricted token's default DACL grants the user too (token.rs). Send input to the command's windows and steal the foreground. Possibly start or end the task (depends on its security descriptor, **unverified**). Replace `%LOCALAPPDATA%\gate\bin\gate.exe`, which ssh then runs elevated: a pre-existing issue, ACL **unverified**. They cannot open the box process, its job, its stop event, the desktop mutex or the pipe read ends. | Any time |
| **The logged-on user (a person)** | at the console or over RDP | Lock the screen, log off, switch user, RDP in (which moves session 1 off the console), click on or close test windows, answer UAC prompts (a secure desktop). Before the start each of these is a refusal; during the run it is a command failure. | Any time |
| **The PC client** | the PC | Sends `Desktop` only to a box whose version text matches exactly (design.md:1738). Reads the marker and refusal tags. Calls `stop` after Ctrl+C or a lost connection, by job name (client.rs:1195, :1240). Calls `boot`. | The run call and after |
| **Other identities** (services, other users) | not the builder user | Refused at `CreateFile` by the pipe DACL (measured for SYSTEM). They can create a pipe under any name, so they could squat one the helper connects to. That is the one cross-identity path, closed in I4. | Any time |

---

## 2. Invariants

### A. Trust boundary

- **I1. The boundary is "the builder user against everyone else", not "the box against the helper".** Any Medium process of the builder user can already:
  - inject into today's restricted session-0 command, whose token's default DACL is the user plus SYSTEM (`crates\gate-win32\src\token.rs`, design.md:1941);
  - probably replace the gate.exe that ssh runs elevated.

  The pid and handle checks below therefore protect against mistakes, stale helpers and mixing up runs. They do not stop the user's own processes, and the docs should say that.
- **I2. Only the builder user's SID may connect, and only locally.** The pipe is created by the box with:
  - an explicit DACL for the builder SID only (measured to refuse SYSTEM);
  - `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE` and at most one instance;
  - a fresh 128-bit random part in the name, not derived from the run's nonce;
  - access narrowed to read and write. The spike's `FA` also hands the client `WRITE_DAC`/`WRITE_OWNER`.
- **I3. The box accepts a connection only from the task instance it started.** `GetNamedPipeClientProcessId` must equal `IRunningTask.EnginePID`. The box opens that pid the moment `RunEx` returns and keeps the handle until the hand-off ends, which pins the pid. It checks:
  - created after the `RunEx` call;
  - in the target session;
  - image is the expected helper path;
  - token user is the builder SID.

  Any other client is disconnected and the box keeps listening until the deadline. Nothing a wrong client sends is read.
- **I4. The helper checks the server.**
  - The pipe's owner must be the builder SID and its DACL exactly the entries the box writes (read with `GetSecurityInfo` on the open handle); another identity cannot set either without `SeRestorePrivilege`. The box's pipe also carries a Medium mandatory label (no read or write up), so a Low-integrity process of the user cannot take its one instance.
  - `GetNamedPipeServerProcessId` must equal the box pid in `Arg0`, as a cross-check only: once the box has exited, its pid can be reused.
  - It connects with `SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`, the same as `crates\gate-win32\src\launch.rs:385` does for output files.

  Without these, another identity that squats a pipe name could impersonate the user's interactive token, and could choose what the helper starts. Your position only covers the box's side.
- **I5. Read ends, the job handle, the stop event and the desktop mutex never leave the box.** Only the two pipe write ends go out, into the helper and from there into the child.
  - Precedent: `crates\gate-win32\src\piped.rs:129` gives a command no handle to a kill-on-close job.
  - Under I14, a planted job handle would keep the job, and the command, alive after the box dies.

### B. Order and the start marker

- **I6. Today's rule is stricter than the safety argument needs.** The rule is "the marker before the command's process is created" (design.md:1411; `crates\gate\src\remote\mod.rs:94-99`). What the safety actually rests on: the marker goes to the box's stderr before the forwarders start and before the command runs a single instruction. A suspended child executes nothing, and anything waiting in the pipe is forwarded after the marker.

  So for a desktop run: say the marker after adoption (I9) and before the stop look and the resume (`crates\gate\src\boxside\run.rs:1006-1010`). The two doc sites above change to the weaker rule.
- **I7. Before the marker, every failure of the machinery is a refusal; after it, nothing is.**
  - Session check, task, `RunEx`, connect, HELLO, the hand-off, verification and assignment: tag `desktop` (busy is the exception, see D5), exit 75, and the child, if one exists, is ended.
  - A `FAILED{code}` from the helper's `CreateProcess` is the command's own start failure: the marker, then `could not start`, 127, as today.
  - It must go straight to `not_started` (run.rs:1490), never through `start_failed`'s breakaway check (run.rs:1139-1154). That check asks about the box's own job, which says nothing about a process the helper created.
- **I8. A desktop payload never runs in session 0.** The desktop branch is its own function. It cannot reach `PipedLaunch`, the own-token retry (run.rs:1036-1045) or the donor (run.rs:766). A box that cannot get the desktop refuses.
  - On the PC, the exact-version check (design.md:1738) is the only thing that keeps an older box from ignoring an unknown `Desktop` key (design.md:1752) and running the command in session 0.
  - The box refuses a `Desktop` that is not a boolean with exit 2.

### C. What the box accepts as the child

- **I9. The child is taken by handle, from the helper's handle table; the pid is only a cross-check.** The box copies the reported handle values out of the helper with `DUPLICATE_SAME_ACCESS` (measured), then closes the helper's copies itself with `DUPLICATE_CLOSE_SOURCE`, so it does not depend on the helper tidying up. It accepts the process only when all of these hold:
  - `GetProcessId(process)` equals the reported pid, and `GetProcessIdOfThread(thread)` equals it too;
  - the parent pid is the helper (`parent_pid_of`, proc-table) and the child was created after the helper;
  - the child is in the target session;
  - its image is the `application` the box sent;
  - it is still suspended exactly once (`SuspendThread` returns a count of exactly 1, then `ResumeThread` undoes it), and the final resume fails unless `ResumeThread` returns 1, so a child suspended twice is never left suspended after the marker;
  - it is not yet in the box's job.

  Then assign it, and check `IsProcessInJob(process, box_job)` is **true** for the box's job specifically. The spike's `injob=1` asked about "any job". The only proof the child was in the box's job was that kill-on-close ended it.
- **I10. On a failed check, end only what is provably the helper's.** Never end the process behind an unverified handle: it could be any process of the user. End every process whose parent is the helper and which was created after it in the target session (a proc-table snapshot), then refuse. This sweep also covers a helper that died between `CreateProcess` and its report, which would otherwise leave a suspended orphan holding the pipe write ends.

### D. The job

- **I11. The command must be the first member of an empty job, and nothing from session 0 may ever join it.**
  - The docs say a process already in a job can only join a job that is empty or in its hierarchy. They also say "All processes within a job must run within the same session as the job" ([AssignProcessToJobObject](https://learn.microsoft.com/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)).
  - The spike's success across sessions contradicts that second line. My hypothesis, **unverified**: an empty job takes the session of its first member.
  - So for a desktop run: no donor (`crates\gate-win32\src\console.rs:369-371` puts the donor in the job before the command), no keeper, nothing assigned by the box but the child. The tool restore already has its own `-tools` job.
- **I12. No `CREATE_BREAKAWAY_FROM_JOB` in the helper (measured: refused).** The command nests below the Task Scheduler's job. The docs say the most restrictive priority class across the job chain wins ([same page](https://learn.microsoft.com/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)), so register the task at Priority 4 (D2).
- **I13. Normal priority, explicitly.**
  - A process whose parent is below-normal inherits below-normal ([Scheduling Priorities](https://learn.microsoft.com/windows/win32/procthread/scheduling-priorities)), and a task's default priority 7 gives its action below-normal.
  - So the helper passes `NORMAL_PRIORITY_CLASS`, and the task is registered at Priority 4: normal CPU, normal I/O, normal memory priority.
  - Without this the box's ceiling arithmetic, which assumes every core is demanded at normal priority (design.md:1831), and the PC's comparison with local runs both drift.
  - The hand proof reads the command's priority class.
- **I14. The desktop job is created kill-on-close.**
  - Use `Job::create_kill_on_close` (`crates\gate-win32\src\job.rs:321`), not `Job::create` (run.rs:1166). The tool restore's job is the precedent (design.md:1909).
  - With one desktop, an orphan that outlives a dead box would hold the console while the next desktop run starts. Kill-on-close makes the box's death end the command.
  - The PC's `stop` after a lost connection then finds no job and exits 0 silently, which is already handled.
  - Document this as a deliberate difference from session-0 commands.

### E. Output pipes and handles

- **I15. Pipe objects are not tied to a session; handles belong to processes.**
  - The box makes both anonymous pipes as today (piped.rs:220), with its own default DACL (Administrators and SYSTEM).
  - The write ends go into the helper as non-inheritable copies, so none leaks into a process the helper itself starts. The helper's launch makes its own inheritable copies inside the spawn lock and hands exactly NUL plus those two to the child through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, the same rule as create.rs:82.
  - The box's copies of the write ends close once the child holds its own (piped.rs:151); the helper's are closed by I9.
  - Forwarders start only after the marker. The read ends stay in the box.
- **I16. The helper uses the same creation rules as the box.** It goes through `check_launch` and the handle list, with the desktop-app policy `DISABLE_PROCESS_TREE` (piped.rs:148, so packaged children stay in the job), `CREATE_NO_WINDOW` and `CREATE_SUSPENDED`. `lpDesktop` is set to `WinSta0\Default` explicitly; `startup_info` at create.rs:278 sets none today. In practice the helper calls a gate-win32 function and does not re-implement any of this.

### F. Environment and working folder

- **I17. The box builds the environment block and the helper passes it on unchanged.** One code path: `Variables::from_process().with_build_servers(..)` plus `with_safe_directory` (run.rs:736-739, :814). It is sent as bytes: base64 of UTF-16LE, so unpaired surrogates survive. The helper builds its `EnvironmentBlock` from those bytes only through a constructor that refuses a block that does not end in two NULs or holds an empty or `=`-only entry, since `CreateProcessW` reads until the double NUL.

  `safe.directory` is still needed, because the token is non-elevated and the work folders are owned by Administrators. Session-0 variables such as `SSH_*` go along, as they do today.
- **I18. Every path sent to the helper must exist in the interactive logon session.**
  - The application, the working folder and the program resolved by `find_program` (run.rs:744) all have to resolve there.
  - Drive letters are per logon session ([Local and Global MS-DOS Device Names](https://learn.microsoft.com/windows-hardware/drivers/kernel/local-and-global-ms-dos-device-names)). A `subst` or `net use` drive made under the ssh logon does not exist for the helper.
  - So the builder root must be on a real volume. A violation shows up as a post-marker 127.
  - Cheap upgrade: the helper checks the folder before the marker and refuses `desktop` instead.

### G. Reaching the command: stop, kill, ceiling, heartbeat, client gone

- **I19. Everything goes through the box's own job handle, so the session does not matter.**
  - `gate box stop` opens `Local\gate-remote-<key>-<nonce>` from session 0 (stop.rs:170), the same namespace the box created it in.
  - Kill: `terminate_and_wait` (stop.rs:148).
  - Ceiling: `job.accounting()` (ceiling.rs:236, 268-271).
  - Heartbeat: heartbeat.rs:85.
  - Leftovers: `end_leftovers` (run.rs:1304 → `crates\gate\src\run.rs:2117`), with `keep = []` since there is no donor.
  - Client gone: run.rs:1404, 1451.
  - Panic: `Stopper` (run.rs:1179).

  Accounting adds up through the immediate job (docs, same page). **Unverified:** `TerminateJobObject` on a job nested under the Task Scheduler's job; the spike measured only the close of a kill-on-close job.
- **I20. Never call `IRunningTask::Stop` or `IRegisteredTask::Stop` after adoption, and never register with `StopExisting`.** The Task Scheduler's job is above the box's, and stopping the instance may end it (**unverified**). End a lingering helper with `TerminateProcess` only.

### H. The one desktop

- **I21. Lock order: the work-folder lock, then the desktop mutex, never the other way round.**
  - The mutex is `Global\gate-box-desktop` (machine-wide, so two PCs share it), with a box seam to rename it in tests.
  - It is taken just before the job is created, after checkout and restore, so the desktop is held only while the command runs.
  - It is taken on the run's main thread and released on the same thread: mutexes belong to threads (`crates\gate-win32\src\mutex.rs:13-14`).
  - It is released once the job is empty and the output drained, before the back commit.
- **I22. The wait watches the stop event and has a deadline.**
  - Loop `acquire(WAIT_STEP)` with stop looks (mutex.rs:73).
  - Heartbeats keep beating uncapped meanwhile, so the PC's stall check is quiet.
  - Say one prose line when the wait starts.
  - A stop during the wait gives "stopped before the command started", exit 1, as `before_the_job` does (run.rs:1072).
- **I23. Abandoned is a take.** It means the last holder died (mutex.rs:97). Say a line, mirroring the slots' "which a killed gate had left held". Because of I14 the dead holder's command is being ended too. There is a millisecond race: abandonment can come before the dead box's handles close.

### I. Session state

- **I24. Before the start, the target is the active console session, owned by the builder, unlocked, on the Default input desktop.**
  - The box checks first: `WTSGetActiveConsoleSessionId`, then `WTSQuerySessionInformation(WTSSessionInfoEx)` for `WTSActive`, the user name, and `WTS_SESSIONSTATE_UNLOCK`. Those flag values are reversed only on Windows 7 ([WTSINFOEX_LEVEL1](https://learn.microsoft.com/windows/win32/api/wtsapi32/ns-wtsapi32-wtsinfoex_level1_w)).
  - The helper confirms from inside the session in HELLO: `OpenInputDesktop` succeeds and is named `Default`. A UAC prompt or the lock screen is a secure desktop.
  - `RunEx` uses `TASK_RUN_USE_SESSION_ID` with that session. Note that with no user given, the docs say the Task Scheduler runs the task "as the user who is logged on to the specified session". That is why I3's token check matters.
  - The builder logs its user on at the console after every reboot, through Sysinternals Autologon (owner ruling on GATE-231), so a console session exists for this check to find; builder setup keeps it so.
- **I25. A session change mid-run is a command failure, never a fallback.** Logging off ends the session's processes; the box sees the exit and the status stands. Locking, RDP takeover and disconnection let the command run on and fail as it fails. Recommendation: one WTS read after the wait, and a note line if the session ended, changed or locked, so a failure has a cause attached.

### J. The helper dies, lies, hangs or is impersonated

| Case | What happens |
|---|---|
| Dies before connecting | The box's wait includes the helper's process handle (from `EnginePID`), so it returns at once and refuses `desktop` with the exit status. If the pid is still 0, `Refresh` briefly and fall back to the deadline. |
| Hangs | Deadlines: connect 10 s, each message 5 s (seams to shorten). Then `TerminateProcess`, the I10 sweep, and a refusal. |
| Lies (bad handle values, a pid that does not match, a running process) | Verification fails, the I10 sweep runs, and the box refuses. A helper's copy can never carry more access than the helper had. |
| Impersonated (another client connects first) | Pid mismatch, disconnect, keep listening. Endless reconnects run out the deadline and refuse: a denial of service that ends in a local run. |
| Squatter of another identity | The box's `CreateNamedPipe` with `FIRST_PIPE_INSTANCE` fails and the box refuses before `RunEx`. Against a pipe the helper reaches, I4. |
| Helper of another build | HELLO carries the version text; a mismatch refuses `desktop`. |

### K. Task Scheduler settings, as invariants

- `MultipleInstancesPolicy=Parallel`. The default is IgnoreNew, and StopExisting must never be used.
- `Priority=4`, `AllowDemandStart=true`, `Enabled=true`.
- `DisallowStartIfOnBatteries=false` and `StopIfGoingOnBatteries=false` (the defaults are true).
- `ExecutionTimeLimit=PT0S`. The box bounds the helper itself, and a Task Scheduler limit could end its job (**unverified**).
- No triggers.
- Principal: builder SID, `InteractiveToken`, `LeastPrivilege`. The spike measured that this gives Medium integrity.
- Action: the helper's full path, with `$(Arg0)`.
- The box reads the registered definition on each desktop run. A missing, disabled or different task is a refusal, and its why names the fix.

---

## 3. Sites that must obey each invariant

Paths are relative to the repo root, at feat/toolchain-manifest b903f50.

| Site | Today | For a desktop run | Invariants |
|---|---|---|---|
| `crates\gate\src\boxside\run.rs:266-297` `run` | lock (276), stop event (290) | Optional cheap WTS refusal right after the lock, before checkout | I24, I7 |
| `run.rs:497-545` `checked_out` | restore (520), command (524) | Branch at 524 to a separate `desktop_command`, no flag threaded through | I8 |
| `run.rs:734-804` `command` | environment (736-739), `PipedLaunch` with restricted token (754-761), `before_the_job` (763), `create_job` (764), donor (766), park (767), `start_command` (779), ceiling (791), `watch` (799) | Same environment and resolution. Then mutex, session check, kill-on-close job, pipe, `RunEx`, hand-off, adopt, marker, resume. No donor, no token. Park after adoption. | I8, I11, I14, I17, I21 |
| `run.rs:904-919` `start_command` | marker first (905) | Marker after adoption, before the stop look | I6, I7 |
| `run.rs:991-1018` `launch` | assign (1004), stop look (1006), resume (1010) | Reused on adopted handles | I9, I19 |
| `run.rs:1024-1047` `create_suspended` | own-token retry | Never reached for desktop runs | I8 |
| `run.rs:1139-1154` `start_failed` | box's breakaway check | Not used for a helper's `FAILED` | I7 |
| `run.rs:1166-1175` `create_job` | `Job::create` | `Job::create_kill_on_close` for desktop runs | I14 |
| `run.rs:1211-1259` `watch`; `1304` `end_what_it_left`; `1372-1412` `Watched::wait` | unchanged | `keep=[]`; release the mutex after `drained` or the kill ending | I19, I21 |
| `crates\gate\src\boxside\stop.rs:30, 148, 168-174, 186` | by name | Unchanged, and it works because the job name lives in session 0 | I19 |
| `crates\gate-win32\src\piped.rs:87-89, 127-153` | breakaway (88), job handle (129), flags (144-147), policy (148) | The helper's creation mirrors this without breakaway and with `NORMAL_PRIORITY_CLASS` | I12, I13, I16 |
| `crates\gate-win32\src\create.rs:82-136, 278-288, 293-370` | spawn lock, handle list, no `lpDesktop` | Exposed for the helper; `lpDesktop` added | I15, I16 |
| `crates\gate-win32\src\launch.rs:415-425` `Suspended` (fields `pub(crate)`), `464`, `480-492`, `495-501` | built only from `CreateProcess` | New constructor that adopts a process and thread handle; Drop still ends it unstarted | I9, I10 |
| `crates\gate-win32\src\job.rs:321, 392-397, 404-412, 438` | | `holds` for the job-specific membership check | I9, I14 |
| `crates\gate-win32\src\console.rs:369-371` | donor put in the job first | Never called for desktop runs | I11 |
| `crates\gate-win32\src\mutex.rs:50-101` | null security attributes, abandoned flag | Desktop mutex; Administrators and SYSTEM through the elevated default DACL | I21-I23 |
| `crates\gate\src\boxside\payload.rs:65, 127, 403` | key readers | `Desktop` (bool) and the wait seconds | I8 |
| `crates\gate\src\remote\mod.rs:68-111` | `Refusal` | Add `Desktop`; fix the ordering doc at 94-99 | I6, I7 |
| `crates\gate\src\remote\client.rs:2664-2718` | `ProtocolLines` | Desktop tag mapped to `FallbackReason::Desktop` | I7 |
| `crates\gate\src\remote\payload.rs:174-189` `run_word` | keys | `Desktop` and the wait | I8 |
| `crates\gate-state\src\journal_writer.rs:225-261` | `FallbackReason` | `Desktop` ("desktop"): a journal format change, so fixtures and an inbox request to ops-tower (ARCHITECTURE.md:264) | |
| `crates\gate-router\src\config.rs:38, 111, 397`; `decide.rs:328, 659, 751, 902` | siblings consent | Desktop patterns and consent, same model | |
| `crates\gate-args\src\request.rs:54-95` | `BoxSeams`, `BoxVerb` | Desktop seams, plus the helper verb if it is a mode of gate.exe | |

---

## 4. Recommendations for the open decisions

**D1. Where the helper lives.** Take one more measurement before the brief:
- Run the spike rig's console `helper` to start yaat's `AutomationClientZOrderTests` with `CREATE_NO_WINDOW`.
- Look at the builder's console for a console window. On Windows 11 22H2+ the default terminal can be Windows Terminal.
- Read the test's priority class.

The decision:
- If it passes and no window appears: a hidden verb of gate.exe (`gate box desktop-helper <arg0>`). One file, nothing new to install or push, and the build is the same by construction.
- If a window appears and disturbs the test: a separate Windows-subsystem binary, with the helper's body in gate-win32 and the protocol in a pure module.

Worst case: the second binary. `push.rs` gains a second file in its stdin protocol, along with `install.ps1`, `suite/install.rs`, `suite/push.rs` and fake-ssh. Version skew between the two files is caught by the HELLO check and becomes a `desktop` refusal.

**D2. Registering the task.** Generate the task XML with a pure, unit-tested function. Register it through COM `ITaskFolder::RegisterTask(..., TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN)` with the K settings, at `\gate\desktop-helper`. The command is a hidden verb (`gate box register-desktop`) that builder setup runs once over ssh. An elevated session-0 process can register a same-user interactive-token task with no password; the spike measured that. The box checks the definition on every run (K).

Alternative: `gate push` re-registers on every push. That keeps the task in step with the exe, but push no longer "only replaces files", so it is an owner question.

Worst case: a builder that skipped setup refuses every desktop run with a why naming the verb, and the run goes local.

**D3. Pipe protocol.**
- Byte-mode pipe, frames of a u32 length plus UTF-8 JSON, 4 MiB cap, every field untrusted.
- Messages:
  - `HELLO{version, desktop names, input desktop ok}` (helper to box);
  - `SPAWN{application, command line, directory, environment as base64 UTF-16LE, stdout and stderr handle values}` (box to helper);
  - `CHILD{pid, process handle, thread handle}` or `FAILED{code}` (helper to box);
  - `DONE` (box to helper).
- The box words a `FAILED` from the code with its own `win32_message`, not from the helper's text.
- Overlapped I/O, waiting on the pipe event, the helper's process handle and the stop event, under a deadline.

Worst case: the protocol code is new, unsafe-heavy Win32 work. It is testable entirely on the PC (section 5).

**D4. Refusal tags and why texts.**
- Tag `desktop`, then the owner's line followed by `: <why>`, the same form as `layout` (design.md:1895): `gate: not routed: the builder has no desktop session: <why>`. The PC says `gate: remote <host> desktop; running here`.
- Fixed whys:
  - `nobody is logged on at the console`
  - `the console session is locked`
  - `the console session is <state>`
  - `<user> is logged on at the console, not <builder>`
  - `cannot read the console session: <error>`
  - `the desktop task <path> is not registered|is disabled|is not as gate registers it: <field>; run gate box register-desktop`
  - `the desktop task could not be started: <message>`
  - `the desktop helper did not answer in <n> s`
  - `the desktop helper exited before it answered (status <n>)`
  - `the desktop helper runs gate <v>, the box <v>`
  - `the desktop helper is not on the console's Default desktop`
  - `the desktop helper handed back <what>`
  - `the box cannot put the desktop command in its job: <error>`
- Busy desktop: tag `busy` with `gate: remote the builder's desktop is busy; waited <n> s`. It is load, not a fault, which matters for GATE-27's per-host failure record. If it were `desktop`, a busy desktop would mark the builder as broken.

Worst case: the journal's `busy` mixes a busy worktree with a busy desktop, and only the line tells them apart.

**D5. Detecting a busy desktop.** Use the I21-I23 mutex. The wait is bounded by a payload `DesktopWait`: what is left of the PC's 60 s remote wait, with a small floor. That matches the owner's "up to the usual remote wait".

Worst case:
- A losing run pays push, checkout and restore before it falls back.
- The desktop wait sits inside the run call, so it counts against the client's backstop (5 × ceiling + 60 s, design.md:1645). A run with a short `--timeout` can hit the BACKSTOP before its command starts. That needs an Integration Footguns entry, or a cap of the wait at the timeout.
- While a desktop run waits it holds its PC's remote slot, so other routed runs of that PC can fall back `full`.

**D6. The job.** Kill-on-close (I14). Worst case: a box killed by sshd ends the desktop command at once. That is the behaviour we want for the one desktop.

**D7. COM.** Use the `windows` crate with only `Win32_System_TaskScheduler`, `Win32_System_Com`, `Win32_System_Variant` and `Win32_Foundation`, confined to a new `task.rs` in gate-win32. The spike used `windows =0.62.2`; look up the current version that pairs with windows-sys 0.61.

Run the COM calls on a short-lived multithreaded-apartment thread, and release every interface after `EnginePID`.

The alternatives are hand-written vtables for five interfaces, or pwsh `Schedule.Service` per run (about 1 s and a pwsh dependency). Neither is better.

Worst case: compile time, and a second Windows API crate in the Layers rule (ARCHITECTURE.md:173).

**D8. Order of a desktop start.**
1. Lock, optional early WTS check, checkout, restore.
2. `before_the_job`, then the mutex.
3. Authoritative session check.
4. Kill-on-close job, then the pipe.
5. Task check, then `RunEx`. Open the helper, then the connection and its pid check, then HELLO.
6. Push the write ends, `SPAWN`, then `CHILD` or `FAILED`.
7. Pull, close the helper's copies, verify, assign, check membership.
8. `DONE`, then end the helper within a bound.
9. **Marker.**
10. Console-fallback line, stop look, resume, watch, drain.
11. Release the mutex, session note, back commit.

The helper ends the child it started only when its control pipe reads closed, or fails with a pipe-gone error (`ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED`), before `DONE` has been read: the box is gone and nothing will resume the child. Any other read error leaves the child. It never ends the child on a timeout alone, since the box may have pulled the child and be about to resume it; the box bounds the helper instead (step 8) and ends the child itself through the handles it pulled. `adopt::process_of_child` proves only who a pid is, never whether the box took it.

A received stdout or stderr value is used only when it passes `adopt::is_handle_value`, names a pipe, and is not the helper's own control-pipe handle (`adopt::check_received_output`).

---

## 5. Tests: what the PC can and cannot cover

**Seam:** `--test-box-desktop spawn` replaces only the Task Scheduler start and the WTS read. The box starts the helper itself, inside a fresh anonymous job without breakaway (which mimics the Task Scheduler's job) and at below-normal priority. Everything else is the real code. Companions:
- `--test-box-desktop-session active|none|locked|other-user|disconnected`
- helper fault modes: `exit-before-hello`, `hang`, `wrong-version`, `lie-handle`, `exit-after-create`
- desktop-wait milliseconds and the mutex name

fake-tool gains steps that print the session, the priority class and the working folder.

**Covered locally:**
- framing and validation, the version gate;
- the marker after adoption, refusals before it, 127 after it;
- pushing the write ends, pulling the handles, `DUPLICATE_CLOSE_SOURCE`;
- every verification and the I10 sweep;
- assigning into an empty job from a foreign job that refuses breakaway (the nesting rule);
- kill-on-close when the box dies;
- `gate box stop` by name, ceiling, heartbeat, client gone;
- output end-of-file once the helper's copies close;
- explicit normal priority under a below-normal parent;
- no donor, the token and console seams inert;
- `start_failed` bypassed;
- mutex busy, timeout, abandoned, a stop during the wait;
- a client that connects first; a pre-created pipe name.

Plus unit tests: task XML, `Arg0` parsing, the why texts, and a security-descriptor round trip.

**Optional, PC-only, ignored by default:** register a temporary task and run it with `RunEx` in the developer's own session. That covers `RunEx`, `$(Arg0)`, `EnginePID`, the real Task Scheduler job, `Parallel` and Priority 4, but it flashes a helper window on the PC.

**Not covered locally:**
- the session-0 to session-1 boundary and the documented same-session rule;
- a High box pulling handles from a Medium helper;
- the DACL against another identity;
- WTS lock and console states;
- `TASK_RUN_USE_SESSION_ID`;
- D3D, Z-order and foreground;
- logoff, lock or RDP during a run;
- the Task Scheduler's job behaviour on an instance stop.

**Builder hand proof, to record in the issue:**
- membership through `IsProcessInJob(child, box_job)`, `process_ids` and rising accounting;
- `TerminateJobObject` reaches the child;
- the child's priority class and I/O priority;
- `RunEx` with `USE_SESSION_ID`, and the Task Scheduler's start latency;
- yaat's Z-order test passes through the real path;
- a refusal for each of: locked, logged off, RDP-moved;
- `gate box stop` during a desktop run;
- the box killed mid-run ends the command.

---

## 6. Where I disagree with your position

1. **"Its kill-on-close job".** The run's job is not kill-on-close today (run.rs:1166; design.md:1939). I agree it should be for desktop runs (I14), but the brief must say so as a change, along with its consequences: no planted handle, and a `stop` after a lost connection finds nothing.
2. **"Refuse when the box is busy".** The owner ruled: wait up to the remote wait, then run here. Also choose the tag: I recommend `busy`, not `desktop` (D4).
3. **"Receives the child's pid, pulls handles".** Pull by handle value from the helper's table, and use the pid only as a cross-check. Never `OpenProcess` by pid (I9).
4. **"Supervises it as a session-0 command".** Close, but not the same:
   - no donor and no session-0 member ever (I11);
   - no restricted token, no breakaway;
   - kill-on-close;
   - `start_failed` must be bypassed;
   - explicit priority;
   - the marker moves;
   - a nested `gate run` inside a desktop command takes `Local\` slots in session 1's namespace, apart from session 0's (design.md:444). That is a known limit to document.
5. **The refusal list is incomplete.** Add: locked, session not active, another user at the console, a secure desktop, the helper not answering, dying or of another build, a task that is disabled or different, and a failed hand-off.
6. **Main risk.** Same-user posing is in scope only as correctness (I1). The higher risks are:
   - (a) the Z-order target is unmeasured under hidden-console, helper-started, below-normal conditions;
   - (b) the documented same-session rule for jobs contradicts the one measurement, so I11 and the membership check are hard requirements;
   - (c) the helper-side cross-identity squat (I4);
   - (d) the Task Scheduler defaults (IgnoreNew, priority 7, batteries, `S_OK` for a disabled task).
7. **The spike report contradicts itself on GATE-261.** It says the Event Log grant and NETWORK deny-only "still matter"; the map says they stop mattering. An interactive token carries no NETWORK group, and, from memory of the default Application channel ACL, Interactive Users can write. So the grant is likely moot for desktop commands. **Unverified:** check with `wevtutil gl Application` on the builder.

---

## 7. Items for the plan and the docs

- **Pre-existing, for the backlog (GATE-271 territory):** ssh runs `%LOCALAPPDATA%\gate\bin\gate.exe` elevated on the builder, but the default profile ACL lets any Medium process of the builder user replace it. The ACL is unmeasured on the builder.
- **Glossary terms:** desktop command, desktop helper, desktop task, console session, session 0.
- **ARCHITECTURE.md:** the Task Index row the map proposed. Integration Footguns:
  - the desktop wait counts against the client's backstop;
  - `FallbackReason::Desktop` is a journal format change;
  - box launch changes must be mirrored in the helper's creation.
- **design.md:** the weaker marker rule (I6), the kill-on-close desktop job, and the builder-setup items. Builder setup should also cover: no screen lock or screensaver, no display sleep, notifications off, and never RDP in as the builder user.
