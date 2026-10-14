//! `gate push`: copies the installed `%LOCALAPPDATA%\gate\bin\gate.exe` to every builder `remote.json` names, one
//! after another. Each builder gets one ssh call whose line is a PowerShell script, [`SCRIPT`], sent encoded, and the
//! exe as base64 text on ssh's stdin; the script checks the copy's SHA-256 and installs it on the builder as
//! `tools/install.ps1` installs here, with the old exe set aside, and says what it did on `gate-push: ` lines, which
//! this side reads into the `push: ` lines. Every builder is tried; one that failed makes the exit 1.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_args::PushRequest;
use gate_win32::{EnvironmentBlock, Job, LaunchError, PipedConsole, PipedFed, PipedLaunch, Running};
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::answer::FAILED_STATUS;
use crate::boxside::payload::escaped;
use crate::console::Console;
use crate::remote::git;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::ssh_words;
use crate::resolve::Environment;
use crate::route::read_remote_json;

/// How long one builder's call may take before it is killed.
const BOUND: Duration = Duration::from_secs(300);
/// How long the installed exe's `--version` may take here.
const VERSION_BOUND: Duration = Duration::from_secs(30);
/// What starts each of the builder script's lines this side reads.
const MARK: &str = "gate-push: ";
/// The most `gate-push: ` lines kept of one call; a later one takes the last place, so the call's last is kept.
const MARKED_CAP: usize = 16;
/// The most bytes kept of one line of a call's output; the rest of a longer line is skipped.
const LINE_CAP: u64 = 64 * 1024;
/// What the builder's cmd runs: pwsh with the script, as UTF-16LE base64, its one other word.
const PWSH: &str = "pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand ";
/// The stand-in for the exe's SHA-256, as upper-case hex, in [`SCRIPT`].
const SHA_SLOT: &str = "@SHA@";

