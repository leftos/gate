//! `gate csharp format`: nothing to do without files, the verb it needs, and a real run's restore, format, stage and
//! verify through stand-in programs.

use std::ffi::OsString;
use std::fs;

use crate::support::{Case, USAGE, finish};

#[test]
fn a_format_of_no_files_says_nothing_and_ends_zero() {
    let case = Case::new("a_format_of_no_files_says_nothing_and_ends_zero");
    let outcome = case.run(&["csharp", "format"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

#[test]
fn a_bad_verb_is_a_usage_error() {
    let case = Case::new("a_bad_verb_is_a_usage_error");
    let outcome = case.run(&["csharp", "fix"]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "nothing is formatted");
    assert_eq!(outcome.stderr, format!("gate: csharp takes the verb format, not fix\n{USAGE}"));
}

#[test]
fn a_real_run_restores_formats_stages_and_verifies_in_order() {
    let case = Case::new("a_real_run_restores_formats_stages_and_verifies_in_order");
    let path = stand_ins(&case, &[("dotnet", &[]), ("git", &[])]);
    let folder = case.folder();
    fs::write(folder.join("a.csproj"), "").expect("the project can be written");
    fs::write(folder.join("a.cs"), "").expect("the file can be written");

    let mut command = case.gate(&["csharp", "format", "a.cs"]);
    command.env("PATH", path);
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(folder.join("calls.log")).expect("the stand-ins wrote their calls");
    let calls: Vec<&str> = log.lines().map(str::trim_end).collect();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[0].starts_with("dotnet restore ") && calls[0].ends_with("a.csproj"), "{calls:?}");
    assert_eq!(
        calls[1],
        "dotnet format style --severity info --no-restore --exclude-diagnostics IDE0059 IDE0060 IDE0130 --include a.cs"
    );
    assert_eq!(calls[2], "git add -- a.cs");
    assert_eq!(
        calls[3],
        "dotnet format style --verify-no-changes --severity info --no-restore --diagnostics IDE0059 IDE0060 --include a.cs"
    );
}

#[test]
fn a_stand_in_that_fails_ends_the_run_with_its_status() {
    let case = Case::new("a_stand_in_that_fails_ends_the_run_with_its_status");
    let path = stand_ins(&case, &[("dotnet", &["@exit /b 4"]), ("git", &[])]);
    let folder = case.folder();
    fs::write(folder.join("a.csproj"), "").expect("the project can be written");
    fs::write(folder.join("a.cs"), "").expect("the file can be written");

    let mut command = case.gate(&["csharp", "format", "a.cs"]);
    command.env("PATH", path);
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    let log = fs::read_to_string(folder.join("calls.log")).expect("the stand-ins wrote their calls");
    let calls: Vec<&str> = log.lines().map(str::trim_end).collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("dotnet restore "), "{calls:?}");
    assert!(outcome.stderr.contains("failed (status 4)"), "{outcome:?}");
}

#[test]
fn a_missing_program_says_it_cannot_start() {
    let case = Case::new("a_missing_program_says_it_cannot_start");
    let path = stand_ins(&case, &[("git", &[])]);
    let folder = case.folder();
    fs::write(folder.join("a.cs"), "").expect("the file can be written");

    let mut command = case.gate(&["csharp", "format", "a.cs"]);
    command.env("PATH", path);
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: csharp format: cannot start dotnet: "), "{outcome:?}");
    assert!(!folder.join("calls.log").exists(), "no stand-in ran");
}

/// The case's `stub` folder, holding a `{name}.cmd` for each of `programs` that appends its name and words to the case's
/// `calls.log` and then runs its own lines; answers it as the `PATH` the gate's child programs are found through.
fn stand_ins(case: &Case, programs: &[(&str, &[&str])]) -> OsString {
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    let log = case.folder().join("calls.log");
    for (name, lines) in programs {
        let mut script = format!("@echo {name} %*>>\"{}\"\r\n", log.display());
        for line in *lines {
            script.push_str(line);
            script.push_str("\r\n");
        }
        fs::write(stub.join(format!("{name}.cmd")), script).expect("a stand-in can be written");
    }
    stub.into_os_string()
}
