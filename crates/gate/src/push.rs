//! `gate push`: copies the installed `%LOCALAPPDATA%\gate\bin\gate.exe` to every builder `remote.json` names, one
//! after another. Each builder gets one ssh call whose line is a short PowerShell bootstrap, [`BOOTSTRAP`], sent
//! encoded; ssh's stdin carries the install script, [`SCRIPT`], as one line of base64, then the exe compressed with gzip,
//! as base64 text. The bootstrap reads the script's line and runs it in the bootstrap's scope; the script reads the
//! rest, decompresses it, checks the copy's SHA-256 and installs it on the builder as `tools/install.ps1` installs
//! here, with the old exe set aside, and says what it did on `gate-push: ` lines, which this side reads into the
//! `push: ` lines. Every builder is tried; one that failed makes the exit 1.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::GzEncoder;
use gate_args::PushRequest;
use gate_win32::{EnvironmentBlock, Job, LaunchError, PipedConsole, PipedFed, PipedLaunch, Running};
use regex::Regex;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

use crate::answer::FAILED_STATUS;
use crate::boxside::payload::escaped;
use crate::console::Console;
use crate::remote::git;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Wire, ssh_words};
use crate::resolve::Environment;
use crate::route::read_remote_json;

/// How long one builder's call may take before it is killed.
const BOUND: Duration = Duration::from_secs(300);
/// How long the installed exe's `--version` may take here.
const VERSION_BOUND: Duration = Duration::from_secs(30);
/// What starts each of the builder script's lines this side reads.
const MARK: &str = "gate-push: ";
/// What starts a builder script's `note` line.
const NOTE_MARK: &str = "gate-push: note ";
/// The most `gate-push: ` lines kept of one call; once they are full, a later note takes the last note's place and any
/// other line the last note's, else the last place, so the facts and the call's last line are kept ([`Kept::mark`]).
const MARKED_CAP: usize = 16;
/// The most bytes kept of one line of a call's output; the rest of a longer line is skipped.
const LINE_CAP: u64 = 64 * 1024;
/// What the builder's cmd runs: pwsh with [`BOOTSTRAP`], as UTF-16LE base64, its one other word.
const PWSH: &str = "pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand ";
/// The Win32 error of an open refused because another handle shares nothing.
const ERROR_SHARING_VIOLATION: i32 = 32;
/// The stand-in for the exe's SHA-256, as upper-case hex, in [`SCRIPT`].
const SHA_SLOT: &str = "@SHA@";

/// What the push line runs: it reads stdin's first line, [`SCRIPT`] as base64 of its UTF-8, and runs it dot-sourced,
/// so the script's variables, `$lock` among them, and its functions live in one scope, the bootstrap's. Both read
/// through `[Console]::In`, one buffered reader, so the script reads on from where the bootstrap's line ended.
const BOOTSTRAP: &str = "$l=[Console]::In.ReadLine();. ([ScriptBlock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($l))))";

