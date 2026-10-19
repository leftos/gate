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

The invariants are in [design.md, The desktop run's design](../design.md#the-desktop-runs-design).

The labels used below and by GATE-385 map to its paragraphs: I1 to I5 are "Trust boundary" (I3 is the pid pin and checks in the hand-off text of The box side); I6 to I8 are "Where the command can run"; I9 and I10 are "Adopting the child"; I11 to I14 are "The job"; I15 and I16 are "Output pipes and creation"; I17 and I18 are "Environment and folder"; I19 and I20 are "Reaching the command"; I21 to I23 are "The desktop lock"; I24 and I25 are "Session state".

J is the table of helper failures, and K is "The task's settings".

Not built: the optional single WTS read after the wait, with a note line when the session ended, changed or locked (I25).

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
| `crates\gate-win32\src\launch.rs:415-425` `Suspended` (fields `pub(crate)`), `464`, `480-492`, `495-501` | built only from `CreateProcess` | New constructor that adopts a process and thread handle and an `EndReport` sink; Drop still ends it unstarted, and a drop that cannot end it calls the sink once with the pid and the error, never writing to stderr | I9, I10 |
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

**D2. Registering the task.** Generate the task XML with a pure, unit-tested function. Register it through COM `ITaskFolder::RegisterTask(..., TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN)` with the K settings, at `\gate\desktop-helper`. The command is a hidden verb (`gate box register-desktop`) that `gate push` runs on every builder after it installs the helper beside gate.exe (the script `tools/install.ps1` places the helper first; the push places gate.exe first), and that builder setup may run by hand over ssh. An elevated session-0 process can register a same-user interactive-token task with no password; the spike measured that. The box checks the definition on every run (K).

The verb registers the task in the folder `\gate`, makes the folder when it is missing, and gives both the same protected descriptor: owner Administrators, full access for SYSTEM and Administrators, read for the builder user's SID. The string the code writes is `O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;<user SID>)`; Windows reads it back as `O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;<user SID>)`, the `AI` being the auto-inherited flag it sets, so a hand check compares the ACEs and not the flags.

A task is registered with `TASK_DONT_ADD_PRINCIPAL_ACE`, so the user gets no write access through the principal, and is read back and checked after registering.

A folder or task that already exists with a weaker owner or DACL is refused, never adopted or repaired: whoever could change it could have replaced the task the box then runs. The verb says `gate-register: failed: the task is set up wrong: <what differs>`. To recover from a squatted `\gate` folder or task, an administrator deletes it on the builder (Task Scheduler, or `schtasks /delete /tn \gate\desktop-helper`, and the folder if it too is the squatter's), then runs the verb again.

`gate push` re-registers on every push, which keeps the task in step with the exe; push no longer only replaces files. A failed registration is said on that builder's line and does not change push's exit code.

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

**D5. Detecting a busy desktop.** Use the I21-I23 mutex. The wait is bounded by a payload `DesktopWait`: the constant usual remote wait, 60 s (`REMOTE_LIMIT`), which the box caps at the run's timeout. That matches the owner's "up to the usual remote wait".

Worst case:
- A losing run pays push, checkout and restore before it falls back.
- The desktop wait sits inside the run call, so it counts against the client's backstop (5 × ceiling + 60 s, design.md:1645). A run with a short `--timeout` can hit the BACKSTOP before its command starts. That needs an Integration Footguns entry, or a cap of the wait at the timeout.
- While a desktop run waits it holds its PC's remote slot, so other routed runs of that PC can fall back `full`.

**D6. The job.** Kill-on-close (I14). Worst case: a box killed by sshd ends the desktop command at once. That is the behaviour we want for the one desktop.

**D7. COM.** Use the `windows` crate (0.62.2) with only `Win32_System_TaskScheduler`, `Win32_System_Com`, `Win32_System_Ole`, `Win32_System_Variant` and `Win32_Foundation`, confined to `task.rs` in gate-win32.

Run the COM calls on a short-lived multithreaded-apartment thread, and release every interface before it ends.

The Task Scheduler's class is made from `taskschd.dll`, loaded once for the process by its full path in the system folder (`LOAD_LIBRARY_SEARCH_SYSTEM32`) and never freed, never looked up in the registry, where a user's own class entry could stand in for it; the loaded module's path is checked against the system folder.

`run_task(&TaskStart { path, argument, session_id, helper, pid_wait }, deadline, wake)` returns `StartedTask::Process(pid)`, or `NoPid` when the task started but the Task Scheduler gave no process within `pid_wait` (the helper may be running; only the caller's deadline bounds the wait for it). It reads the definition first and checks every K setting plus the task's owner (SYSTEM or Administrators) and DACL (no one else allowed more than read); it runs with `TASK_RUN_USE_SESSION_ID | TASK_RUN_IGNORE_CONSTRAINTS`.

`TaskError` tells the caller whether the helper may exist. `NotRegistered`, `SetUpWrong` and `Failed`: nothing was started. `StartedUnknown`: started, then a later call failed. `TimedOut`, `Woken` and `Unanswered` (the wait failed or the start thread panicked): the start may have happened, the thread that asked is left to finish, one that has not yet run the task starts nothing, and the caller drops its pipe server. Every text is escaped and cut, so none can carry a line break or control character to the console.

The alternatives are hand-written vtables for five interfaces, or pwsh `Schedule.Service` per run (about 1 s and a pwsh dependency). Neither is better.

Worst case: compile time, and a second Windows API crate in the Layers rule (ARCHITECTURE.md, gate-win32).

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

The helper ends the child it started only when its control pipe reads closed, or fails with a pipe-gone error (`ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED`), before `DONE` has been read: the box is gone and nothing will resume the child. Any other read error leaves the child. It never ends the child on a timeout alone, since the box may have pulled the child and be about to resume it; the box bounds the helper instead (step 8) and ends the child itself through the handles it pulled. The helper ends the child through the copy of its process handle that its release kept, never by finding the pid again, so a reused pid is never ended.

A received stdout or stderr value is used only when it passes `adopt::is_handle_value`, names a pipe, and is not the helper's own control-pipe handle (`adopt::check_received_output`).

---

## 5. Tests: what the PC can and cannot cover

**Seam:** `--test-box-desktop ok` replaces only the Task Scheduler start and the WTS read. The box starts the helper itself, inside a fresh anonymous job without breakaway (which mimics the Task Scheduler's job) and at below-normal priority. Everything else is the real code. Companions:
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

- **Addressed by GATE-425:** ssh used to run a `gate.exe` elevated from a folder any Medium process of the builder user could write. `gate push` now installs into the admin-owned `C:\Program Files\gate\bin`, and the box refuses every verb when its own image or the helper is not installed where only administrators and SYSTEM can change them (design.md, "Install tripwire"). The user's own Medium processes stay outside the boundary (I1): they cannot replace the trusted install, and anything else they can do is what I1 already grants them.
- **Glossary terms:** desktop command, desktop helper, desktop task, console session, session 0.
- **ARCHITECTURE.md:** the Task Index row the map proposed. Integration Footguns:
  - the desktop wait counts against the client's backstop;
  - `FallbackReason::Desktop` is a journal format change;
  - box launch changes must be mirrored in the helper's creation.
- **design.md:** the weaker marker rule (I6), the kill-on-close desktop job, and the builder-setup items. Builder setup should also cover: no screen lock or screensaver, no display sleep, notifications off, and never RDP in as the builder user.