/// The builder script, as `tools/install.ps1` installs: it reads the exe's base64 from stdin and refuses bytes whose
/// SHA-256 is not the one this side put in for [`SHA_SLOT`]. In `%LOCALAPPDATA%\gate\bin`, made when missing and
/// refused when it is a link, it deletes each `gate.exe.*.old`, and each `gate.exe.<pid>.tmp` of another pid, that no
/// process holds, saying `note` and why for one it cannot delete. With a `gate.exe` of the same SHA-256 it says `same`
/// and its version; else it writes `gate.exe.<pid>.tmp` afresh, moves the old exe aside to
/// `gate.exe.<yyyyMMddHHmmss>.<pid>.old` and the temp into place, checks the installed copy's SHA-256, and on a
/// failure moves the aside copy back; then it says `replaced` and the old version when it read one, and `installed`
/// and the new one. A failure says `failed` and why, and exits 1. Each version read is bounded at 30 s.
const SCRIPT: &str = r#"$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
function Say($k,$t){[Console]::Out.WriteLine("gate-push: $k $t")}
function Fail($t){Say failed $t;exit 1}
function Ver($p){try{$x=[Diagnostics.Process]::Start([Diagnostics.ProcessStartInfo]@{FileName=$p;Arguments='--version';RedirectStandardOutput=1;RedirectStandardError=1;UseShellExecute=0})}catch{return ''}
$o=$x.StandardOutput.ReadToEndAsync();$e=$x.StandardError.ReadToEndAsync()
if(!$x.WaitForExit(30000)){$x.Kill($true);return ''}
if(($o.Result -split '\r?\n')[0] -notmatch '^gate\s+(.+?)\s*$'){return ''};$Matches[1]}
function Held($p){try{[IO.File]::Open($p,'Open','ReadWrite','None').Dispose();$false}catch{$_.Exception.GetBaseException().HResult -eq -2147024864}}
function Drop($p){Remove-Item -LiteralPath $p -Force -EA Ignore}
function Back($m){if(!$aside){Fail $m}
try{Move-Item -LiteralPath $aside -Destination $dest -Force}catch{Fail "$m; the previous gate.exe is at $aside"}
Fail "$m; the previous gate.exe is back in place"}
if(!$env:LOCALAPPDATA){Fail 'LOCALAPPDATA is not set'}
$dir=Join-Path $env:LOCALAPPDATA 'gate\bin';$dest=Join-Path $dir 'gate.exe';$aside=$null;$ci="could not install ${dest}:"
try{$bytes=[Convert]::FromBase64String([Console]::In.ReadToEnd())}catch{Fail "the copy could not be read: $($_.Exception.Message)"}
if([Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)) -ne '@SHA@'){Fail 'the copy arrived damaged'}
try{New-Item -ItemType Directory -Path $dir -Force|Out-Null}catch{Fail "could not create ${dir}: $($_.Exception.Message)"}
if((Get-Item -LiteralPath $dir).LinkType){Fail "$dir is a link"}
foreach($f in Get-ChildItem -LiteralPath $dir -File|?{$_.Name -like 'gate.exe.*.old' -or ($_.Name -match '^gate\.exe\.(\d+)\.tmp$' -and $Matches[1] -ne $PID)}){
try{Remove-Item -LiteralPath $f.FullName -Force}catch{if(!(Held $f.FullName)){Say note "could not delete $($f.FullName): $($_.Exception.Message)"}}}
if((Test-Path -LiteralPath $dest) -and (Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash -eq '@SHA@'){Say same (Ver $dest);exit 0}
$tmp="$dest.$PID.tmp";Drop $tmp
try{$s=[IO.File]::Open($tmp,'CreateNew','Write','None');try{$s.Write($bytes,0,$bytes.Length)}finally{$s.Dispose()}}catch{Drop $tmp;Fail "$ci $($_.Exception.Message)"}
$old=''
if(Test-Path -LiteralPath $dest){$old=Ver $dest;$aside="$dest.$(Get-Date -Format yyyyMMddHHmmss).$PID.old"
try{Move-Item -LiteralPath $dest -Destination $aside -Force}catch{Drop $tmp;Fail "$ci $($_.Exception.Message)"}}
try{Move-Item -LiteralPath $tmp -Destination $dest -Force}catch{$m=$_.Exception.Message;Drop $tmp;Back "$ci $m"}
if((Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash -ne '@SHA@'){Back 'the installed copy does not match'}
if($old){Say replaced $old}
Say installed (Ver $dest)
"#;

/// `gate --version`'s line, matched in any letter case as PowerShell's `-match` does: the version is what follows
/// `gate ` and its white space, trailing white space left out.
static VERSION: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?i)^gate\s+(.+?)\s*$").ok());

/// Runs `gate push` and returns its exit status.
pub(crate) fn push(request: &PushRequest, environment: &Environment, console: &mut Console) -> u32 {
    let variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let prepared = match prepared(environment, &variables) {
        Ok(prepared) => prepared,
        Err(line) => {
            console.err(&line);
            return u32::from(FAILED_STATUS);
        }
    };
    let ssh = match &request.seams.ssh {
        Some(stub) => std::path::absolute(stub).map_err(|error| format!("cannot run {}: {error}", stub.display())),
        None => Ssh::find().map(|ssh| ssh.program),
    };
    let bound = request.seams.bound_ms.map_or(BOUND, Duration::from_millis);
    let input = STANDARD.encode(&prepared.bytes);
    let mut status = 0;
    for host in &prepared.builders {
        console.out(&format!("push: {host}: sending gate {}", prepared.version));
        let heard = match &ssh {
            Ok(program) => called(&Call {
                program,
                args: ssh_words(host, &prepared.line),
                input: input.as_bytes(),
                variables: &variables,
                bound,
            }),
            Err(why) => Heard::failed(why.clone()),
        };
        let said = said(host, &prepared.version, &heard, bound);
        for line in &said.notes {
            console.err(line);
        }
        for line in &said.out {
            console.out(line);
        }
        if let Some(line) = &said.failure {
            console.err(line);
            status = u32::from(FAILED_STATUS);
        }
    }
    status
}

/// What every builder is sent: the builders, the exe's bytes and version as shown, and the ssh line.
struct Prepared {
    builders: Vec<String>,
    bytes: Vec<u8>,
    version: String,
    line: String,
}

/// The push's builders, exe and line, read from `LOCALAPPDATA`, the exe's version read with `variables` as its
/// environment; else the line saying why there is nothing to push or nowhere to push it.
fn prepared(environment: &Environment, variables: &[(OsString, OsString)]) -> Result<Prepared, String> {
    let folder = environment
        .local_app_data
        .as_deref()
        .map(Path::new)
        .filter(|folder| folder.is_absolute())
        .ok_or_else(|| "push: no remote.json: LOCALAPPDATA is not an absolute folder".to_owned())?;
    let gate = folder.join("gate");
    let builders = builders(&gate.join("remote.json"))?;
    let exe = gate.join("bin").join("gate.exe");
    let bytes = installed(&exe)?;
    let version = shown(&local_version(&exe, variables)?);
    let line = push_line(&sha256_hex(&bytes));
    Ok(Prepared {
        builders,
        bytes,
        version,
        line,
    })
}

/// The builders the routing file at `path` names, read as routing reads it; else the line saying it is missing or why
/// it is ignored, a bad `hosts` included.
fn builders(path: &Path) -> Result<Vec<String>, String> {
    let ignored = |reason: &str| format!("push: ignored {}: {reason}", path.display());
    match read_remote_json(path) {
        Ok(None) => Err(format!("push: no remote.json at {}", path.display())),
        Err(problem) => Err(ignored(&problem.reason)),
        Ok(Some(config)) => config.builders().map(<[String]>::to_vec).map_err(ignored),
    }
}

/// The installed exe's bytes; else the line saying there is none or it cannot be read.
fn installed(exe: &Path) -> Result<Vec<u8>, String> {
    if !fs::metadata(exe).is_ok_and(|metadata| metadata.is_file()) {
        return Err(format!("push: no installed gate at {}; run tools/install.ps1", exe.display()));
    }
    fs::read(exe).map_err(|error| format!("push: cannot read {}: {error}", exe.display()))
}

/// The installed exe's version, from its `--version`, a call bounded as a builder's is, at [`VERSION_BOUND`]; else
/// the line saying it could not be read.
fn local_version(exe: &Path, variables: &[(OsString, OsString)]) -> Result<String, String> {
    let heard = called(&Call {
        program: exe,
        args: vec![OsString::from("--version")],
        input: b"",
        variables,
        bound: VERSION_BOUND,
    });
    let version = match heard.end {
        End::Exited(_) => heard.first.as_deref().and_then(version_of),
        End::TimedOut | End::Failed(_) => None,
    };
    version.ok_or_else(|| format!("push: could not read the version of {}", exe.display()))
}

/// What `--version`'s output says after `gate `, as `tools/install.ps1`'s `Get-GateVersion` reads it: its first line
/// matched in any letter case, `None` when that line is no `gate <version>`.
fn version_of(output: &str) -> Option<String> {
    let first = output.lines().next()?;
    VERSION
        .as_ref()?
        .captures(first)
        .and_then(|captures| captures.get(1))
        .map(|version| version.as_str().to_owned())
}

/// `bytes`' SHA-256 as upper-case hex, as `Get-FileHash` and `Convert.ToHexString` write it.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut hex, byte| {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02X}");
        hex
    })
}

