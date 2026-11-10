//! `gate csharp format`: nothing to do without files, the verb it needs, and a real run's restore, index check, format
//! and verify through stand-in programs.

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
fn a_real_run_restores_checks_the_index_formats_and_verifies_in_order() {
    let case = Case::new("a_real_run_restores_checks_the_index_formats_and_verifies_in_order");
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
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert!(calls[0].starts_with("dotnet restore ") && calls[0].ends_with("a.csproj"), "{calls:?}");
    assert_eq!(calls[1], "git ls-files --error-unmatch -- a.cs");
    assert_eq!(calls[2], "git diff --quiet -- a.cs");
    assert_eq!(
        calls[3],
        "dotnet format style --severity info --no-restore --exclude-diagnostics IDE0059 IDE0060 IDE0130 --include a.cs"
    );
    assert_eq!(
        calls[4],
        "dotnet format style --verify-no-changes --severity info --no-restore --diagnostics IDE0059 IDE0060 --include a.cs"
    );
}

#[test]
fn a_real_run_stages_the_file_its_format_pass_changed() {
    let case = Case::new("a_real_run_stages_the_file_its_format_pass_changed");
    let marker = "@if \"%2\"==\"style\" if \"%3\"==\"--severity\" if not exist \"%~dp0formatted\" (echo changed>a.cs& echo.>\"%~dp0formatted\")";
    let path = stand_ins(&case, &[("dotnet", &[marker]), ("git", &[])]);
    let folder = case.folder();
    fs::write(folder.join("a.csproj"), "").expect("the project can be written");
    fs::write(folder.join("a.cs"), "").expect("the file can be written");

    let mut command = case.gate(&["csharp", "format", "a.cs"]);
    command.env("PATH", path);
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(folder.join("calls.log")).expect("the stand-ins wrote their calls");
    let calls: Vec<&str> = log.lines().map(str::trim_end).collect();
    let format = "dotnet format style --severity info --no-restore --exclude-diagnostics IDE0059 IDE0060 IDE0130 --include a.cs";
    assert_eq!(calls.len(), 7, "{calls:?}");
    assert!(calls[0].starts_with("dotnet restore ") && calls[0].ends_with("a.csproj"), "{calls:?}");
    assert_eq!(calls[1], "git ls-files --error-unmatch -- a.cs");
    assert_eq!(calls[2], "git diff --quiet -- a.cs");
    assert_eq!(calls[3], format);
    assert_eq!(calls[4], format);
    assert_eq!(calls[5], "git add -- a.cs");
    assert_eq!(
        calls[6],
        "dotnet format style --verify-no-changes --severity info --no-restore --diagnostics IDE0059 IDE0060 --include a.cs"
    );
    let written = fs::read_to_string(folder.join("a.cs")).expect("the file can be read");
    assert_eq!(written.trim(), "changed", "{written:?}");
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
    let log = fs::read_to_string(folder.join("calls.log")).expect("the git stand-in wrote its calls");
    let calls: Vec<&str> = log.lines().map(str::trim_end).collect();
    assert_eq!(calls, ["git ls-files --error-unmatch -- a.cs", "git diff --quiet -- a.cs"], "{calls:?}");
    assert!(!calls.iter().any(|call| call.starts_with("dotnet")), "no dotnet stand-in ran: {calls:?}");
}

#[test]
fn the_index_checks_print_nothing_of_their_own() {
    let case = Case::new("the_index_checks_print_nothing_of_their_own");
    let path = stand_ins(&case, &[("dotnet", &[]), ("git", &["@echo noisy-out", "@echo noisy-err 1>&2"])]);
    let folder = case.folder();
    fs::write(folder.join("a.csproj"), "").expect("the project can be written");
    fs::write(folder.join("a.cs"), "").expect("the file can be written");

    let mut command = case.gate(&["csharp", "format", "a.cs"]);
    command.env("PATH", path);
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stdout.contains("noisy"), "{outcome:?}");
    assert!(!outcome.stderr.contains("noisy"), "{outcome:?}");
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
