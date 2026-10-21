use super::{
    AUTH_SCHEMES, COMMAND_CHARS, GLUED_PASSWORD_PROGRAMS, IMAGE_CHARS, KEY_PROGRAMS, SECRET_FLAGS, TOKEN_PREFIXES, is_unsafe, journal_image,
    journal_text, sanitize,
};

#[test]
fn a_url_s_password_is_hidden_and_its_user_kept() {
    assert_eq!(
        journal_text("git", "git clone https://alice:hunter2@github.com/o/r.git"),
        "git clone https://alice:***@github.com/o/r.git"
    );
}

#[test]
fn a_url_s_lone_token_is_hidden_and_a_url_without_credentials_kept() {
    assert_eq!(
        journal_text("git", "git push https://t0ken@host/r.git https://host/r.git"),
        "git push https://***@host/r.git https://host/r.git"
    );
}

#[test]
fn the_argument_after_each_secret_flag_is_hidden() {
    for flag in SECRET_FLAGS {
        assert_eq!(
            journal_text("tool", &format!("tool {flag} s3cret --verbose")),
            format!("tool {flag} *** --verbose"),
            "{flag}"
        );
    }
    assert_eq!(journal_text("tool", "tool --TOKEN s3cret"), "tool --TOKEN ***", "a flag in capitals");
    assert_eq!(
        journal_text("nuget", "nuget push x.nupkg -ApiKey k3y -Source s"),
        "nuget push x.nupkg -ApiKey *** -Source s",
        "nuget's single-dash form"
    );
    assert_eq!(journal_text("tool", "tool -Password pw x"), "tool -Password *** x");
}

#[test]
fn k_is_a_secret_only_for_dotnet_and_nuget() {
    for program in KEY_PROGRAMS {
        assert_eq!(
            journal_text(program, &format!("{program} x -k k3y")),
            format!("{program} x -k ***"),
            "{program}"
        );
    }
    assert_eq!(journal_text("mysql", "mysql -k x"), "mysql -k x");
    assert_eq!(journal_text("cargo", "cargo test -p gate-win32 -k x"), "cargo test -p gate-win32 -k x");
}

#[test]
fn a_glued_p_is_a_password_only_for_mysql_and_mariadb() {
    for program in GLUED_PASSWORD_PROGRAMS {
        assert_eq!(
            journal_text(program, &format!("{program} -u root -ps3cret -p 5432")),
            format!("{program} -u root -p*** -p 5432"),
            "{program}"
        );
    }
    assert_eq!(journal_text("psql", "psql -p5432 db"), "psql -p5432 db", "psql's -p is a port");
    assert_eq!(journal_text("dotnet", "dotnet build -pfoo"), "dotnet build -pfoo");
}

#[test]
fn dotnet_s_properties_are_kept_but_a_secret_one_is_hidden() {
    assert_eq!(
        journal_text("dotnet", "dotnet build -p:Configuration=Release"),
        "dotnet build -p:Configuration=Release"
    );
    assert_eq!(
        journal_text("dotnet", "dotnet build -property:Configuration=Release"),
        "dotnet build -property:Configuration=Release"
    );
    assert_eq!(journal_text("dotnet", "dotnet build -p:NuGetApiKey=x"), "dotnet build -p:NuGetApiKey=***");
    assert_eq!(
        journal_text("mysql", "mysql -p:Configuration=Release"),
        "mysql -p:Configuration=Release",
        "-p: is never a password"
    );
}