/// The line each builder's cmd runs: pwsh with [`SCRIPT`], `sha` put in, encoded as UTF-16LE base64, a form cmd passes
/// on with no quoting.
fn push_line(sha: &str) -> String {
    encoded_line(&SCRIPT.replace(SHA_SLOT, sha))
}

/// The line that runs `script` through pwsh, encoded as UTF-16LE base64.
fn encoded_line(script: &str) -> String {
    let wide: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!("{PWSH}{}", STANDARD.encode(wide))
}

/// One call: the program, its words, what it is fed on stdin, its whole environment and how long it may take.
struct Call<'a> {
    program: &'a Path,
    args: Vec<OsString>,
    input: &'a [u8],
    variables: &'a [(OsString, OsString)],
    bound: Duration,
}

/// How a call ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// It exited with this status.
    Exited(u32),
    /// The bound ended it.
    TimedOut,
    /// It could not be made or waited for, for this reason.
    Failed(String),
}

/// What a call said: how it ended, the first line of its standard output, its first [`MARKED_CAP`] `gate-push: `
/// lines, the last in the last place, its last standard error line that is not blank, and the first problem feeding or
/// reading it. Every line is cut to [`LINE_CAP`] bytes.
#[derive(Debug)]
struct Heard {
    end: End,
    first: Option<String>,
    marked: Vec<String>,
    last_error: Option<String>,
    problem: Option<String>,
}

impl Heard {
    /// A call that could not be made, for `why`.
    fn failed(why: String) -> Heard {
        Heard {
            end: End::Failed(why),
            first: None,
            marked: Vec::new(),
            last_error: None,
            problem: None,
        }
    }
}

