//! cmd.exe quoting (C5) for a `.cmd` or `.bat` target, its command line, and the words it refuses.

use gate_args::{BatchRefusal, batch_command_line, batch_refusal, is_batch_target, quote_batch_arg, quote_batch_arg_str};

const DYNAMIC: [&str; 10] = [
    "CD",
    "DATE",
    "TIME",
    "RANDOM",
    "ERRORLEVEL",
    "CMDCMDLINE",
    "CMDEXTVERSION",
    "HIGHESTNUMANODENUMBER",
    "__CD__",
    "__APPDIR__",
];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// The command's environment in these tests: `PATH`, `B` and `VAR` are set, names compared case-insensitively.
fn is_set(name: &str) -> bool {
    ["PATH", "B", "VAR"].iter().any(|set| set.eq_ignore_ascii_case(name))
}

/// The word a batch target refuses among `run.cmd` and `word`, if any.
fn refused(word: &str) -> Option<BatchRefusal> {
    batch_refusal(&[wide("run.cmd"), wide(word)], is_set)
}

/// Each word and how `Format-BatchArgument` in the PowerShell gate writes it.
#[test]
fn quote_batch_arg_writes_each_word_as_format_batch_argument_does() {
    let mut rows: Vec<(String, String)> = "&|<>^(),;=".chars().map(|meta| (format!("a{meta}b"), format!("\"a{meta}b\""))).collect();
    for (word, written) in [
        ("", r#""""#),
        (r#""""#, r#""""""""#),
        ("--mode=fast", r#""--mode=fast""#),
        ("a b", r#""a b""#),
        (r#"x"y"#, r#""x""y""#),
        (r#"a\"b"#, r#""a\\""b""#),
        (r"c:\dir with space\", r#""c:\dir with space\\""#),
        ("!x", "!x"),
        ("abc", "abc"),
    ] {
        rows.push((word.to_owned(), written.to_owned()));
    }
    for (word, written) in rows {
        assert_eq!(quote_batch_arg_str(&word), written, "word {word:?}");
        assert_eq!(quote_batch_arg(&wide(&word)), wide(&written), "word {word:?} as UTF-16");
    }
}

#[test]
fn batch_command_line_runs_the_words_through_cmd() {
    let words = [wide(r"C:\tools dir\run.cmd"), wide("a&b"), wide("plain"), wide("")];
    assert_eq!(
        batch_command_line(&wide(r"C:\Windows\System32\cmd.exe"), &words),
        wide(r#"C:\Windows\System32\cmd.exe /d /v:off /s /c ""C:\tools dir\run.cmd" "a&b" plain """"#)
    );
    assert_eq!(
        batch_command_line(&wide(r"C:\Program Files\cmd.exe"), &[wide("run.cmd")]),
        wide(r#""C:\Program Files\cmd.exe" /d /v:off /s /c "run.cmd""#),
        "the shell is quoted as for the C runtime"
    );
}

#[test]
fn batch_refusal_refuses_what_cmd_would_split_or_expand() {
    let mut words: Vec<String> = [
        "%A%B%",
        "%Path%",
        "x%VAR%y",
        "%CD:~0,3%",
        "%PATH:a=b%",
        "%VAR:~1%",
        "%time:~0,5%",
        "a\rb",
        "a\nb",
    ]
    .map(str::to_owned)
    .into();
    words.extend(DYNAMIC.iter().map(|name| format!("%{name}%")));
    words.extend(DYNAMIC.iter().map(|name| format!("%{}%", name.to_lowercase())));
    for word in words {
        assert_eq!(refused(&word), Some(BatchRefusal { word: wide(&word) }), "word {word:?}");
    }
}

/// cmd's hidden `=`-named variables (`=ExitCode`, `=C:`) are never listed as set, and a NUL ends the line cmd reads.
#[test]
fn batch_refusal_refuses_equals_names_and_nul_with_nothing_set() {
    for word in ["%=ExitCode%", "%=C:%", "%=ExitCodeAscii%", "x%=ExitCode:~0,2%", "a\0b"] {
        assert_eq!(
            batch_refusal(&[wide("run.cmd"), wide(word)], |_| false),
            Some(BatchRefusal { word: wide(word) }),
            "word {word:?}"
        );
    }
}

/// cmd.exe sets `COMSPEC`, `PATHEXT` and `PROMPT` for itself when the block it is started with lacks them, so they
/// expand whatever the command's environment holds.
#[test]
fn batch_refusal_refuses_the_names_cmd_sets_for_itself_with_nothing_set() {
    for word in ["%PROMPT%", "%prompt%", "%COMSPEC%", "%PathExt%", "x%PROMPT:~0,1%", "%COMSPEC:a=b%"] {
        assert_eq!(
            batch_refusal(&[wide("run.cmd"), wide(word)], |_| false),
            Some(BatchRefusal { word: wide(word) }),
            "word {word:?}"
        );
    }
}

/// The whole name between the `%`s is looked up as well as the part before its first `:`.
#[test]
fn batch_refusal_looks_up_the_whole_name() {
    let is_set = |name: &str| name.eq_ignore_ascii_case("A:B");
    assert_eq!(batch_refusal(&[wide("%a:b%")], is_set), Some(BatchRefusal { word: wide("%a:b%") }));
}

#[test]
fn is_batch_target_reads_the_extension_as_windows_does() {
    for (path, batch) in [
        ("run.cmd", true),
        ("run.CMD", true),
        (r"C:\tools\run.Bat", true),
        ("run.bat.", true),
        ("run.cmd ", true),
        ("run.cmd. .", true),
        ("run.cmdx", false),
        ("run", false),
        ("run.exe", false),
        (r"C:\dir.cmd\run", false),
    ] {
        assert_eq!(is_batch_target(path), batch, "path {path:?}");
    }
}

#[test]
fn batch_refusal_passes_a_lone_percent_and_unset_names() {
    for word in ["%", "100%", "%%", "%UNSET%", "%UNSET:~0,1%", "%UNSET:a=b%", "%A%B", "%CD", "plain", "!B!"] {
        assert_eq!(refused(word), None, "word {word:?}");
    }
}

#[test]
fn batch_refusal_names_the_first_refused_word_the_program_included() {
    let words = [wide("run.cmd"), wide("ok"), wide("%CD%"), wide("%PATH%")];
    assert_eq!(batch_refusal(&words, is_set), Some(BatchRefusal { word: wide("%CD%") }));
    let program = [wide(r"C:\%B%\run.cmd"), wide("ok")];
    assert_eq!(
        batch_refusal(&program, is_set),
        Some(BatchRefusal {
            word: wide(r"C:\%B%\run.cmd")
        })
    );
}

#[test]
fn batch_refusal_line() {
    let refusal = refused("%CD%").expect("%CD% is refused");
    assert_eq!(
        refusal.to_string(),
        "gate: a .cmd or .bat command cannot take a word holding a line break or %NAME% of a set variable, which cmd.exe \
         would split or expand: %CD%"
    );
}