#[test]
fn the_program_falls_back_to_argv0_s_stem() {
    assert_eq!(journal_text("", "mysql -psecret"), "mysql -p***");
    assert_eq!(
        journal_text("", r#""C:\Program Files\MySQL\bin\mysql.exe" -psecret"#),
        r#""C:\Program Files\MySQL\bin\mysql.exe" -p***"#
    );
    assert_eq!(journal_text("", "cargo test -psecret"), "cargo test -psecret");
}

#[test]
fn the_password_of_u_s_user_is_hidden() {
    assert_eq!(journal_text("curl", "curl -u alice:pw https://host"), "curl -u alice:*** https://host");
    assert_eq!(
        journal_text("curl", "curl -u alice https://host"),
        "curl -u alice https://host",
        "no password"
    );
}

#[test]
fn a_token_used_as_u_s_user_is_hidden() {
    assert_eq!(
        journal_text("curl", "curl -u ghp_abc123: https://api.github.com"),
        "curl -u ***:*** https://api.github.com"
    );
    for prefix in TOKEN_PREFIXES {
        assert_eq!(
            journal_text("curl", &format!("curl -u {prefix}xyz:x-oauth-basic https://h")),
            "curl -u ***:*** https://h",
            "{prefix}"
        );
    }
}

#[test]
fn u_before_a_drive_path_is_no_password() {
    assert_eq!(journal_text("python", r"python -u C:\dev\x\run.py"), r"python -u C:\dev\x\run.py");
    assert_eq!(journal_text("python", "python -u D:/x/run.py"), "python -u D:/x/run.py");
}

#[test]
fn the_rest_of_an_authorization_or_bearer_argument_is_hidden() {
    assert_eq!(
        journal_text("curl", r#"curl -H "Authorization: Bearer abc.def" https://host"#),
        r#"curl -H "Authorization: ***" https://host"#
    );
    assert_eq!(journal_text("curl", r#"curl -H "bearer abc" x"#), r#"curl -H "bearer ***" x"#);
    assert_eq!(
        journal_text("git", r#"git -c "http.extraheader=AUTHORIZATION: basic eHk=" fetch"#),
        r#"git -c "http.extraheader=AUTHORIZATION: ***" fetch"#
    );
    assert_eq!(
        journal_text("tool", "tool Authorization: Bearer abc next"),
        "tool Authorization: *** *** next",
        "unquoted, the scheme and the token after it"
    );
    assert_eq!(journal_text("tool", "tool Bearer abc next"), "tool Bearer *** next");
}

#[test]
fn each_auth_scheme_after_an_unquoted_authorization_carries_the_secret_on() {
    for scheme in AUTH_SCHEMES {
        let capitalised = scheme.to_ascii_uppercase();
        assert_eq!(
            journal_text("tool", &format!("tool Authorization: {capitalised} eHk= next")),
            "tool Authorization: *** *** next",
            "{scheme}"
        );
    }
}

#[test]
fn the_value_of_a_key_that_names_a_secret_is_hidden() {
    let line = r#"tool --db-password=a API_KEY=b client_secret=c Pwd=d auth=e "credentials=f" mytoken=g api-key=h apikey=i"#;
    assert_eq!(
        journal_text("tool", line),
        r#"tool --db-password=*** API_KEY=*** client_secret=*** Pwd=*** auth=*** "credentials=***" mytoken=*** api-key=*** apikey=***"#
    );
    assert_eq!(
        journal_text("tool", "tool --name=value -p:Configuration=Release"),
        "tool --name=value -p:Configuration=Release"
    );
}

#[test]
fn every_pair_in_an_argument_is_checked() {
    assert_eq!(
        journal_text("msbuild", "msbuild -p:Configuration=Release;DeployPassword=x /t:Build"),
        "msbuild -p:Configuration=Release;DeployPassword=*** /t:Build"
    );
    assert_eq!(journal_text("mount", "mount -o user=a,password=b /m"), "mount -o user=a,password=*** /m");
    assert_eq!(journal_text("tool", "tool --opt=a,token=xyz"), "tool --opt=a,token=***");
    assert_eq!(
        journal_text("curl", "curl https://h/x?a=1&token=abc&b=2"),
        "curl https://h/x?a=1&token=***&b=2"
    );
}

#[test]
fn a_quoted_argument_holding_white_space_is_redacted_as_a_command_line() {
    assert_eq!(
        journal_text("cmd", r#"cmd.exe /d /s /c "tool --token s3cret""#),
        r#"cmd.exe /d /s /c "tool --token ***""#
    );
    assert_eq!(
        journal_text("bash", r#"bash -c "curl -u alice:pw https://h""#),
        r#"bash -c "curl -u alice:*** https://h""#
    );
    assert_eq!(
        journal_text("cmd", r#"cmd /c "set A=1&& set TOKEN=abc""#),
        r#"cmd /c "set A=1&& set TOKEN=***""#
    );
    assert_eq!(
        journal_text("cmd", r#"cmd /c "mysql -psecret db""#),
        r#"cmd /c "mysql -p*** db""#,
        "the inner program's own rules"
    );
}

#[test]
fn an_escaped_quote_is_a_literal_one() {
    assert_eq!(journal_text("tool", r#"tool --token "a\"b c" x"#), "tool --token *** x");
}

#[test]
fn a_quoted_secret_value_is_hidden_through_its_closing_quote() {
    assert_eq!(journal_text("tool", r#"tool --password="hunter2" x"#), r#"tool --password="***" x"#);
    assert_eq!(
        journal_text("dotnet", r#"dotnet build -p:SignPassword="hunter2""#),
        r#"dotnet build -p:SignPassword="***""#
    );
    assert_eq!(
        journal_text("msbuild", r#"msbuild /p:DeployPassword="a b" /t:Build"#),
        r#"msbuild /p:DeployPassword="***" /t:Build"#
    );
    assert_eq!(
        journal_text("cmd", r#"cmd /c "set TOKEN="abc" && x""#),
        r#"cmd /c "set TOKEN="***" && x""#
    );
    assert_eq!(journal_text("tool", r#"tool --token="a b" x"#), r#"tool --token="***" x"#);
    assert_eq!(
        journal_text("tool", r#"tool --token="a b"#),
        r#"tool --token="***"#,
        "a quote never closed"
    );
}

#[test]
fn a_secret_value_runs_past_a_separator_no_pair_follows() {
    assert_eq!(journal_text("tool", "tool --password=ab;cd&ef,gh x"), "tool --password=*** x");
    assert_eq!(
        journal_text("docker", "docker login --password=p@ss,w0rd x"),
        "docker login --password=*** x"
    );
}

#[test]
fn a_literal_quote_neither_opens_nor_closes_a_quoted_argument() {
    assert_eq!(
        journal_text("tool", r#"tool --in "C:\My Dir\" --token s3cret"#),
        r#"tool --in "C:\My Dir\" --token ***"#
    );
}

#[test]
fn the_rest_of_a_cmd_line_after_c_or_k_is_a_command_line_of_its_own() {
    assert_eq!(
        journal_text(
            "cmd",
            r#"cmd.exe /d /s /c ""C:\Program Files\MySQL\MySQL Server 8.0\bin\mysql.exe" -uroot -psecret""#
        ),
        r#"cmd.exe /d /s /c ""C:\Program Files\MySQL\MySQL Server 8.0\bin\mysql.exe" -uroot -p***""#
    );
    assert_eq!(journal_text("cmd", "cmd /c mysql -psecret"), "cmd /c mysql -p***");
    assert_eq!(
        journal_text("", "cmd /K mysql -psecret"),
        "cmd /K mysql -p***",
        "argv[0]'s stem, /K in capitals"
    );
    assert_eq!(
        journal_text("cmd", "cmd /d /c dotnet nuget push p.nupkg -k KEY"),
        "cmd /d /c dotnet nuget push p.nupkg -k ***"
    );
    assert_eq!(journal_text("tool", "tool /c mysql -psecret"), "tool /c mysql -psecret", "only cmd's /c");
}

#[test]
fn each_kind_of_token_is_hidden_wherever_it_stands() {
    for prefix in TOKEN_PREFIXES {
        assert_eq!(
            journal_text("tool", &format!("tool {prefix}abc123 x={prefix}def456")),
            "tool *** x=***",
            "{prefix}"
        );
    }
    assert_eq!(
        journal_text("tool", "tool task-runner disk-sk-x"),
        "tool task-runner disk-sk-x",
        "not at a word's start"
    );
}

#[test]
fn a_secret_the_cut_would_split_is_hidden_before_the_cut() {
    let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let line = format!("{} {token}", "x".repeat(COMMAND_CHARS - 10));
    let kept = journal_text("tool", &line);
    assert!(!kept.contains("ghp_") && !kept.contains("abcd"), "{kept}");

    let line = format!("{} https://alice:hunter2hunter2@host", "x".repeat(COMMAND_CHARS - 20));
    let kept = journal_text("tool", &line);
    assert!(!kept.contains("hunter"), "{kept}");
}

#[test]
fn controls_separators_and_direction_marks_become_spaces() {
    let unsafe_characters = [
        '\u{0}', '\u{1b}', '\t', '\n', '\u{7f}', '\u{80}', '\u{9b}', '\u{9f}', '\u{2028}', '\u{2029}',
    ]
    .into_iter()
    .chain('\u{202a}'..='\u{202e}')
    .chain('\u{2066}'..='\u{2069}');
    for character in unsafe_characters {
        assert_eq!(sanitize(&format!("a{character}b")), "a b", "U+{:04X}", u32::from(character));
    }
    assert_eq!(sanitize("é ü 漢 \u{200d}"), "é ü 漢 \u{200d}", "other characters are kept");
    assert_eq!(journal_text("tool", "tool \u{1b}[31mred"), "tool  [31mred");
}

#[test]
fn a_line_is_cut_to_200_characters() {
    assert_eq!(journal_text("tool", &"é".repeat(300)).chars().count(), COMMAND_CHARS);
}

#[test]
fn an_image_is_sanitized_and_cut_to_its_last_1024_characters() {
    let path = format!(r"C:\{}\tool.exe", "d\u{202e}".repeat(1_000));
    let kept = journal_image(&path);

    assert_eq!(kept.chars().count(), IMAGE_CHARS);
    assert!(kept.ends_with(r"\tool.exe"), "the file name survives the cut");
    assert!(!kept.contains('\u{202e}'), "sanitized");
    assert_eq!(journal_image(r"C:\x\tool.exe"), r"C:\x\tool.exe", "a short path is kept whole");
}

/// A small seeded generator, so a failing string can be found again.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

#[test]
fn any_line_gives_at_most_200_safe_characters_without_panicking() {
    const WORDS: [&str; 18] = [
        "\"",
        "\\",
        " /c ",
        "password=",
        " ",
        "\t",
        "=",
        ":",
        ",",
        ";",
        "&",
        "?",
        "@",
        "://",
        "-p",
        "-u",
        "--token",
        "Authorization:",
    ];
    const CHARACTERS: [char; 12] = ['a', 'Z', '0', '-', '_', 'é', '漢', '\u{0}', '\u{1b}', '\u{9b}', '\u{2028}', '\u{202e}'];
    const PROGRAMS: [&str; 5] = ["", "mysql", "dotnet", "curl", "cmd"];
    const SEEDS: [&str; 7] = [
        r#"tool --password="hunter2" x"#,
        r#"cmd /c "set TOKEN="abc" && x""#,
        r#"tool --token="a b"#,
        "tool --password=ab;cd&ef,gh x",
        r#"tool --in "C:\My Dir\" --token s3cret"#,
        r#"cmd.exe /d /s /c ""C:\Program Files\MySQL\bin\mysql.exe" -uroot -psecret""#,
        r#"cmd /c cmd /k "cmd /c "" /c"#,
    ];
    for seed in SEEDS {
        for program in PROGRAMS {
            let kept = journal_text(program, seed);
            assert!(
                kept.chars().count() <= COMMAND_CHARS && !kept.chars().any(is_unsafe),
                "{seed:?} gave {kept:?}"
            );
        }
    }
    let mut random = Seeded(0x9e37_79b9_7f4a_7c15);
    for round in 0..5_000 {
        let mut line = String::new();
        for _ in 0..random.below(120) {
            match random.below(3) {
                0 => line.push_str(WORDS[random.below(WORDS.len())]),
                1 => line.push_str(TOKEN_PREFIXES[random.below(TOKEN_PREFIXES.len())]),
                _ => line.push(CHARACTERS[random.below(CHARACTERS.len())]),
            }
        }
        let program = PROGRAMS[random.below(PROGRAMS.len())];

        let kept = journal_text(program, &line);

        assert!(kept.chars().count() <= COMMAND_CHARS, "round {round}: {line:?}");
        assert!(!kept.chars().any(is_unsafe), "round {round}: {line:?} gave {kept:?}");
    }
}