/// The builder script, run by [`BOOTSTRAP`], as `tools/install.ps1` installs: on a pwsh older than 7.2, whose .NET
/// lacks what its file checks call, it says `failed` and touches nothing. It reads the rest of stdin, the exe as
/// base64 of its gzip, decompresses it, and refuses bytes whose SHA-256 is not the one this side put in for
/// [`SHA_SLOT`]. It then adds its file checks, a small C# type, and fails before anything is touched when that cannot
/// be done. In `%LOCALAPPDATA%\gate\bin`, made when missing and refused when it is a link, it first holds `push.lock`
/// open, unshared, to its end, so a second push to the builder at once says `failed` and touches nothing. `push.lock` is
/// opened without following a link and refused as a link when it is one, a second name of another file, a folder or a
/// junction; the plain file is left in place, since the open handle is the lock. It deletes each `gate.exe.<pid>.tmp`
/// of another pid that no process holds, saying `note` and why for one it cannot delete. With a `gate.exe` of the same
/// SHA-256 it says `same` and its version; else it writes `gate.exe.<pid>.tmp` afresh and reads its file id, the volume
/// serial number and the 128-bit id within the volume, from the handle that wrote it; the rename into place keeps the
/// id. It moves the old exe aside to `gate.exe.<yyyyMMddHHmmss>.<pid>.old` and the temp into place, never by force,
/// checks the installed copy's SHA-256 and that it answers `--version` with `gate <version>`, and on a failure, the
/// check's own included, moves the aside copy back, or removes the installed copy when there was no old exe, but only
/// while `gate.exe` is missing or still the push's own file, the one with the temp copy's file id. An id it cannot
/// read is tried three times, 300 ms apart, and a file gone at any try or 300 ms after the last counts as missing. Over
/// a missing one it moves never by force: one another process placed is left as it is. Then it says `replaced` and
/// the old version when it read one, and `installed` and the new one. Only an installed gate that answers lets it
/// delete the `gate.exe.*.old` copies no process holds, its own aside copy kept, with the same `note` for one it cannot
/// delete or a folder it cannot list; a push that ends with no such gate, a failed one included, keeps every aside
/// copy, and says `kept` and their number when there are any and it ends with `gate.exe` missing or one it could not
/// verify in place. A failure says `failed` and why, and exits 1. Each version read is bounded at 30 s, and a version
/// read that is killed at its bound is waited for up to 10 s more; one that cannot be stopped says `note` and why.
const SCRIPT: &str = r#"$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
function Say($k,$t){[Console]::Out.WriteLine("gate-push: $k $t")}
function Tally{try{$n=@(Get-ChildItem -LiteralPath $dir -File|?{$_.Name -like 'gate.exe.*.old'}).Count;if($n){Say kept $n}}
catch{Say note "could not list ${dir}: $($_.Exception.Message)"}}
function Fail($t){if($locked -and ($st.u -or !(Test-Path -LiteralPath $g))){Tally};Say failed $t;exit 1}
function Ver($p){try{$x=[Diagnostics.Process]::Start([Diagnostics.ProcessStartInfo]@{FileName=$p;Arguments='--version';RedirectStandardOutput=1;RedirectStandardError=1;UseShellExecute=0})}catch{return ''}
$o=$x.StandardOutput.ReadToEndAsync();$e=$x.StandardError.ReadToEndAsync()
if(!$x.WaitForExit(30000)){try{$x.Kill($true)}catch{Say note "could not stop the version read of ${p}: $($_.Exception.Message)"}
[void]$x.WaitForExit(10000);return ''}
if(($o.Result -split '\r?\n')[0] -notmatch '^gate\s+(.+?)\s*$'){return ''};$Matches[1]}
function Held($p){try{[IO.File]::Open($p,'Open','ReadWrite','None').Dispose();$false}catch{$_.Exception.GetBaseException().HResult -eq -2147024864}}
function Drop($p){Remove-Item -LiteralPath $p -Force -EA Ignore}
function Purge($p){try{Remove-Item -LiteralPath $p -Force}catch{if(!(Held $p)){Say note "could not delete ${p}: $($_.Exception.Message)"}}}
function Clean{try{$fs=@(Get-ChildItem -LiteralPath $dir -File|?{$_.Name -like 'gate.exe.*.old' -and (Join-Path $dir $_.Name) -ne $a})}
catch{Say note "could not delete the aside copies in ${dir}: $($_.Exception.Message)";return}
foreach($f in $fs){Purge $f.FullName}}
function Mine{foreach($i in 1..3){if(!(Test-Path -LiteralPath $g)){return 'missing'}
try{if([GateFile]::Of($g) -eq $st.i){return 'mine'};return 'foreign'}
catch{if($_.Exception.GetBaseException() -is [IO.FileNotFoundException]){return 'missing'};Start-Sleep -Milliseconds 300}}
if(Test-Path -LiteralPath $g){'foreign'}else{'missing'}}
function Placed($m){$st.u=1;$t="$m; another gate.exe was placed meanwhile and was left as it is"
if($a){$t+=", the previous gate.exe is at $a"};Fail $t}
function Back($m){$w=Mine;if($w -eq 'foreign'){Placed $m}
if(!$a){if($w -eq 'mine'){
try{Remove-Item -LiteralPath $g -Force}catch{Fail "$m; the copy at $g could not be removed: $($_.Exception.Message)"}}
Fail "$m; the copy was removed and no earlier gate.exe existed"}
try{if($w -eq 'mine'){Move-Item -LiteralPath $a -Destination $g -Force}else{Move-Item -LiteralPath $a -Destination $g}}
catch{if($w -eq 'missing' -and (Test-Path -LiteralPath $g)){Placed $m};Fail "$m; the previous gate.exe is at $a"}
$st.u=0;Fail "$m; the previous gate.exe is back in place"}
$pv=$PSVersionTable.PSVersion;if([version]"$($pv.Major).$($pv.Minor)" -lt [version]'7.2'){Fail "the builder needs pwsh 7.2 or later (it has $pv)"}
if(!$env:LOCALAPPDATA){Fail 'LOCALAPPDATA is not set'}
$dir=Join-Path $env:LOCALAPPDATA 'gate\bin';$g=Join-Path $dir 'gate.exe';$a=$null;$ci="could not install ${g}:";$st=@{u=0;i=''}
try{$u=[IO.MemoryStream]::new()
$z=[IO.MemoryStream]::new([Convert]::FromBase64String([Console]::In.ReadToEnd()))
[IO.Compression.GZipStream]::new($z,[IO.Compression.CompressionMode]::Decompress).CopyTo($u)
$b=$u.ToArray()}catch{Fail "the copy could not be read: $($_.Exception.Message)"}
if([Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($b)) -ne '@SHA@'){Fail 'the copy arrived damaged'}
try{Add-Type -TypeDefinition 'using System;using System.ComponentModel;using System.IO;using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class GateFile{const uint ReadWrite=0xC0000000,OpenAlways=4,NoFollow=0x200080,Folder=0x10,Link=0x400;
const int Busy=-2147024864,FileIdInfo=18;
[DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)]
static extern SafeFileHandle CreateFileW(string n,uint a,uint s,IntPtr p,uint d,uint f,IntPtr t);
[DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)]static extern uint GetFileAttributesW(string n);
[DllImport("kernel32.dll",SetLastError=true)]static extern bool GetFileInformationByHandle(SafeFileHandle h,[Out] uint[] i);
[DllImport("kernel32.dll",SetLastError=true)]static extern bool GetFileInformationByHandleEx(SafeFileHandle h,int c,[Out] byte[] i,uint n);
static IOException Error(string t){int e=Marshal.GetLastPInvokeError();
return new IOException(t+new Win32Exception(e).Message,unchecked((int)0x80070000)|e);}
public static string Id(SafeFileHandle h){var i=new byte[24];
if(!GetFileInformationByHandleEx(h,FileIdInfo,i,24)){throw Error("the file id could not be read: ");}return Convert.ToHexString(i);}
public static string Of(string p){using(var h=File.OpenHandle(p,FileMode.Open,FileAccess.Read,FileShare.ReadWrite|FileShare.Delete)){return Id(h);}}
public static FileStream OpenLock(string p){var h=CreateFileW(p,ReadWrite,0,IntPtr.Zero,OpenAlways,NoFollow,IntPtr.Zero);
if(h.IsInvalid){var x=Error("");h.Dispose();var a=GetFileAttributesW(p);
if(x.HResult!=Busy&&a!=uint.MaxValue&&(a&(Folder|Link))!=0){return null;}throw x;}
var i=new uint[13];if(!GetFileInformationByHandle(h,i)){var x=Error("");h.Dispose();throw x;}
if((i[0]&Link)!=0||i[10]!=1){h.Dispose();return null;}
return new FileStream(h,FileAccess.ReadWrite);}}'}catch{Fail "could not prepare the install: $($_.Exception.Message)"}
try{New-Item -ItemType Directory -Path $dir -Force|Out-Null}catch{Fail "could not create ${dir}: $($_.Exception.Message)"}
if((Get-Item -LiteralPath $dir).LinkType){Fail "$dir is a link"}
$pl=Join-Path $dir 'push.lock'
try{$lock=[GateFile]::OpenLock($pl)}catch{$x=$_.Exception.GetBaseException()
if($x.HResult -eq -2147024864){Fail 'another gate push is installing on this builder'};Fail "could not lock ${dir}: $($x.Message)"}
if($null -eq $lock){Fail "$pl is a link"}
$locked=1
foreach($f in Get-ChildItem -LiteralPath $dir -File|?{$_.Name -match '^gate\.exe\.(\d+)\.tmp$' -and $Matches[1] -ne $PID}){Purge $f.FullName}
if((Test-Path -LiteralPath $g) -and (Get-FileHash -LiteralPath $g).Hash -eq '@SHA@'){$v=Ver $g;if($v){Clean}else{Tally};Say same $v;exit 0}
$tmp="$g.$PID.tmp";Drop $tmp
try{$s=[IO.File]::Open($tmp,'CreateNew','Write','None');try{$s.Write($b,0,$b.Length);$st.i=[GateFile]::Id($s.SafeFileHandle)}finally{$s.Dispose()}}
catch{Drop $tmp;Fail "$ci $($_.Exception.Message)"}
$old=''
if(Test-Path -LiteralPath $g){$old=Ver $g;$a="$g.$(Get-Date -Format yyyyMMddHHmmss).$PID.old"
try{Move-Item -LiteralPath $g -Destination $a -Force}catch{Drop $tmp;Fail "$ci $($_.Exception.Message)"}}
try{Move-Item -LiteralPath $tmp -Destination $g}catch{$m=$_.Exception.Message;Drop $tmp
if(!$a){Fail "$ci $m; nothing was installed and no earlier gate.exe existed"};Back "$ci $m"}
$st.u=1
try{$h=(Get-FileHash -LiteralPath $g).Hash}catch{Back "$ci $($_.Exception.Message)"}
if($h -ne '@SHA@'){Back 'the installed copy does not match'}
$v=Ver $g;if(!$v){Back 'the installed gate.exe does not run'}
Clean
if($old){Say replaced $old}
Say installed $v
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
    let mut status = 0;
    for host in &prepared.builders {
        console.out(&format!("push: {host}: sending gate {}", prepared.version));
        let heard = match &ssh {
            Ok(program) => called(&Call {
                program,
                args: ssh_words(host, &prepared.line, Wire::Plain),
                input: prepared.input.as_bytes(),
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

/// What every builder is sent: the builders, the exe as its call is fed it, [`pushed_input`], the exe's version as
/// shown, and the ssh line.
struct Prepared {
    builders: Vec<String>,
    input: String,
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
    let bin = gate.join("bin");
    let exe = bin.join("gate.exe");
    // The lock tools/install.ps1 holds while it replaces and checks the exe, held for the read and the version only;
    // with no bin folder there is no exe, which the read says.
    let lock = if bin.is_dir() { Some(local_lock(&bin)?) } else { None };
    let bytes = installed(&exe)?;
    let version = shown(&local_version(&exe, variables)?);
    drop(lock);
    let line = push_line();
    let input = pushed_input(&sha256_hex(&bytes), &bytes);
    Ok(Prepared {
        builders,
        input,
        version,
        line,
    })
}

/// What a builder's call is fed for `exe`, whose SHA-256 is `sha`: [`script_line`] and a line break, then
/// [`gzipped`].
fn pushed_input(sha: &str, exe: &[u8]) -> String {
    format!("{}\n{}", script_line(sha), gzipped(exe))
}

/// [`SCRIPT`] with `sha` put in, as base64 of its UTF-8: one line, as [`BOOTSTRAP`] reads it.
fn script_line(sha: &str) -> String {
    STANDARD.encode(SCRIPT.replace(SHA_SLOT, sha))
}

/// `exe`'s gzip at the best level, as base64 text.
#[expect(clippy::expect_used, reason = "gzip written into a Vec has no write that can fail")]
fn gzipped(exe: &[u8]) -> String {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(exe).expect("writing into a Vec cannot fail");
    let gzip = encoder.finish().expect("finishing into a Vec cannot fail");
    STANDARD.encode(gzip)
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

/// `bin`'s `push.lock`, held open unshared until the file is dropped, as the builder script and `tools/install.ps1`
/// hold it; else the line saying an install holds it, it is a link, or why it could not be taken. It is opened without
/// following a link there and kept only when the opened handle is a plain file with one name, so a link, a hard link
/// included, is refused, and nothing can swap the file between the check and the open. One that cannot be opened is
/// looked at without following a link: a link or a folder there, which no file open takes, is refused as a link too.
fn local_lock(bin: &Path) -> Result<File, String> {
    let path = bin.join("push.lock");
    let linked = || format!("push: {} is a link; nothing was sent", path.display());
    let unlocked = |error: &io::Error| format!("push: could not lock {}: {error}; nothing was sent", bin.display());
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|error| {
            if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                "push: an install is in progress on this PC; nothing was sent".to_owned()
            } else if is_link_or_folder(&path) {
                linked()
            } else {
                unlocked(&error)
            }
        })?;
    match gate_win32::check_plain_file(&file) {
        Ok(()) => Ok(file),
        // A refusal carries no system error code; a handle that could not be inspected carries the system's.
        Err(error) if error.raw_os_error().is_none() => Err(linked()),
        Err(error) => Err(unlocked(&error)),
    }
}

/// Whether `path`, looked at without following a link, is a link or a folder; `false` when it cannot be looked at.
fn is_link_or_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_attributes() & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0)
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

/// The line each builder's cmd runs: pwsh with [`BOOTSTRAP`] encoded as UTF-16LE base64, a form cmd passes on with no
/// quoting.
fn push_line() -> String {
    encoded_line(BOOTSTRAP)
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

/// What a call said: how it ended, the first line of its standard output, up to [`MARKED_CAP`] of its `gate-push: `
/// lines as [`Kept::mark`] keeps them, its last standard error line that is not blank, and the first problem feeding or
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

/// What one stream of a call kept: its first line, its `gate-push: ` lines up to [`MARKED_CAP`] as [`Kept::mark`]
/// keeps them, its last line that is not blank, and the problem that ended the reading early.
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
            self.mark(line.clone());
        }
        if !line.trim().is_empty() {
            self.last = Some(line);
        }
    }

    /// Keeps the `gate-push: ` line `line` within [`MARKED_CAP`] lines: once they are full, a note takes the last note's
    /// place and is dropped when none is kept, and any other line takes the last note's place, else the last place, so
    /// notes never push out a fact.
    fn mark(&mut self, line: String) {
        if self.marked.len() >= MARKED_CAP {
            let last_note = self.marked.iter().rposition(|kept| kept.starts_with(NOTE_MARK));
            match last_note {
                Some(at) => {
                    self.marked.remove(at);
                }
                None if line.starts_with(NOTE_MARK) => return,
                None => {
                    self.marked.pop();
                }
            }
        }
        self.marked.push(line);
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
    /// The number of aside copies a push that ended with no verified gate kept.
    Kept(String),
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
        "kept" => Some(Fact::Kept(text)),
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
/// builder gave, whatever the push's end, the end, and after its stdout lines the aside copies it kept, whatever the
/// push's end.
fn said(host: &str, version: &str, heard: &Heard, bound: Duration) -> Said {
    let facts: Vec<Fact> = heard.marked.iter().filter_map(|line| fact(line)).collect();
    let notes = facts
        .iter()
        .filter_map(|fact| match fact {
            Fact::Note(text) => Some(format!("push: {host}: {}", shown(text))),
            _ => None,
        })
        .collect();
    let kept = facts.iter().filter_map(|fact| match fact {
        Fact::Kept(count) => Some(format!("push: {host}: kept {} aside copies: gate.exe is not verified", shown(count))),
        _ => None,
    });
    let (mut out, failure) = match ended(host, version, heard, &facts, bound) {
        Ok(out) => (out, None),
        Err(reason) => (Vec::new(), Some(format!("push: {host} failed: {reason}"))),
    };
    out.extend(kept);
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
    use flate2::read::GzDecoder;

    use super::{
        BOOTSTRAP, Call, End, Heard, LINE_CAP, MARKED_CAP, PWSH, SCRIPT, Said, called, encoded_line, gzipped, kept, push_line, pushed_input, said,
        script_line, sha256_hex, version_of,
    };
    use crate::environment::Variables;
    use crate::run::find_program;

    const SHA: &str = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
    const BOUND: Duration = Duration::from_secs(300);

    #[test]
    fn the_push_line_fits_the_cap() {
        let line = push_line();
        assert!(line.starts_with(PWSH), "{line}");
        assert!(
            line.len() <= 8127,
            "the push line is {} characters, longer than the 8127 cmd takes",
            line.len()
        );
    }

    #[test]
    fn the_push_line_decodes_to_the_bootstrap_and_the_input_to_the_script() {
        let line = push_line();
        let encoded = line.strip_prefix(PWSH).expect("the line starts with pwsh's words");
        assert!(!encoded.contains(' '), "the bootstrap is one word");
        let bytes = STANDARD.decode(encoded).expect("the bootstrap is base64");
        let (pairs, rest) = bytes.as_chunks::<2>();
        assert!(rest.is_empty(), "UTF-16 is whole pairs of bytes");
        let wide: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        assert_eq!(String::from_utf16(&wide).expect("the bootstrap is UTF-16"), BOOTSTRAP);

        let input = pushed_input(SHA, b"an exe");
        let (first, exe) = input.split_once('\n').expect("the script's line ends with a line break");
        assert_eq!(exe, gzipped(b"an exe"), "the exe follows the script's line");
        let script = String::from_utf8(STANDARD.decode(first).expect("the script's line is base64")).expect("the script is UTF-8");
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
        let folder = crate::test_support::temp_dir().join(format!("gate-push-{}-{test}", std::process::id()));
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
        run_script_fed(&pushed_input(&sha256_hex(exe), exe), local_app_data)
    }

    /// The push line run here through local pwsh, fed `input`, `LOCALAPPDATA` set to `local_app_data`: the
    /// `gate-push: ` lines it said and its end.
    fn run_script_fed(input: &str, local_app_data: &Path) -> Heard {
        let folder = std::env::current_dir().expect("the working folder is read");
        let pwsh = find_program("pwsh", &Variables::from_process(), &folder).expect("pwsh is on PATH");
        let line = push_line();
        let args: Vec<OsString> = line.split(' ').skip(1).map(OsString::from).collect();
        let mut variables: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("LOCALAPPDATA"))
            .collect();
        variables.push(("LOCALAPPDATA".into(), local_app_data.into()));
        called(&Call {
            program: &pwsh,
            args,
            input: input.as_bytes(),
            variables: &variables,
            bound: BOUND,
        })
    }

    #[test]
    fn the_pushed_input_is_the_gzip_of_the_exe_in_base64() {
        let exe = stand_in();
        let input = pushed_input(&sha256_hex(&exe), &exe);
        let (_, input) = input.split_once('\n').expect("the exe follows the script's line");
        let gzip = STANDARD.decode(input).expect("the input is base64");
        let mut unpacked = Vec::new();
        GzDecoder::new(gzip.as_slice()).read_to_end(&mut unpacked).expect("the input is gzip");
        assert!(unpacked == exe, "the input decompresses to the exe's own bytes");
        assert!(
            input.len() < STANDARD.encode(&exe).len(),
            "the input, {} bytes, is smaller than the exe's base64",
            input.len()
        );
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
        let heard = run_script_answering("", &exe, &exe, &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert_eq!(heard.marked, [format!("gate-push: installed {ANSWERED}")], "{heard:?}");
        let installed = fs::read(local_app_data.join(r"gate\bin\gate.exe")).expect("the exe is installed");
        assert_eq!(sha256_hex(&installed), sha256_hex(&exe));
        let _ = fs::remove_dir_all(&local_app_data);
    }

    /// The names in `folder`, in name order.
    fn listed(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder is listed")
            .map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The push line run here as [`run_script`] runs it, with `prelude` put before the script on its line, so a case can
    /// stand in for one of the commands it calls: a function takes precedence over a cmdlet of the same name.
    fn run_script_after(prelude: &str, exe: &[u8], local_app_data: &Path) -> Heard {
        run_lines(prelude, &SCRIPT.replace("@SHA@", &sha256_hex(exe)), exe, local_app_data)
    }

    /// [`run_script_after`] with [`answering`]'s script, so a gate.exe holding `answers`'s bytes answers its version.
    fn run_script_answering(prelude: &str, answers: &[u8], exe: &[u8], local_app_data: &Path) -> Heard {
        run_lines(prelude, &answering(&sha256_hex(exe), answers), exe, local_app_data)
    }

    /// `script` after `prelude` on the script's line, run as the push line runs it, fed `exe`.
    fn run_lines(prelude: &str, script: &str, exe: &[u8], local_app_data: &Path) -> Heard {
        let script = format!("{prelude}\n{script}");
        run_script_fed(&format!("{}\n{}", STANDARD.encode(script), gzipped(exe)), local_app_data)
    }

    /// What the script's version read starts with.
    const VER: &str = "function Ver($p){";
    /// The version [`answering`]'s stand-in says.
    const ANSWERED: &str = "0.0.0-stand-in";

    /// [`SCRIPT`] with `sha` put in and its version read standing in for a gate's: a file holding `answers`'s bytes says
    /// `gate` [`ANSWERED`], as a gate would, since no exe the system has answers `--version` so; any other file is run.
    fn answering(sha: &str, answers: &[u8]) -> String {
        let script = SCRIPT.replace("@SHA@", sha);
        assert!(script.contains(VER), "the script reads versions with Ver");
        let stand_in = format!(
            "{VER}$h=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([IO.File]::ReadAllBytes($p)))\n\
             if($h -eq '{}'){{return '{ANSWERED}'}}\n",
            sha256_hex(answers)
        );
        script.replacen(VER, &stand_in, 1)
    }

    /// An older gate a case's builder holds.
    const OLDER: &[u8] = b"an older gate";
    /// An aside copy an earlier push left, named as the script names one, and its bytes.
    const ASIDE: &str = "gate.exe.20200101000000.4.old";
    const ASIDE_BYTES: &[u8] = b"an older aside copy";
    /// A second aside copy an earlier push left.
    const OTHER_ASIDE: &str = "gate.exe.20200102000000.5.old";
    /// What another process writes over gate.exe.
    const FOREIGN: &[u8] = &[1, 2, 3];
    /// What another process does to gate.exe, `$Destination` in [`around_the_move_in`]: it puts a new file of its own,
    /// [`FOREIGN`], in the copy's place.
    const REPLACES: &str = "[IO.File]::Delete($Destination);[IO.File]::WriteAllBytes($Destination,[byte[]](1,2,3))";

    /// A scratch LOCALAPPDATA for `test` whose install folder holds `files`, each a name and its bytes.
    fn builder_with(test: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let local_app_data = scratch(test);
        let bin = local_app_data.join(r"gate\bin");
        fs::create_dir_all(&bin).expect("the bin folder is made");
        for (name, bytes) in files {
            fs::write(bin.join(name), bytes).expect("a builder file is written");
        }
        local_app_data
    }

    /// The aside copies in `bin`, in name order.
    fn asides(bin: &Path) -> Vec<String> {
        listed(bin)
            .into_iter()
            .filter(|name| Path::new(name).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("old")))
            .collect()
    }

    #[test]
    fn a_first_install_whose_copy_fails_its_check_leaves_no_gate() {
        let local_app_data = scratch("first-mismatch");
        let heard = run_script_after("function Get-FileHash{@{Hash='MISMATCH'}}", &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            ["gate-push: failed the installed copy does not match; the copy was removed and no earlier gate.exe existed"],
            "{heard:?}"
        );
        assert_eq!(
            listed(&local_app_data.join(r"gate\bin")),
            ["push.lock"],
            "the copy that failed its check was removed"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_first_install_whose_failed_copy_cannot_be_removed_says_where_it_is() {
        let local_app_data = scratch("first-unremovable");
        let gate = local_app_data.join(r"gate\bin\gate.exe");
        // Only the removal of the installed copy fails; every other Remove-Item runs as itself.
        let prelude = "function Get-FileHash{@{Hash='MISMATCH'}}\n\
                       function Remove-Item{if($args -contains $g){throw [IO.IOException]::new('the copy is held')}\n\
                       Microsoft.PowerShell.Management\\Remove-Item @args}";
        let heard = run_script_after(prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [format!(
                "gate-push: failed the installed copy does not match; the copy at {} could not be removed: the copy is held",
                gate.display()
            )],
            "{heard:?}"
        );
        assert!(gate.exists(), "the copy that could not be removed is where the line says");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn the_lock_is_held_until_the_install_is_checked() {
        let local_app_data = scratch("lock-held");
        // The check after the move is the install's last step; a collection first ends a lock stream no variable keeps.
        let prelude = "function Get-FileHash($LiteralPath){[GC]::Collect();[GC]::WaitForPendingFinalizers()\n\
                       Say note \"lock held: $(Held (Join-Path $dir 'push.lock'))\"\n\
                       Microsoft.PowerShell.Utility\\Get-FileHash -LiteralPath $LiteralPath}";
        let heard = run_script_answering(prelude, &stand_in(), &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert_eq!(
            heard.marked.first().map(String::as_str),
            Some("gate-push: note lock held: True"),
            "{heard:?}"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_check_that_cannot_read_the_installed_copy_puts_the_old_gate_back() {
        let local_app_data = scratch("check-throws");
        let bin = local_app_data.join(r"gate\bin");
        fs::create_dir_all(&bin).expect("the bin folder is made");
        let gate = bin.join("gate.exe");
        fs::write(&gate, b"an older gate").expect("an older gate is installed");
        // The first hash is the same-exe check on the older gate; the second, after the move, fails as a held file does.
        let prelude = "$global:hashes=0;function Get-FileHash($LiteralPath){$global:hashes++\n\
                       if($global:hashes -eq 2){throw [IO.IOException]::new('the file is held')}\n\
                       Microsoft.PowerShell.Utility\\Get-FileHash -LiteralPath $LiteralPath}";
        let heard = run_script_after(prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [format!(
                "gate-push: failed could not install {}: the file is held; the previous gate.exe is back in place",
                gate.display()
            )],
            "{heard:?}"
        );
        assert_eq!(fs::read(&gate).expect("a gate is installed"), b"an older gate", "the older gate is back");
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "no aside or temp copy is left");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    /// The `failed` line the script said, and that it exited 1 with nothing installed.
    fn failed_line(heard: &Heard, local_app_data: &Path) -> String {
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert!(!local_app_data.join(r"gate\bin\gate.exe").exists(), "nothing was installed: {heard:?}");
        let line = heard.marked.iter().find(|line| line.starts_with("gate-push: failed ")).cloned();
        assert!(line.is_some(), "a failed line: {heard:?}");
        line.unwrap_or_default()
    }

    #[test]
    fn a_copy_that_is_not_gzip_fails_as_could_not_be_read() {
        let local_app_data = scratch("not-gzip");
        let exe = stand_in();
        let input = format!("{}\n{}", script_line(&sha256_hex(&exe)), STANDARD.encode(&exe));
        let heard = run_script_fed(&input, &local_app_data);
        let line = failed_line(&heard, &local_app_data);
        assert!(line.starts_with("gate-push: failed the copy could not be read: "), "{line}");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_gzip_of_other_bytes_fails_as_damaged() {
        let local_app_data = scratch("other-gzip");
        let exe = stand_in();
        let heard = run_script_fed(&pushed_input(&sha256_hex(&exe), b"other bytes"), &local_app_data);
        assert_eq!(failed_line(&heard, &local_app_data), "gate-push: failed the copy arrived damaged");
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
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "nothing was replaced");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    /// A prelude whose `Move-Item` throws `denied` when `condition` holds, and else moves as itself.
    fn move_denied_when(condition: &str) -> String {
        format!(
            "function Move-Item($LiteralPath,$Destination,[switch]$Force){{if({condition}){{throw [IO.IOException]::new('denied')}}\n\
             Microsoft.PowerShell.Management\\Move-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force}}"
        )
    }

    /// A prelude whose `Get-FileHash` runs `body` on its `nth` call, and else hashes as itself.
    fn nth_hash(nth: u32, body: &str) -> String {
        format!(
            "$global:hashes=0;function Get-FileHash($LiteralPath){{$global:hashes++\n\
             if($global:hashes -eq {nth}){{{body}}}\n\
             Microsoft.PowerShell.Utility\\Get-FileHash -LiteralPath $LiteralPath}}"
        )
    }

    #[test]
    fn a_push_that_installs_nothing_keeps_the_aside_copies() {
        let local_app_data = builder_with("installs-nothing", &[("gate.exe", OLDER), (ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        // Only the temp copy's move into place fails; the old gate's moves aside and back run as themselves.
        let heard = run_script_after(&move_denied_when("$LiteralPath -eq $tmp"), &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [format!(
                "gate-push: failed could not install {}: denied; the previous gate.exe is back in place",
                bin.join("gate.exe").display()
            )],
            "the older gate is back, so the aside copy is kept silently: {heard:?}"
        );
        assert_eq!(listed(&bin), ["gate.exe", ASIDE, "push.lock"], "the older aside copy is kept");
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is back"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn an_install_whose_version_cannot_be_read_fails_and_puts_the_old_gate_back() {
        let local_app_data = builder_with("unrunnable-over-old", &[("gate.exe", OLDER), (ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        // The older gate answers its version; the stand-in pushed over it does not.
        let heard = run_script_answering("", OLDER, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            ["gate-push: failed the installed gate.exe does not run; the previous gate.exe is back in place"],
            "the older gate is back, so the aside copy is kept silently: {heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is back"
        );
        assert_eq!(listed(&bin), ["gate.exe", ASIDE, "push.lock"], "the aside copies are kept");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn an_install_whose_version_cannot_be_read_keeps_every_aside_copy() {
        let local_app_data = builder_with("unrunnable-asides", &[(ASIDE, ASIDE_BYTES), (OTHER_ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        let heard = run_script(&stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [
                "gate-push: kept 2",
                "gate-push: failed the installed gate.exe does not run; the copy was removed and no earlier gate.exe existed"
            ],
            "{heard:?}"
        );
        assert_eq!(listed(&bin), [ASIDE, OTHER_ASIDE, "push.lock"], "every aside copy is kept");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_same_exe_that_does_not_run_keeps_the_aside_copies() {
        let exe = stand_in();
        let local_app_data = builder_with("same-unrunnable", &[("gate.exe", exe.as_slice()), (ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        let heard = run_script(&exe, &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert_eq!(heard.marked, ["gate-push: kept 1", "gate-push: same "], "{heard:?}");
        assert_eq!(listed(&bin), ["gate.exe", ASIDE, "push.lock"], "the aside copy is kept");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_missing_gate_exe_keeps_the_aside_copy() {
        let local_app_data = builder_with("missing-gate", &[(ASIDE, OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        let heard = run_script_after(&move_denied_when("$true"), &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [
                "gate-push: kept 1".to_owned(),
                format!(
                    "gate-push: failed could not install {}: denied; nothing was installed and no earlier gate.exe existed",
                    bin.join("gate.exe").display()
                )
            ],
            "{heard:?}"
        );
        assert_eq!(listed(&bin), [ASIDE, "push.lock"], "the aside copy, the only gate left, is kept");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_failed_move_back_leaves_the_aside_copy_for_the_next_push() {
        let local_app_data = builder_with("failed-move-back", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // The check after the move fails, and so does the move of the aside copy back.
        let prelude = format!(
            "{}\n{}",
            nth_hash(2, "return @{Hash='MISMATCH'}"),
            move_denied_when("$LiteralPath -like '*.old'")
        );
        let first = run_script_after(&prelude, &stand_in(), &local_app_data);
        let kept = asides(&bin);
        assert_eq!(kept.len(), 1, "{first:?}");
        assert_eq!(
            first.marked,
            [
                "gate-push: kept 1".to_owned(),
                format!(
                    "gate-push: failed the installed copy does not match; the previous gate.exe is at {}",
                    bin.join(&kept[0]).display()
                )
            ],
            "{first:?}"
        );

        let mut other = stand_in();
        other.push(0);
        let second = run_script(&other, &local_app_data);

        assert_eq!(second.end, End::Exited(1), "the second push cannot verify its install: {second:?}");
        assert_eq!(asides(&bin), kept, "the first push's aside copy is kept: {second:?}");
        assert_eq!(fs::read(bin.join(&kept[0])).expect("the aside copy is read"), OLDER);
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn back_does_not_remove_a_gate_another_process_placed() {
        let local_app_data = scratch("back-first-foreign");
        let bin = local_app_data.join(r"gate\bin");
        // Another process puts its own gate.exe in place just after the move in, so the check reads its bytes.
        let heard = run_script_after(&around_the_move_in("", REPLACES), &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            ["gate-push: failed the installed copy does not match; another gate.exe was placed meanwhile and was left as it is"],
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is there"),
            FOREIGN,
            "the other gate is left"
        );
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"]);
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn back_does_not_overwrite_a_gate_another_process_placed_over_the_old_one() {
        let local_app_data = builder_with("back-old-foreign", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // Another process puts its own gate.exe in place just after the move in, so the check reads its bytes.
        let heard = run_script_after(&around_the_move_in("", REPLACES), &stand_in(), &local_app_data);
        let kept = asides(&bin);
        assert_eq!(kept.len(), 1, "{heard:?}");
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [
                "gate-push: kept 1".to_owned(),
                format!(
                    "gate-push: failed the installed copy does not match; another gate.exe was placed meanwhile and was left as it is, \
                     the previous gate.exe is at {}",
                    bin.join(&kept[0]).display()
                )
            ],
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is there"),
            FOREIGN,
            "the other gate is left"
        );
        assert_eq!(
            fs::read(bin.join(&kept[0])).expect("the aside copy is read"),
            OLDER,
            "the older gate is kept aside"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_first_install_whose_move_fails_leaves_a_gate_placed_meanwhile() {
        let local_app_data = scratch("first-move-foreign");
        let bin = local_app_data.join(r"gate\bin");
        // Another process takes the name gate.exe, so the move of the temp copy into place fails.
        let prelude = "function Move-Item($LiteralPath,$Destination,[switch]$Force){[IO.File]::WriteAllBytes($Destination,[byte[]](1,2,3))\n\
                       throw [IO.IOException]::new('the name is taken')}";
        let heard = run_script_after(prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [format!(
                "gate-push: failed could not install {}: the name is taken; nothing was installed and no earlier gate.exe existed",
                bin.join("gate.exe").display()
            )],
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is there"),
            FOREIGN,
            "the other gate is left"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_move_back_does_not_overwrite_a_gate_placed_while_gate_exe_was_missing() {
        let local_app_data = builder_with("move-back-foreign", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // The temp copy's move into place fails, so gate.exe is missing; another process writes one as the aside copy
        // is moved back.
        let prelude = "function Move-Item($LiteralPath,$Destination,[switch]$Force){\n\
                       if($LiteralPath -eq $tmp){throw [IO.IOException]::new('denied')}\n\
                       if($LiteralPath -like '*.old'){[IO.File]::WriteAllBytes($Destination,[byte[]](1,2,3))}\n\
                       Microsoft.PowerShell.Management\\Move-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force}";
        let heard = run_script_after(prelude, &stand_in(), &local_app_data);
        let kept = asides(&bin);
        assert_eq!(kept.len(), 1, "{heard:?}");
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [
                "gate-push: kept 1".to_owned(),
                format!(
                    "gate-push: failed could not install {}: denied; another gate.exe was placed meanwhile and was left as it is, \
                     the previous gate.exe is at {}",
                    bin.join("gate.exe").display(),
                    bin.join(&kept[0]).display()
                )
            ],
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is there"),
            FOREIGN,
            "the other gate is left"
        );
        assert_eq!(
            fs::read(bin.join(&kept[0])).expect("the aside copy is read"),
            OLDER,
            "the older gate is kept aside"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    /// A prelude whose `Move-Item` runs `before` and `after` around the temp copy's move into place, and else moves as
    /// itself; `$Destination` is gate.exe in both.
    fn around_the_move_in(before: &str, after: &str) -> String {
        format!(
            "function Move-Item($LiteralPath,$Destination,[switch]$Force){{\n\
             if($LiteralPath -eq $tmp){{{before}}}\n\
             Microsoft.PowerShell.Management\\Move-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force\n\
             if($LiteralPath -eq $tmp){{{after}}}}}"
        )
    }

    #[test]
    fn an_installed_copy_that_does_not_match_puts_the_old_gate_back() {
        let local_app_data = builder_with("real-mismatch", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // The bytes in place really differ from the pushed copy's, as a copy damaged on the disk would.
        let prelude = around_the_move_in("", "[IO.File]::WriteAllBytes($Destination,[byte[]](9,9,9))");
        let heard = run_script_after(&prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            ["gate-push: failed the installed copy does not match; the previous gate.exe is back in place"],
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is back"
        );
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "no aside or temp copy is left");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_gate_placed_before_the_move_in_is_left_as_it_is() {
        let local_app_data = builder_with("placed-before-move-in", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // Another process writes gate.exe after the old gate was moved aside and before the temp copy's move.
        let prelude = around_the_move_in("[IO.File]::WriteAllBytes($Destination,[byte[]](1,2,3))", "");
        let heard = run_script_after(&prelude, &stand_in(), &local_app_data);
        let kept = asides(&bin);
        assert_eq!(kept.len(), 1, "{heard:?}");
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(heard.marked.len(), 2, "{heard:?}");
        assert_eq!(heard.marked[0], "gate-push: kept 1", "{heard:?}");
        let failed = &heard.marked[1];
        let start = format!("gate-push: failed could not install {}: ", bin.join("gate.exe").display());
        let end = format!(
            "; another gate.exe was placed meanwhile and was left as it is, the previous gate.exe is at {}",
            bin.join(&kept[0]).display()
        );
        assert!(failed.starts_with(&start) && failed.ends_with(&end), "{heard:?}");
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is there"),
            FOREIGN,
            "the other gate is left"
        );
        assert_eq!(
            fs::read(bin.join(&kept[0])).expect("the aside copy is read"),
            OLDER,
            "the older gate is kept aside"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_gate_exe_that_goes_while_it_is_checked_counts_as_missing() {
        let local_app_data = builder_with("goes-while-checked", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // The installed copy is held unshared just after the move in, so neither its hash nor its identity can be read;
        // between two reads of its identity the holder lets it go and deletes it.
        let prelude = format!(
            "{}\nfunction Start-Sleep{{if($global:held){{$global:held.Dispose();$global:held=$null;[IO.File]::Delete($g)}}}}",
            around_the_move_in("", "$global:held=[IO.File]::Open($Destination,'Open','Read','None')")
        );
        let heard = run_script_after(&prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(heard.marked.len(), 1, "{heard:?}");
        let failed = &heard.marked[0];
        let start = format!("gate-push: failed could not install {}: ", bin.join("gate.exe").display());
        assert!(
            failed.starts_with(&start) && failed.ends_with("; the previous gate.exe is back in place"),
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is back"
        );
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "no aside or temp copy is left");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_gate_exe_that_goes_after_its_last_identity_read_counts_as_missing() {
        let local_app_data = builder_with("goes-after-last-read", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        // The installed copy is held unshared just after the move in, so no read of its identity succeeds; the holder
        // lets it go and deletes it only in the wait after the last read.
        let prelude = format!(
            "{}\n$global:waits=0;function Start-Sleep{{$global:waits++\n\
             if($global:waits -eq 3){{$global:held.Dispose();[IO.File]::Delete($g)}}}}",
            around_the_move_in("", "$global:held=[IO.File]::Open($Destination,'Open','Read','None')")
        );
        let heard = run_script_after(&prelude, &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(heard.marked.len(), 1, "{heard:?}");
        let failed = &heard.marked[0];
        let start = format!("gate-push: failed could not install {}: ", bin.join("gate.exe").display());
        assert!(
            failed.starts_with(&start) && failed.ends_with("; the previous gate.exe is back in place"),
            "{heard:?}"
        );
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is back"
        );
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "no aside or temp copy is left");
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_type_that_cannot_be_added_fails_the_push_before_gate_exe_is_touched() {
        let local_app_data = builder_with("no-compiler", &[("gate.exe", OLDER), (ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        // As where pwsh runs in constrained language or finds no compiler.
        let heard = run_script_after("function Add-Type{throw 'no compiler'}", &stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            ["gate-push: failed could not prepare the install: no compiler"],
            "{heard:?}"
        );
        assert_eq!(listed(&bin), ["gate.exe", ASIDE], "nothing was locked, cleaned up or replaced");
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is left"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_builder_push_lock_that_is_a_hard_link_is_refused() {
        let local_app_data = builder_with("lock-hard-link", &[("gate.exe", OLDER)]);
        let bin = local_app_data.join(r"gate\bin");
        let other = local_app_data.join("other.txt");
        fs::write(&other, b"another file").expect("the other file is written");
        fs::hard_link(&other, bin.join("push.lock")).expect("push.lock is made a second name of the other file");
        let heard = run_script(&stand_in(), &local_app_data);
        assert_eq!(heard.end, End::Exited(1), "{heard:?}");
        assert_eq!(
            heard.marked,
            [format!("gate-push: failed {} is a link", bin.join("push.lock").display())],
            "{heard:?}"
        );
        assert_eq!(listed(&bin), ["gate.exe", "push.lock"], "nothing was installed");
        assert_eq!(
            fs::read(bin.join("gate.exe")).expect("a gate is installed"),
            OLDER,
            "the older gate is left"
        );
        assert_eq!(
            fs::read(&other).expect("the other file is read"),
            b"another file",
            "the other file is untouched"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn a_cleanup_that_cannot_list_the_folder_says_so_and_still_installs() {
        let exe = stand_in();
        let local_app_data = builder_with("clean-unlisted", &[(ASIDE, ASIDE_BYTES)]);
        let bin = local_app_data.join(r"gate\bin");
        // The first listing is the stale temp copies'; the second, the cleanup's after the verified install, fails.
        let prelude = "$global:lists=0;function Get-ChildItem{$global:lists++\n\
                       if($global:lists -eq 2){throw [IO.IOException]::new('the folder is busy')}\n\
                       Microsoft.PowerShell.Management\\Get-ChildItem @args}";
        let heard = run_script_answering(prelude, &exe, &exe, &local_app_data);
        assert_eq!(heard.end, End::Exited(0), "{heard:?}");
        assert_eq!(
            heard.marked,
            [
                format!(
                    "gate-push: note could not delete the aside copies in {}: the folder is busy",
                    bin.display()
                ),
                format!("gate-push: installed {ANSWERED}")
            ],
            "{heard:?}"
        );
        let _ = fs::remove_dir_all(&local_app_data);
    }

    #[test]
    fn more_notes_than_the_cap_never_push_out_the_kept_and_failed_facts() {
        let mut lines = "gate-push: note could not delete x\n".repeat(MARKED_CAP + 4);
        lines.push_str("gate-push: kept 2\ngate-push: failed the installed gate.exe does not run\n");
        let marked = kept(Cursor::new(lines)).marked;
        assert_eq!(marked.len(), MARKED_CAP, "{marked:?}");
        assert_eq!(
            marked.get(MARKED_CAP - 2..),
            Some(
                [
                    "gate-push: kept 2".to_owned(),
                    "gate-push: failed the installed gate.exe does not run".to_owned()
                ]
                .as_slice()
            ),
            "{marked:?}"
        );
        let heard = Heard {
            marked,
            ..heard(1, &[], None)
        };
        let said = said("b1", "0.2.0", &heard, BOUND);
        assert_eq!(said.out, ["push: b1: kept 2 aside copies: gate.exe is not verified"], "{said:?}");
        assert_eq!(said.failure.as_deref(), Some("push: b1 failed: the installed gate.exe does not run"));
    }

    #[test]
    fn the_kept_fact_is_said_on_stdout_and_never_fails_a_push() {
        let kept = "push: b1: kept 2 aside copies: gate.exe is not verified";
        assert_eq!(
            said("b1", "0.2.0", &heard(0, &["gate-push: kept 2", "gate-push: same "], None), BOUND),
            out(&["push: b1: gate 0.2.0 already installed", kept])
        );
        assert_eq!(
            said(
                "b1",
                "0.2.0",
                &heard(1, &["gate-push: kept 2", "gate-push: failed the installed gate.exe does not run"], None),
                BOUND
            ),
            Said {
                out: vec![kept.to_owned()],
                ..failure("push: b1 failed: the installed gate.exe does not run")
            }
        );
    }
}