/// Makes `call` in an anonymous job that ends every process in it once the call ends: its stdin fed, its two streams
/// read, and the whole job ended at the bound.
fn called(call: &Call<'_>) -> Heard {
    started(call).unwrap_or_else(Heard::failed)
}

fn started(call: &Call<'_>) -> Result<Heard, String> {
    let shown = call.program.display().to_string();
    let job = Job::anonymous_kill_on_close().map_err(|error| format!("cannot make a job for {shown}: {error}"))?;
    let cannot_start = |error: LaunchError| format!("cannot start {shown}: {error}");
    let PipedFed {
        suspended,
        stdin,
        stdout,
        stderr,
    } = launch(call)
        .map_err(|error| format!("cannot start {shown}: {error}"))?
        .start_suspended_within_fed(&job)
        .map_err(cannot_start)?;
    let running = suspended.assign_and_resume(&job).map_err(cannot_start)?;
    thread::scope(|scope| {
        let fed = scope.spawn(|| fed(stdin, call.input));
        let out = scope.spawn(|| kept(stdout));
        let err = scope.spawn(|| kept(stderr));
        let end = waited(&running, call.bound, &shown);
        // Closing the job's one handle ends every process left in it, so no pipe stays open past here.
        drop(job);
        let out = joined(out.join());
        let err = joined(err.join());
        let fed_problem = fed.join().map_or_else(|_| Some("the feeding thread panicked".to_owned()), Result::err);
        Ok(Heard {
            end,
            first: out.first,
            marked: out.marked,
            last_error: err.last,
            problem: fed_problem.or(out.problem).or(err.problem),
        })
    })
}

/// How the call `running`, shown as `shown`, ended: within `bound`, or at it.
fn waited(running: &Running, bound: Duration, shown: &str) -> End {
    match running.wait(bound) {
        Ok(Some(status)) => End::Exited(status),
        Ok(None) => End::TimedOut,
        Err(error) => End::Failed(format!("cannot wait for {shown}: {error}")),
    }
}

/// The launch of `call` on a hidden console of its own, in the working folder.
fn launch(call: &Call<'_>) -> io::Result<PipedLaunch> {
    let words: Vec<Vec<u16>> = std::iter::once(call.program.as_os_str())
        .chain(call.args.iter().map(OsString::as_os_str))
        .map(|word| word.encode_wide().collect())
        .collect();
    Ok(PipedLaunch {
        application: call.program.to_path_buf(),
        command_line: gate_args::command_line(&words),
        directory: std::env::current_dir()?,
        environment: EnvironmentBlock::new(call.variables.iter().cloned())?,
        console: PipedConsole::Hidden,
        token: None,
    })
}

/// Writes `input` and a line break to the call's stdin, then closes it; why it could not.
fn fed(mut stdin: File, input: &[u8]) -> Result<(), String> {
    stdin
        .write_all(input)
        .and_then(|()| stdin.write_all(b"\n"))
        .map_err(|error| format!("cannot feed the call: {error}"))
}

/// What one stream of a call kept: its first line, its `gate-push: ` lines up to [`MARKED_CAP`], its last line that is
/// not blank, and the problem that ended the reading early.
#[derive(Debug, Default)]
struct Kept {
    first: Option<String>,
    marked: Vec<String>,
    last: Option<String>,
    problem: Option<String>,
}

impl Kept {
    fn keep(&mut self, line: String) {
        if self.first.is_none() {
            self.first = Some(line.clone());
        }
        if line.starts_with(MARK) {
            if self.marked.len() >= MARKED_CAP {
                self.marked.pop();
            }
            self.marked.push(line.clone());
        }
        if !line.trim().is_empty() {
            self.last = Some(line);
        }
    }
}

/// What `stream` kept, read to its end a line at a time, each line cut to [`LINE_CAP`] bytes.
fn kept(stream: impl Read) -> Kept {
    let mut reader = BufReader::new(stream);
    let mut kept = Kept::default();
    loop {
        match next_line(&mut reader) {
            Ok(Some(line)) => kept.keep(line),
            Ok(None) => return kept,
            Err(error) => {
                kept.problem = Some(format!("cannot read the call's output: {error}"));
                return kept;
            }
        }
    }
}

/// The next line of `reader` without its line break, cut to [`LINE_CAP`] bytes with the rest of it skipped; `None` at
/// the end.
fn next_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    if reader.by_ref().take(LINE_CAP).read_until(b'\n', &mut bytes)? == 0 {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') {
        reader.skip_until(b'\n')?;
    }
    Ok(Some(String::from_utf8_lossy(&bytes).trim_end_matches(['\n', '\r']).to_owned()))
}

/// A reader thread's lines and problem; a panicked one kept none.
fn joined(read: thread::Result<Kept>) -> Kept {
    read.unwrap_or_else(|_| Kept {
        problem: Some("a reading thread panicked".to_owned()),
        ..Kept::default()
    })
}

/// One fact a `gate-push: ` line states.
#[derive(Debug, PartialEq, Eq)]
enum Fact {
    Note(String),
    Same(String),
    Replaced(String),
    Installed(String),
    Failed(String),
}

/// The fact `line` states, `None` for a line that states none.
fn fact(line: &str) -> Option<Fact> {
    let rest = line.strip_prefix(MARK)?;
    let (kind, text) = rest.split_once(' ').unwrap_or((rest, ""));
    let text = text.trim().to_owned();
    match kind {
        "note" => Some(Fact::Note(text)),
        "same" => Some(Fact::Same(text)),
        "replaced" => Some(Fact::Replaced(text)),
        "installed" => Some(Fact::Installed(text)),
        "failed" => Some(Fact::Failed(text)),
        _ => None,
    }
}

/// What a builder's push says: the notes for stderr, the lines for stdout, and the failure line for stderr when it
/// failed.
#[derive(Debug, PartialEq, Eq)]
struct Said {
    notes: Vec<String>,
    out: Vec<String>,
    failure: Option<String>,
}

/// What the push to `host` of the gate `version` says after its call was `heard`, limited by `bound`: each note the
/// builder gave, whatever the push's end, and the end.
fn said(host: &str, version: &str, heard: &Heard, bound: Duration) -> Said {
    let facts: Vec<Fact> = heard.marked.iter().filter_map(|line| fact(line)).collect();
    let notes = facts
        .iter()
        .filter_map(|fact| match fact {
            Fact::Note(text) => Some(format!("push: {host}: {}", shown(text))),
            _ => None,
        })
        .collect();
    let (out, failure) = match ended(host, version, heard, &facts, bound) {
        Ok(out) => (out, None),
        Err(reason) => (Vec::new(), Some(format!("push: {host} failed: {reason}"))),
    };
    Said { notes, out, failure }
}

/// The stdout lines of a push that succeeded, else why it failed.
fn ended(host: &str, version: &str, heard: &Heard, facts: &[Fact], bound: Duration) -> Result<Vec<String>, String> {
    let status = match &heard.end {
        End::TimedOut => return Err(format!("timed out after {} s", bound.as_millis().div_ceil(1000))),
        End::Failed(why) => return Err(why.clone()),
        End::Exited(status) => *status,
    };
    if let Some(reason) = facts.iter().find_map(|fact| match fact {
        Fact::Failed(reason) => Some(reason),
        _ => None,
    }) {
        return Err(shown(reason));
    }
    if status == 0
        && let Some(out) = succeeded(host, version, facts)
    {
        return Ok(out);
    }
    let reason = heard.last_error.as_deref().map(shown).or_else(|| heard.problem.clone());
    Err(reason.unwrap_or_else(|| "no answer".to_owned()))
}

/// The lines of a push whose builder said it holds `version` already, or installed it: `None` when it said neither.
/// A version the builder could not read is said as `version`, this PC's.
fn succeeded(host: &str, version: &str, facts: &[Fact]) -> Option<Vec<String>> {
    let theirs = |text: &str| if text.is_empty() { version.to_owned() } else { shown(text) };
    for fact in facts {
        if let Fact::Same(same) = fact {
            return Some(vec![format!("push: {host}: gate {} already installed", theirs(same))]);
        }
    }
    let installed = facts.iter().find_map(|fact| match fact {
        Fact::Installed(new) => Some(new),
        _ => None,
    })?;
    let replaced = facts.iter().find_map(|fact| match fact {
        Fact::Replaced(old) if !old.is_empty() => Some(format!("push: {host}: replaced gate {}", shown(old))),
        _ => None,
    });
    Some(
        replaced
            .into_iter()
            .chain([format!("push: {host}: installed gate {}", theirs(installed))])
            .collect(),
    )
}

/// Text a `push: ` line shows: escaped as the remote client escapes a builder's text, then cut to its first
/// [`git::FAILURE_BYTES`] at a character's boundary.
fn shown(text: &str) -> String {
    let escaped = escaped(text);
    let mut end = escaped.len().min(git::FAILURE_BYTES);
    while !escaped.is_char_boundary(end) {
        end -= 1;
    }
    escaped.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose files or programs cannot be made must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Cursor, Read};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::{Call, End, Heard, LINE_CAP, MARKED_CAP, PWSH, SCRIPT, Said, called, encoded_line, kept, push_line, said, sha256_hex, version_of};
    use crate::environment::Variables;
    use crate::run::find_program;

    const SHA: &str = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
    const BOUND: Duration = Duration::from_secs(300);

    #[test]
    fn the_push_line_fits_the_cap() {
        let line = push_line(SHA);
        assert!(line.starts_with(PWSH), "{line}");
        assert!(
            line.len() <= 8127,
            "the push line is {} characters, longer than the 8127 cmd takes",
            line.len()
        );
    }

    #[test]
    fn the_push_line_decodes_to_the_script() {
        let line = push_line(SHA);
        let encoded = line.strip_prefix(PWSH).expect("the line starts with pwsh's words");
        assert!(!encoded.contains(' '), "the script is one word");
        let bytes = STANDARD.decode(encoded).expect("the script is base64");
        let (pairs, rest) = bytes.as_chunks::<2>();
        assert!(rest.is_empty(), "UTF-16 is whole pairs of bytes");
        let wide: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        let script = String::from_utf16(&wide).expect("the script is UTF-16");
        assert_eq!(script, SCRIPT.replace("@SHA@", SHA));
        assert_eq!(
            script.matches(SHA).count(),
            3,
            "the hash checks the copy, the installed exe and the copy put in place"
        );
    }

    fn heard(status: u32, marked: &[&str], last_error: Option<&str>) -> Heard {
        Heard {
            end: End::Exited(status),
            first: None,
            marked: marked.iter().map(|line| (*line).to_owned()).collect(),
            last_error: last_error.map(str::to_owned),
            problem: None,
        }
    }

    fn out(lines: &[&str]) -> Said {
        Said {
            notes: Vec::new(),
            out: lines.iter().map(|line| (*line).to_owned()).collect(),
            failure: None,
        }
    }

    fn failure(line: &str) -> Said {
        Said {
            notes: Vec::new(),
            out: Vec::new(),
            failure: Some(line.to_owned()),
        }
    }

    #[test]
    fn machine_lines_map_to_the_owners_lines() {
        let said = |heard: &Heard| said("b1", "0.2.0", heard, BOUND);
        assert_eq!(
            said(&heard(0, &["gate-push: same 0.2.0"], None)),
            out(&["push: b1: gate 0.2.0 already installed"])
        );
        assert_eq!(
            said(&heard(0, &["gate-push: replaced 0.1.0", "gate-push: installed 0.2.0"], None)),
            out(&["push: b1: replaced gate 0.1.0", "push: b1: installed gate 0.2.0"])
        );
        assert_eq!(
            said(&heard(0, &["gate-push: installed 0.2.0"], None)),
            out(&["push: b1: installed gate 0.2.0"])
        );
        assert_eq!(
            said(&heard(0, &["gate-push: installed "], None)),
            out(&["push: b1: installed gate 0.2.0"]),
            "a version the builder could not read is this PC's"
        );
        assert_eq!(
            said(&heard(
                0,
                &[
                    "gate-push: note could not delete D:\\x\\gate.exe.1.old: denied",
                    "gate-push: installed 0.2.0"
                ],
                None
            )),
            Said {
                notes: vec![r"push: b1: could not delete D:\x\gate.exe.1.old: denied".to_owned()],
                ..out(&["push: b1: installed gate 0.2.0"])
            },
            "a note never fails the push"
        );
        assert_eq!(
            said(&heard(1, &["gate-push: failed the copy arrived damaged"], Some("noise"))),
            failure("push: b1 failed: the copy arrived damaged")
        );
        assert_eq!(said(&heard(0, &[], None)), failure("push: b1 failed: no answer"));
        assert_eq!(
            said(&heard(255, &[], Some("ssh: connect to host b1 port 22: Connection refused"))),
            failure("push: b1 failed: ssh: connect to host b1 port 22: Connection refused")
        );
        assert_eq!(
            said(&heard(255, &["gate-push: installed 0.2.0"], Some("Connection reset"))),
            failure("push: b1 failed: Connection reset"),
            "an ssh that failed with no failed line failed"
        );
        let timed_out = Heard {
            end: End::TimedOut,
            ..heard(0, &[], None)
        };
        assert_eq!(said(&timed_out), failure("push: b1 failed: timed out after 300 s"));
        assert_eq!(
            super::said("b1", "0.2.0", &timed_out, Duration::from_millis(250)),
            failure("push: b1 failed: timed out after 1 s")
        );
        assert_eq!(
            said(&Heard::failed("cannot run ssh: not found".to_owned())),
            failure("push: b1 failed: cannot run ssh: not found")
        );
    }

    #[test]
    fn a_call_that_ends_before_reading_its_input_says_why() {
        let ended_early = Heard {
            problem: Some("cannot feed the call: The pipe is being closed. (os error 232)".to_owned()),
            ..heard(255, &[], None)
        };
        assert_eq!(
            said("b1", "0.2.0", &ended_early, BOUND),
            failure("push: b1 failed: cannot feed the call: The pipe is being closed. (os error 232)")
        );
        let said_why = Heard {
            last_error: Some("Permission denied (publickey).".to_owned()),
            ..ended_early
        };
        assert_eq!(
            said("b1", "0.2.0", &said_why, BOUND),
            failure("push: b1 failed: Permission denied (publickey)."),
            "ssh's own reason comes first"
        );
    }

    #[test]
    fn builder_text_is_escaped_in_push_lines() {
        let said = |heard: &Heard| said("b1", "0.2.0", heard, BOUND);
        assert_eq!(
            said(&heard(1, &["gate-push: failed bad\u{1b}[31m\u{202e}x"], None)),
            failure(r"push: b1 failed: bad\u{1b}[31m\u{202e}x")
        );
        assert_eq!(
            said(&heard(0, &["gate-push: same 1\u{7}"], None)),
            out(&[r"push: b1: gate 1\u{7} already installed"])
        );
        assert_eq!(said(&heard(255, &[], Some("lost\u{200f}"))), failure(r"push: b1 failed: lost\u{200f}"));
        let prefix = "push: b1 failed: ".len();
        let escaped_first = said(&heard(1, &[&format!("gate-push: failed {}", "\u{7}".repeat(3000))], None));
        let line = escaped_first.failure.expect("a failure");
        assert_eq!(line.len(), prefix + 4096, "escaped first, then cut to 4 KiB");
        assert!(line.contains(r"failed: \u{7}\u{7}"), "the bell is escaped: {line}");
        let wide = said(&heard(1, &[&format!("gate-push: failed x{}", "\u{e9}".repeat(3000))], None));
        let line = wide.failure.expect("a failure");
        assert_eq!(line.len(), prefix + 4095, "a character the cut would split is left out");
    }

    #[test]
    fn a_version_line_parses_as_install_does() {
        assert_eq!(version_of("gate 0.1.0\r\n").as_deref(), Some("0.1.0"));
        assert_eq!(version_of("gate \t 1.2 beta  \nsecond").as_deref(), Some("1.2 beta"));
        assert_eq!(version_of("GATE 3").as_deref(), Some("3"), "matched in any letter case, as -match is");
        for output in ["", "gate", "gate ", "\ngate 1", "not gate 1", "gate1"] {
            assert_eq!(version_of(output), None, "{output:?}");
        }
    }

    #[test]
    fn endless_builder_output_keeps_bounded_lines() {
        let cap = usize::try_from(LINE_CAP).expect("the cap fits");
        let unbroken = kept(io::repeat(b'x').take(8 << 20));
        assert_eq!(
            unbroken.first.as_deref().map(str::len),
            Some(cap),
            "one line of 8 MiB is kept as its first 64 KiB"
        );
        assert_eq!(unbroken.last.as_deref().map(str::len), Some(cap));
        assert_eq!(unbroken.marked, Vec::<String>::new());

        let mut many = "gate-push: note x\n".repeat(200_000);
        many.push_str("gate-push: installed 1\n");
        let flooded = kept(Cursor::new(many));
        assert_eq!(flooded.marked.len(), MARKED_CAP);
        assert_eq!(
            flooded.marked.last().map(String::as_str),
            Some("gate-push: installed 1"),
            "the last line is kept"
        );

        let long = format!("gate-push: same {}\n", "y".repeat(100_000)).repeat(50);
        let wide = kept(Cursor::new(format!("{long}gate-push: installed 2\r\n")));
        assert_eq!(wide.marked.len(), MARKED_CAP);
        assert!(wide.marked.iter().all(|line| line.len() <= cap), "every line is cut to 64 KiB");
        assert_eq!(
            wide.marked.last().map(String::as_str),
            Some("gate-push: installed 2"),
            "a cut line's rest is skipped"
        );
        assert_eq!(wide.last.as_deref(), Some("gate-push: installed 2"));
        assert_eq!(wide.problem, None);
    }

    /// A folder of its own for `test`, empty.
    fn scratch(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-push-{}-{test}", std::process::id()));
        // A folder left by an earlier run of the same pid is cleared first.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder is made");
        folder
    }

    /// A small exe to push: a copy of the system's `whoami.exe`, which answers no `gate <version>`.
    fn stand_in() -> Vec<u8> {
        let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32");
        fs::read(system.join("whoami.exe")).expect("whoami.exe is read")
    }

    /// The script run here through local pwsh with `exe` fed, `LOCALAPPDATA` set to `local_app_data`: the
    /// `gate-push: ` lines it said and its end.
    fn run_script(exe: &[u8], local_app_data: &Path) -> Heard {
        let folder = std::env::current_dir().expect("the working folder is read");
        let pwsh = find_program("pwsh", &Variables::from_process(), &folder).expect("pwsh is on PATH");
        let line = push_line(&sha256_hex(exe));
        let args: Vec<OsString> = line.split(' ').skip(1).map(OsString::from).collect();
        let mut variables: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("LOCALAPPDATA"))
            .collect();
        variables.push(("LOCALAPPDATA".into(), local_app_data.into()));
        let input = STANDARD.encode(exe);
        called(&Call {
            program: &pwsh,
            args,
            input: input.as_bytes(),
            variables: &variables,
            bound: BOUND,
        })
    }

    #[test]
    fn an_uncaught_script_error_reaches_stderr_as_text() {
        let folder = std::env::current_dir().expect("the working folder is read");
        let pwsh = find_program("pwsh", &Variables::from_process(), &folder).expect("pwsh is on PATH");
        let line = encoded_line("throw 'boom'");
        let variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        let heard = called(&Call {
            program: &pwsh,
            args: line.split(' ').skip(1).map(OsString::from).collect(),
            input: b"",
            variables: &variables,
            bound: BOUND,
        });
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        let last = heard.last_error.expect("the error reaches stderr");
        // CLIXML puts its `#< CLIXML` mark on a line of its own and the error inside one `<Objs>` element after it.
        assert!(last.contains("boom") && !last.contains("CLIXML") && !last.contains("<Objs"), "{last}");
    }

    #[test]
    fn the_script_installs_from_stdin() {
        let local_app_data = scratch("installs");
        let exe = stand_in();
        let heard = run_script(&exe, &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert!(heard.marked.iter().any(|line| line.starts_with("gate-push: installed")), "{heard:?}");
        let installed = fs::read(local_app_data.join(r"gate\bin\gate.exe")).expect("the exe is installed");
        assert_eq!(sha256_hex(&installed), sha256_hex(&exe));
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn the_script_says_same_for_an_identical_exe() {
        let local_app_data = scratch("same");
        let exe = stand_in();
        let bin = local_app_data.join(r"gate\bin");
        fs::create_dir_all(&bin).expect("the bin folder is made");
        fs::write(bin.join("gate.exe"), &exe).expect("the same exe is installed");
        let heard = run_script(&exe, &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert!(heard.marked.iter().any(|line| line.starts_with("gate-push: same")), "{heard:?}");
        let names: Vec<OsString> = fs::read_dir(&bin)
            .expect("the bin folder is listed")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, ["gate.exe"], "nothing was replaced");
        let _ = fs::remove_dir_all(&local_app_data);
    }
}
