//! The routing decision: each rule in its order, the cases where the rewrite and the bare first word differ from the
//! PowerShell gate, the subst-drive table, and which facts are asked when. Raising `--stall` for a remote run is the
//! remote client's, not the router's.

use std::path::PathBuf;

use gate_router::{
    BuildSettings, BuildTarget, Config, ConfigProblem, DROPPED_NO_BUILD_LINE, DotnetTarget, Inputs, LocalReason, NO_OUTPUTS_LINE, PROJECT_EXTENSIONS,
    Place, RemoteRun, Route, SiblingFolder, Slot, build_settings, decide, dotnet_builds, dotnet_target, is_cargo, is_dotnet, is_usable_host,
    sdk_build_target, without_no_build, work_folder_key,
};

use crate::support::{FakeFacts, HEAD, ROOT, carried, fake_id, heavy, place_at, sibling_root, words};

const SILENT: Route = Route::Local { reason: None };

fn routed(route: Route) -> RemoteRun {
    assert!(matches!(route, Route::Routed(_)), "the command runs here: {:?}", local_line(&route));
    match route {
        Route::Routed(run) => *run,
        Route::Local { .. } => unreachable!("asserted to route"),
    }
}

fn local_line(route: &Route) -> Option<String> {
    match route {
        Route::Local { reason } => reason.as_ref().map(LocalReason::line),
        Route::Routed(_) => Some("routed".to_owned()),
    }
}

fn place(prefix: &str, checkout: &str) -> Place {
    Place {
        checkout: checkout.to_owned(),
        ..place_at(prefix)
    }
}

fn routing_all() -> Config {
    Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}}"#).expect("usable")
}

/// The source-revision word a routed build carries: the fixture worktree's [`HEAD`].
fn stamp() -> String {
    format!("-p:SourceRevisionId={HEAD}")
}

/// A command's words as the builder gets them: `list` with the source-revision word after the last one.
fn stamped(list: &[&str]) -> Vec<String> {
    let mut words = words(list);
    words.push(stamp());
    words
}

/// Whether the decision asked for the worktree's HEAD, which only a command that gets the word does.
fn asked_head(facts: &FakeFacts) -> bool {
    facts.asked().iter().any(|question| question.as_str() == "head_sha")
}

#[test]
fn the_control_command_routes_from_the_root() {
    let command = words(&["dotnet", "test", "X:/dev/yaat.wt/a/tests/T.csproj"]);
    let facts = FakeFacts::at_root();
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(
        run,
        RemoteRun {
            host: "builder-1".to_owned(),
            slots: 2,
            root: ROOT.to_owned(),
            leaf: "a".to_owned(),
            siblings: Vec::new(),
            repo: "yaat".to_owned(),
            key: work_folder_key(ROOT),
            subfolder: String::new(),
            pattern: "dotnet *".to_owned(),
            words: stamped(&["dotnet", "test", "tests/T.csproj"]),
            fallback: command.clone(),
            outputs: words(&["**/bin/**", "**/.godot/mono/temp/bin/**"]),
            dropped_no_build: false,
            outputs_withheld: false,
        }
    );
    assert_eq!(run.lines(), Vec::<&str>::new());
    assert_eq!(facts.asked(), ["config", "place", "file_in_folder dotnet", "head_sha"]);
}

#[test]
fn only_a_heavy_gate_routes_and_light_critical_or_none_ask_nothing_not_even_the_routing_file() {
    let command = words(&["dotnet", "test"]);
    for slot in [Slot::Light, Slot::Critical, Slot::None] {
        let facts = FakeFacts::at_root();
        let inputs = Inputs { slot, ..heavy(&command) };
        assert_eq!(decide(&inputs, &facts), SILENT, "{slot:?}");
        assert!(facts.asked().is_empty(), "{slot:?} asked {:?}", facts.asked());
    }
}

#[test]
fn local_on_a_heavy_gate_says_not_routed_and_asks_nothing_not_even_the_routing_file() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts::at_root();
    let route = decide(
        &Inputs {
            local: true,
            ..heavy(&command)
        },
        &facts,
    );
    assert_eq!(
        route,
        Route::Local {
            reason: Some(LocalReason::LocalFlag)
        }
    );
    assert_eq!(local_line(&route).as_deref(), Some("gate: not routed: --local"));
    assert_eq!(facts.asked(), Vec::<String>::new());
    let light = Inputs {
        slot: Slot::Light,
        local: true,
        ..heavy(&command)
    };
    assert_eq!(decide(&light, &facts), SILENT);
    assert_eq!(facts.asked(), Vec::<String>::new());
}

#[test]
fn local_with_no_routing_file_still_says_not_routed() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        config: Ok(None),
        ..FakeFacts::at_root()
    };
    let route = decide(
        &Inputs {
            local: true,
            ..heavy(&command)
        },
        &facts,
    );
    assert_eq!(local_line(&route).as_deref(), Some("gate: not routed: --local"));
    assert_eq!(facts.asked(), Vec::<String>::new());
}

#[test]
fn no_routing_file_runs_here_silently_asking_no_git_fact() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        config: Ok(None),
        ..FakeFacts::at_root()
    };
    assert_eq!(decide(&heavy(&command), &facts), SILENT);
    assert_eq!(facts.asked(), ["config"]);
}

#[test]
fn an_unusable_routing_file_on_a_heavy_gate_says_ignored_and_runs_here() {
    let reason = Config::parse("{").expect_err("broken");
    let path = PathBuf::from(r"C:\gate\remote.json");
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        config: Err(ConfigProblem {
            path: path.clone(),
            reason: reason.clone(),
        }),
        ..FakeFacts::at_root()
    };
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        route,
        Route::Local {
            reason: Some(LocalReason::Ignored {
                path,
                reason: reason.clone()
            })
        }
    );
    assert_eq!(
        local_line(&route),
        Some(format!(r"gate: ignored C:\gate\remote.json: {reason}; running locally"))
    );
    assert_eq!(facts.asked(), ["config"]);
}

#[test]
fn an_unusable_routing_file_is_said_even_inside_a_git_hook() {
    let command = words(&["git", "commit"]);
    let facts = FakeFacts {
        config: Err(ConfigProblem {
            path: PathBuf::from("remote.json"),
            reason: "host is missing".to_owned(),
        }),
        ..FakeFacts::at_root()
    };
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    assert_eq!(
        local_line(&decide(&inputs, &facts)).as_deref(),
        Some("gate: ignored remote.json: host is missing; running locally")
    );
}

#[test]
fn a_git_hook_stays_local() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts::at_root();
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    assert_eq!(decide(&inputs, &facts), SILENT);
    assert_eq!(facts.asked(), ["config"]);
}

#[test]
fn no_non_empty_word_or_a_git_or_prek_command_stays_local() {
    let commands: [&[&str]; 9] = [
        &[],
        &["", ""],
        &["git", "status"],
        &["GIT.EXE", "status"],
        &[r"C:\Program Files\Git\cmd\git.exe", "status"],
        &["", "git", "status"],
        &["prek", "run"],
        &["/usr/bin/prek.cmd", "run"],
        &["git.", "status"],
    ];
    for command in commands {
        let command = words(command);
        let facts = FakeFacts::with_config(routing_all());
        assert_eq!(decide(&heavy(&command), &facts), SILENT, "{command:?}");
        assert_eq!(facts.asked(), ["config"], "{command:?}");
    }
    let command = words(&["gitk", "x"]);
    assert!(matches!(
        decide(&heavy(&command), &FakeFacts::with_config(routing_all())),
        Route::Routed(_)
    ));
}

#[test]
fn a_covering_parent_keeps_it_here_silently_even_with_local_and_reads_no_routing_file() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts::at_root();
    let inputs = Inputs {
        covering_parent: true,
        local: true,
        ..heavy(&command)
    };
    assert_eq!(decide(&inputs, &facts), SILENT);
    assert_eq!(facts.asked(), Vec::<String>::new());
}

#[test]
fn a_folder_in_no_git_worktree_runs_here_silently() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        place: Ok(None),
        ..FakeFacts::at_root()
    };
    assert_eq!(decide(&heavy(&command), &facts), SILENT);
    assert_eq!(facts.asked(), ["config", "place"]);
}

#[test]
fn the_repo_is_the_checkout_folder_name_cut_and_matched_in_any_case() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        place: Ok(Some(place("", ".YAAT"))),
        ..FakeFacts::at_root()
    };
    assert_eq!(routed(decide(&heavy(&command), &facts)).repo, "YAAT");
    let unlisted = FakeFacts {
        place: Ok(Some(place("", "other"))),
        ..FakeFacts::at_root()
    };
    assert_eq!(decide(&heavy(&command), &unlisted), SILENT);
    assert_eq!(unlisted.asked(), ["config", "place"]);
}

#[test]
fn rewritten_words_joined_by_spaces_match_and_the_first_pattern_wins() {
    let command = words(&["X:/dev/yaat.wt/a/tool", "--x"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(run.pattern, "tool *");
    assert_eq!(run.words, ["tool", "--x"]);
    let unmatched = words(&["cargo", "build"]);
    assert_eq!(decide(&heavy(&unmatched), &FakeFacts::at_root()), SILENT);
}

#[test]
fn an_empty_pattern_list_never_matches() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        place: Ok(Some(place("", "empty"))),
        ..FakeFacts::at_root()
    };
    assert_eq!(decide(&heavy(&command), &facts), SILENT);
}

#[test]
fn a_word_outside_the_worktree_says_so() {
    let command = words(&["dotnet", "test", r"..\other\x.csproj"]);
    let route = decide(&heavy(&command), &FakeFacts::at_root());
    assert_eq!(
        local_line(&route).as_deref(),
        Some(r"gate: not routed: ..\other\x.csproj is outside the worktree")
    );
}

#[test]
fn a_word_outside_the_worktree_wins_over_a_folder_holding_nothing() {
    let command = words(&["dotnet", "test", r"..\..\..\other\x.csproj"]);
    let facts = FakeFacts {
        has_files: Ok(false),
        ..FakeFacts::at("src/empty/")
    };
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some(r"gate: not routed: ..\..\..\other\x.csproj is outside the worktree")
    );
    assert_eq!(facts.asked(), ["config", "place"]);
}

#[test]
fn a_folder_holding_nothing_the_snapshot_sends_says_so() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        has_files: Ok(false),
        ..FakeFacts::at("src/empty/")
    };
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: src/empty holds nothing the snapshot sends")
    );
    assert_eq!(facts.asked(), ["config", "place", "folder_has_files src/empty/"]);
}

#[test]
fn the_root_is_never_asked_whether_it_holds_files() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts {
        has_files: Ok(false),
        ..FakeFacts::at_root()
    };
    assert!(matches!(decide(&heavy(&command), &facts), Route::Routed(_)));
    assert!(!facts.asked().iter().any(|question| question.starts_with("folder_has_files")));
}

#[test]
fn a_subfolder_routes_with_its_name() {
    let command = words(&["dotnet", "test"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at("src/c/")));
    assert_eq!(run.subfolder, "src/c");
}

#[test]
fn a_fact_that_fails_says_not_routed_on_one_line() {
    let command = words(&["dotnet", "test"]);
    let place_fails = FakeFacts {
        place: Err("git could not be started:\r\n  access\n\n  denied".to_owned()),
        ..FakeFacts::at_root()
    };
    assert_eq!(
        local_line(&decide(&heavy(&command), &place_fails)).as_deref(),
        Some("gate: not routed: git could not be started: access denied")
    );
    let listing_fails = FakeFacts {
        has_files: Err("git ls-files of sub/ failed (status 128)".to_owned()),
        ..FakeFacts::at("sub/")
    };
    assert_eq!(
        local_line(&decide(&heavy(&command), &listing_fails)).as_deref(),
        Some("gate: not routed: git ls-files of sub/ failed (status 128)")
    );
}

#[test]
fn no_build_is_dropped_from_a_dotnet_command_for_the_builder_and_the_fallback() {
    let command = words(&["DotNet.EXE", "test", "--no-build", "--no-build=x", "--NO-BUILD", "--x"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(run.pattern, "* --x");
    assert_eq!(
        run.words,
        ["DotNet.EXE", "test", "--no-build=x", "--NO-BUILD", "--x", stamp().as_str()],
        "there is no --, so the sha stands after the last word"
    );
    assert_eq!(run.fallback, ["DotNet.EXE", "test", "--no-build=x", "--NO-BUILD", "--x"]);
    assert!(run.dropped_no_build);
    assert_eq!(run.lines()[0], DROPPED_NO_BUILD_LINE);
    assert_eq!(
        DROPPED_NO_BUILD_LINE,
        "gate: dropped --no-build: the last build may have run on the other machine"
    );
}

#[test]
fn no_build_stays_on_any_other_command() {
    let command = words(&["cargo", "test", "--no-build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(run.words, command);
    assert_eq!(run.fallback, command);
    assert!(!run.dropped_no_build);
    assert!(!run.outputs_withheld);
    assert_eq!(run.lines(), Vec::<&str>::new());
}

#[test]
fn a_dotnet_no_build_command_brings_no_outputs_back() {
    let command = words(&["dotnet", "test", "--no-build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(
        run.words,
        ["dotnet", "test", stamp().as_str()],
        "the builder's words build, pinned to the HEAD"
    );
    assert_eq!(
        run.fallback,
        ["dotnet", "test"],
        "the fallback builds without --no-build and without the word"
    );
    assert_eq!(run.outputs, Vec::<String>::new());
    assert!(run.outputs_withheld);
    assert_eq!(run.lines(), [DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE]);
    assert_eq!(NO_OUTPUTS_LINE, "gate: outputs: none brought back for a --no-build command");
}

/// Asserts whether `given` is a dotnet command, and the words it leaves once each `--no-build` of the dotnet command's
/// own is dropped.
fn assert_dotnet(given: &[&str], dotnet: bool, dropped: &[&str]) {
    let given = words(given);
    assert_eq!(is_dotnet(&given), dotnet, "{given:?}");
    assert_eq!(without_no_build(given.clone()), dropped, "{given:?}");
}

#[test]
fn a_dot_sourced_pwsh_script_handed_dotnet_after_its_separator_is_a_dotnet_command() {
    assert_dotnet(
        &["pwsh", ".", "tools/hidden-desktop.ps1", "--", "dotnet", "test", "--no-build", "."],
        true,
        &["pwsh", ".", "tools/hidden-desktop.ps1", "--", "dotnet", "test", "."],
    );
}

#[test]
fn a_pwsh_file_script_handed_dotnet_build_is_a_dotnet_command() {
    let given = ["pwsh", "-NoProfile", "-File", "tools/x.ps1", "--", "dotnet", "build"];
    assert_dotnet(&given, true, &given);
}

#[test]
fn the_pwsh_and_dotnet_words_are_file_stems_compared_ignoring_case() {
    assert_dotnet(
        &["pwsh.exe", "-File", "x.ps1", "--", "DOTNET.exe", "test", "--no-build"],
        true,
        &["pwsh.exe", "-File", "x.ps1", "--", "DOTNET.exe", "test"],
    );
}

#[test]
fn powershell_wraps_a_dotnet_command_as_pwsh_does() {
    assert_dotnet(
        &["powershell.exe", "-File", "x.ps1", "--", "dotnet", "test", "--no-build"],
        true,
        &["powershell.exe", "-File", "x.ps1", "--", "dotnet", "test"],
    );
}

#[test]
fn dotnet_itself_is_a_dotnet_command() {
    assert_dotnet(&["dotnet", "test", "--no-build"], true, &["dotnet", "test"]);
}

#[test]
fn a_pwsh_script_handed_dotnet_without_a_separator_is_no_dotnet_command() {
    let given = ["pwsh", "-File", "x.ps1", "dotnet", "test", "--no-build"];
    assert_dotnet(&given, false, &given);
}

#[test]
fn a_pwsh_script_whose_separator_is_not_followed_by_dotnet_is_no_dotnet_command() {
    let given = ["pwsh", "-File", "x.ps1", "--", "echo", "dotnet"];
    assert_dotnet(&given, false, &given);
}

#[test]
fn a_script_run_by_anything_but_pwsh_is_no_dotnet_command() {
    let given = ["cmd", "/c", "x.cmd", "--", "dotnet", "test", "--no-build"];
    assert_dotnet(&given, false, &given);
}

#[test]
fn a_no_build_after_the_wrapped_dotnet_commands_own_separator_is_kept() {
    let given = ["pwsh", "-File", "x.ps1", "--", "dotnet", "test", "--", "--no-build"];
    assert_dotnet(&given, true, &given);
}

#[test]
fn only_the_words_after_the_wrapping_separator_lose_their_no_build() {
    assert_dotnet(
        &["pwsh", "-File", "x.ps1", "--no-build", "--", "dotnet", "test", "--no-build"],
        true,
        &["pwsh", "-File", "x.ps1", "--no-build", "--", "dotnet", "test"],
    );
}

#[test]
fn a_no_build_after_dotnets_own_separator_is_left_for_the_program_it_runs() {
    let given = ["dotnet", "test", "--", "--no-build"];
    assert_dotnet(&given, true, &given);
}

#[test]
fn only_the_sdk_verbs_that_take_no_build_lose_it() {
    let kept: [&[&str]; 4] = [
        &["dotnet", "app.dll", "--no-build"],
        &["dotnet", "exec", "x.dll", "--no-build"],
        &["dotnet", "tool", "run", "t", "--no-build"],
        &["pwsh", "-File", "x.ps1", "--", "dotnet", "exec", "x.dll", "--no-build"],
    ];
    for given in kept {
        assert_dotnet(given, true, given);
    }
    for verb in ["test", "run", "Test", "RUN"] {
        assert_dotnet(&["dotnet", verb, "--no-build"], true, &["dotnet", verb]);
    }
    for verb in ["build", "publish", "pack", "msbuild", "Pack"] {
        let given = ["dotnet", verb, "--no-build"];
        assert_dotnet(&given, true, &given);
    }
}

#[test]
fn a_dotnet_command_with_a_response_file_is_opaque() {
    for given in [
        &["dotnet", "test", "@x.rsp", "--no-build"][..],
        &["pwsh", "-File", "x.ps1", "--", "dotnet", "test", "--no-build", "@args.rsp"],
    ] {
        assert_eq!(without_no_build(words(given)), given, "no drop: {given:?}");
    }
    assert!(!dotnet_builds(&words(&["dotnet", "build", "@x.rsp"])), "no record and no clear");
    assert_eq!(dotnet_target(&words(&["dotnet", "build", "All.sln", "@x.rsp"])), None);
    assert_eq!(sdk_build_target(&words(&["dotnet", "build", "@x.rsp"])), None);
    assert!(
        dotnet_builds(&words(&["dotnet", "test", "--", "@x.rsp"])),
        "a word for the program it runs is not dotnet's"
    );
    let run = routed(decide(&heavy(&words(&["dotnet", "test", "@x.rsp", "--no-build"])), &FakeFacts::at_root()));
    assert_eq!(run.words, ["dotnet", "test", "@x.rsp", "--no-build"], "no drop and no stamp");
    assert!(!run.dropped_no_build);
}

#[test]
fn a_dotnet_build_reads_its_configuration_framework_and_runtime_in_every_spelling() {
    let settings = |given: &[&str]| build_settings(&words(given));
    let set = |configuration: Option<&str>, framework: Option<&str>, runtime: Option<&str>| BuildSettings {
        configuration: configuration.map(str::to_owned),
        framework: framework.map(str::to_owned),
        runtime: runtime.map(str::to_owned),
    };
    assert_eq!(settings(&["dotnet", "build"]), BuildSettings::default());
    for given in [
        &["dotnet", "build", "-c", "Release", "-f", "net10.0", "-r", "win-x64"][..],
        &["dotnet", "build", "-c:Release", "-f:net10.0", "-r:win-x64"],
        &["dotnet", "build", "-c=Release", "-f=net10.0", "-r=win-x64"],
        &[
            "dotnet",
            "build",
            "--configuration",
            "Release",
            "--framework",
            "net10.0",
            "--runtime",
            "win-x64",
        ],
        &["dotnet", "build", "--configuration=Release", "--framework=net10.0", "--runtime=win-x64"],
        &["dotnet", "build", "--configuration:Release", "--framework:net10.0", "--runtime:win-x64"],
    ] {
        assert_eq!(settings(given), set(Some("Release"), Some("net10.0"), Some("win-x64")), "{given:?}");
    }
    assert_eq!(
        settings(&["dotnet", "test", "-c", "Debug", "-c", "Release"]),
        set(Some("Release"), None, None),
        "the last wins"
    );
    assert_eq!(
        settings(&["dotnet", "test", "--", "-c", "Release"]),
        BuildSettings::default(),
        "the program's words"
    );
    assert_eq!(
        settings(&["pwsh", "-File", "x.ps1", "-c", "X", "--", "dotnet", "build", "-r", "linux-x64"]),
        set(None, None, Some("linux-x64"))
    );
    assert!(
        set(Some("Release"), None, None).same_as(&set(Some("release"), None, None)),
        "letter case aside"
    );
    assert!(
        !set(Some("Release"), None, None).same_as(&BuildSettings::default()),
        "absent is not present"
    );
    assert!(BuildSettings::default().same_as(&BuildSettings::default()), "absent equals absent");
    assert!(!set(None, Some("net9.0"), None).same_as(&set(None, Some("net10.0"), None)));
}

#[test]
fn a_dotnet_build_names_a_solution_nothing_or_something_else() {
    let target = |given: &[&str]| sdk_build_target(&words(given));
    assert_eq!(target(&["dotnet", "build"]), Some(BuildTarget::Unnamed));
    assert_eq!(
        target(&["dotnet", "test", "-c", "Release", "-v", "q", "--no-restore"]),
        Some(BuildTarget::Unnamed)
    );
    assert_eq!(target(&["dotnet", "build", "All.sln"]), Some(BuildTarget::Solution));
    assert_eq!(target(&["dotnet", "build", "-c", "Release", "All.SLNX"]), Some(BuildTarget::Solution));
    for given in [
        &["dotnet", "build", "One.csproj"][..],
        &["dotnet", "run", "--project", "One"],
        &["dotnet", "build", "src/App"],
        &["dotnet", "run", "app.cs"],
        &["dotnet", "build", "-unknown", "value"],
    ] {
        assert_eq!(target(given), Some(BuildTarget::Other), "{given:?}");
    }
    assert_eq!(target(&["dotnet", "msbuild", "-t:Restore"]), None, "msbuild");
    assert_eq!(target(&["dotnet", "test", "--no-build"]), None, "no build");
    assert_eq!(target(&["dotnet", "format"]), None, "a verb that does not build");
    assert_eq!(PROJECT_EXTENSIONS, [".csproj", ".fsproj", ".vbproj"]);
}

#[test]
fn a_dotnet_command_names_the_project_or_solution_it_builds() {
    let target = |given: &[&str]| dotnet_target(&words(given));
    let project = |name: &str| Some(DotnetTarget::Project(name.to_owned()));
    let solution = |name: &str| Some(DotnetTarget::Solution(name.to_owned()));
    assert_eq!(target(&["dotnet", "build", "One.csproj"]), project("One.csproj"));
    assert_eq!(
        target(&["dotnet", "test", "-c", "Release", r"src\Two.FSPROJ"]),
        project(r"src\Two.FSPROJ")
    );
    assert_eq!(target(&["dotnet", "build", "lib/Three.vbproj"]), project("lib/Three.vbproj"));
    assert_eq!(target(&["dotnet", "run", "--project", "One"]), project("One"));
    assert_eq!(target(&["dotnet", "run", "--project=One"]), project("One"));
    assert_eq!(target(&["dotnet", "run", "--project:One"]), project("One"));
    assert_eq!(target(&["dotnet", "run", "--project"]), project(""));
    assert_eq!(target(&["dotnet", "build", "All.sln"]), solution("All.sln"));
    assert_eq!(target(&["dotnet", "build", "All.slnx"]), solution("All.slnx"));
    assert_eq!(target(&["dotnet", "test", "All.SLNF"]), solution("All.SLNF"));
    assert_eq!(
        target(&["pwsh", "-File", "x.ps1", "One.csproj", "--", "dotnet", "build", "All.sln"]),
        solution("All.sln")
    );
    assert_eq!(target(&["dotnet", "build"]), None, "no project");
    assert_eq!(target(&["dotnet", "build", "src/App"]), None, "a folder names none");
    assert_eq!(
        target(&["dotnet", "build", "-p:Main=One.csproj", "/p:X=a.sln"]),
        None,
        "a switch names none"
    );
    assert_eq!(target(&["dotnet", "test", "--", "One.csproj"]), None, "a word for the program it runs");
    assert_eq!(target(&["dotnet", "build", ".csproj"]), None, "an extension alone");
    assert_eq!(target(&["cargo", "build", "One.csproj"]), None, "no dotnet command");
}

#[test]
fn a_usable_host_is_one_a_routing_file_may_hold() {
    assert!(is_usable_host("builder"));
    assert!(is_usable_host("me@builder-1.lan"));
    for host in ["", "b", "-x", "bad host", "a@b@c", "x;y"] {
        assert!(!is_usable_host(host), "{host:?}");
    }
}

#[test]
fn a_dotnet_build_is_a_dotnet_command_whose_verb_builds_without_no_build() {
    let builds = |given: &[&str]| dotnet_builds(&words(given));
    assert!(builds(&["dotnet", "build"]));
    assert!(builds(&[
        "pwsh",
        "-File",
        "x.ps1",
        "--no-build",
        "--",
        "dotnet",
        "Test",
        "--",
        "--no-build"
    ]));
    assert!(!builds(&["dotnet", "test", "--no-build"]), "--no-build");
    assert!(
        !builds(&["pwsh", "-File", "x.ps1", "--", "dotnet", "test", "--no-build"]),
        "a wrapped --no-build"
    );
    assert!(!builds(&["dotnet", "format"]), "a verb that does not build");
    assert!(!builds(&["pwsh", "-File", "x.ps1", "dotnet", "build"]), "no dotnet command");
}

#[test]
fn a_wrapped_dotnet_command_routes_as_a_dotnet_command() {
    let command = words(&["pwsh", "-File", "x.ps1", "--no-build", "--", "dotnet", "test", "--no-build", "--", "--x"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(
        run.words,
        [
            "pwsh",
            "-File",
            "x.ps1",
            "--no-build",
            "--",
            "dotnet",
            "test",
            stamp().as_str(),
            "--",
            "--x"
        ],
        "the sha stands before the dotnet command's own --"
    );
    assert_eq!(
        run.fallback,
        ["pwsh", "-File", "x.ps1", "--no-build", "--", "dotnet", "test", "--", "--x"]
    );
    assert_eq!(run.outputs, Vec::<String>::new());
    assert_eq!(run.lines(), [DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE]);
}

#[test]
fn a_repo_with_its_own_outputs_gets_them() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}, "outputs": {"Yaat": ["target/**"]}}"#).expect("usable");
    let command = words(&["cargo", "build"]);
    assert_eq!(routed(decide(&heavy(&command), &FakeFacts::with_config(config))).outputs, ["target/**"]);
}

#[test]
fn a_repo_s_own_outputs_replace_the_cargo_default() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["cargo *"]}, "outputs": {"yaat": ["target/**"]}}"#).expect("usable");
    let command = words(&["cargo", "build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["target/**"], "the repo's own list, not the cargo default");
}

#[test]
fn a_cargo_command_of_a_repo_without_outputs_brings_back_the_profile_folders() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["cargo *"]}}"#).expect("usable");
    let command = words(&["cargo", "build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"]);
}

#[test]
fn a_cargo_command_spelled_in_another_case_or_with_an_extension_gets_the_cargo_default() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}}"#).expect("usable");
    let command = words(&["Cargo.exe", "test"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"]);
}

#[test]
fn a_cargo_command_is_any_spelling_of_cargo_and_a_wrapped_one_is_not() {
    for cargo in [
        vec!["cargo", "build"],
        vec!["Cargo.exe", "test"],
        vec![r"C:\bin\cargo", "check"],
        vec!["CARGO", "fmt"],
    ] {
        let given = words(&cargo);
        assert!(is_cargo(&given), "{given:?} is a cargo command");
    }
    assert!(
        !is_cargo(&words(&["pwsh", "-File", "x.ps1", "--", "cargo", "build"])),
        "a cargo command a script is handed after its -- is not read as one"
    );
    assert!(!is_cargo(&words(&["dotnet", "build"])), "another command is not a cargo command");
}

#[test]
fn a_path_with_spaces_routes_rewritten() {
    let command = words(&["dotnet", "test", r"X:\dev\yaat.wt\a\my tests\T.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at("src/")));
    assert_eq!(run.words, ["dotnet", "test", r"..\my tests\T.csproj", stamp().as_str()]);
}

#[test]
fn a_glob_argument_is_left_alone() {
    let command = words(&["dotnet", "format", "--include", "src/**/*.cs"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at_root()));
    assert_eq!(run.words, command);
}

#[test]
fn flags_that_are_not_paths_are_untouched() {
    let command = words(&["cargo", "test", "--no-build", "-p:x=1", "/nologo", "--filter=a/b", "-j", "4"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::at("src/")));
    assert_eq!(run.words, command);
}

#[test]
fn a_bare_first_word_naming_no_file_stays_bare() {
    let command = words(&["build.cmd", "all"]);
    let facts = FakeFacts::at_root();
    assert_eq!(routed(decide(&heavy(&command), &facts)).words, ["build.cmd", "all"]);
    assert_eq!(facts.asked(), ["config", "place", "file_in_folder build.cmd"]);
}

#[test]
fn a_bare_first_word_naming_a_file_of_the_folder_becomes_dot_slash() {
    let command = words(&["build.cmd", "all"]);
    let facts = FakeFacts {
        files: words(&["build.cmd"]),
        ..FakeFacts::at_root()
    };
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, ["./build.cmd", "all"]);
    assert_eq!(run.pattern, "build.cmd*");
    assert_eq!(run.fallback, command);
}

#[test]
fn a_first_word_with_a_separator_is_never_asked_about() {
    let command = words(&["./tool", "x"]);
    let facts = FakeFacts {
        files: words(&["./tool"]),
        ..FakeFacts::at_root()
    };
    assert_eq!(routed(decide(&heavy(&command), &facts)).words, ["./tool", "x"]);
    assert_eq!(facts.asked(), ["config", "place"]);
}

#[test]
fn subst_a_drive_standing_for_a_prefix_of_the_root_is_inside() {
    let command = words(&["dotnet", "test", r"D:\yaat.wt\a\tests\T", r"e:\elsewhere\x", r"x:\dev\yaat.wt\a\y"]);
    let facts = FakeFacts {
        devices: vec![('D', r"\??\X:\dev".to_owned()), ('E', r"\??\Volume{1234}".to_owned())],
        ..FakeFacts::at_root()
    };
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some(r"gate: not routed: e:\elsewhere\x is outside the worktree")
    );
    assert_eq!(facts.asked(), ["config", "place", "subst_target D", "subst_target E"]);
}

#[test]
fn subst_a_device_that_is_no_subst_drive_adds_no_spelling() {
    let command = words(&["dotnet", "test", r"D:\yaat.wt\a\tests\T"]);
    for target in [r"\Device\HarddiskVolume3", r"\??\X:\de", ""] {
        let facts = FakeFacts {
            devices: vec![('D', target.to_owned())],
            ..FakeFacts::at_root()
        };
        let route = decide(&heavy(&command), &facts);
        assert_eq!(
            local_line(&route).as_deref(),
            Some(r"gate: not routed: D:\yaat.wt\a\tests\T is outside the worktree"),
            "{target}"
        );
    }
    let subst = FakeFacts {
        devices: vec![('D', r"\??\X:\dev".to_owned())],
        ..FakeFacts::at_root()
    };
    assert_eq!(
        routed(decide(&heavy(&command), &subst)).words,
        ["dotnet", "test", r"tests\T", stamp().as_str()]
    );
}

#[test]
fn subst_each_letter_is_asked_once_and_the_roots_own_never() {
    let command = words(&["dotnet", "test", "X:/dev/yaat.wt/a/x", "d:/a", "D:/b", "-p:o=q:/c"]);
    let facts = FakeFacts::at_root();
    let _route = decide(&heavy(&command), &facts);
    assert_eq!(facts.asked(), ["config", "place", "subst_target D", "subst_target Q"]);
}

/// A gate at the root with S: standing for `T:\a` and T: for `t_target`, when given.
fn on_chain(t_target: Option<&str>) -> FakeFacts {
    let mut devices = vec![('S', r"\??\T:\a".to_owned())];
    devices.extend(t_target.map(|target| ('T', target.to_owned())));
    FakeFacts {
        devices,
        ..FakeFacts::at_root()
    }
}

#[test]
fn subst_a_chain_of_drives_standing_for_the_root_is_followed() {
    let facts = on_chain(Some(r"\??\X:\dev\yaat.wt"));
    let command = words(&["dotnet", "test", r"S:\src\x.csproj"]);
    assert_eq!(
        routed(decide(&heavy(&command), &facts)).words,
        stamped(&["dotnet", "test", r"src\x.csproj"])
    );
    assert_eq!(
        facts.asked(),
        ["config", "place", "subst_target S", "subst_target T", "file_in_folder dotnet", "head_sha"]
    );
    let climb = words(&["dotnet", "test", r"S:\..\x"]);
    let climb_facts = on_chain(Some(r"\??\X:\dev\yaat.wt"));
    assert_eq!(
        local_line(&decide(&heavy(&climb), &climb_facts)).as_deref(),
        Some(r"gate: not routed: S:\..\x is outside the worktree")
    );
    assert_eq!(climb_facts.asked(), ["config", "place", "subst_target S", "subst_target T"]);
}

#[test]
fn subst_a_drive_a_chain_passed_through_is_asked_once() {
    let facts = on_chain(Some(r"\??\X:\dev\yaat.wt"));
    let command = words(&["dotnet", "test", r"S:\src\x.csproj", r"T:\a\y"]);
    assert_eq!(
        routed(decide(&heavy(&command), &facts)).words,
        stamped(&["dotnet", "test", r"src\x.csproj", "y"])
    );
    assert_eq!(
        facts.asked(),
        ["config", "place", "subst_target S", "subst_target T", "file_in_folder dotnet", "head_sha"]
    );
}

#[test]
fn subst_a_chain_that_loops_or_reaches_no_subst_drive_adds_no_spelling() {
    let command = words(&["dotnet", "test", r"S:\src\x.csproj"]);
    for target in [Some(r"\??\S:\b"), None, Some(r"\Device\HarddiskVolume3")] {
        let facts = on_chain(target);
        assert_eq!(
            local_line(&decide(&heavy(&command), &facts)).as_deref(),
            Some(r"gate: not routed: S:\src\x.csproj is outside the worktree"),
            "{target:?}"
        );
        assert_eq!(facts.asked(), ["config", "place", "subst_target S", "subst_target T"], "{target:?}");
    }
}

#[test]
fn a_dotnet_build_carries_the_worktree_head_as_its_source_revision() {
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::at_root();
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, ["dotnet", "build", stamp().as_str()]);
    assert_eq!(run.fallback, ["dotnet", "build"], "the fallback carries no word of the builder's");
    assert_eq!(run.lines(), Vec::<&str>::new(), "the word is said by no line");
    assert_eq!(
        facts.asked(),
        ["config", "place", "file_in_folder dotnet", "head_sha"],
        "the HEAD fact is asked for a command that gets the word"
    );
}

#[test]
fn the_source_revision_word_stands_before_the_first_double_dash() {
    let run = routed(decide(
        &heavy(&words(&["dotnet", "test", "proj.csproj", "--", "NUnit.Where=x"])),
        &FakeFacts::at_root(),
    ));
    assert_eq!(run.words, ["dotnet", "test", "proj.csproj", stamp().as_str(), "--", "NUnit.Where=x"]);
    let run = routed(decide(&heavy(&words(&["dotnet", "run", "--", "-p:x"])), &FakeFacts::at_root()));
    assert_eq!(run.words, ["dotnet", "run", stamp().as_str(), "--", "-p:x"]);
}

#[test]
fn every_dotnet_verb_that_builds_is_stamped_in_any_case() {
    let facts = FakeFacts::with_config(routing_all());
    for verb in ["build", "test", "publish", "pack", "run", "msbuild"] {
        let command = words(&["DotNet.EXE", verb]);
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.words, ["DotNet.EXE", verb, stamp().as_str()], "{verb}");
    }
    let run = routed(decide(&heavy(&words(&["dotnet", "PUBLISH"])), &facts));
    assert_eq!(run.words, ["dotnet", "PUBLISH", stamp().as_str()], "the verb's case is ignored");
    let run = routed(decide(&heavy(&words(&["DotNet.EXE", "publish", "-c", "Release"])), &facts));
    assert_eq!(run.words, ["DotNet.EXE", "publish", "-c", "Release", stamp().as_str()]);
    let run = routed(decide(&heavy(&words(&["dotnet", "msbuild", "-t:Pack"])), &facts));
    assert_eq!(run.words, ["dotnet", "msbuild", "-t:Pack", stamp().as_str()]);
}

#[test]
fn a_dotnet_command_that_does_not_build_is_left_alone() {
    let commands: [&[&str]; 10] = [
        &["dotnet", "tool", "restore"],
        &["dotnet", "--version"],
        &["dotnet", "--roll-forward", "x", "build"],
        &["dotnet", "restore"],
        &["dotnet", "clean"],
        &["dotnet", "format", "--include", "src/**/*.cs"],
        &["dotnet", "app.dll"],
        &["dotnet", "new", "console"],
        &["dotnet-ef", "migrations", "add", "x"],
        &["cmd", "/c", "dotnet", "build"],
    ];
    for command in commands {
        let facts = FakeFacts::with_config(routing_all());
        let run = routed(decide(&heavy(&words(command)), &facts));
        assert_eq!(run.words, words(command), "{command:?}");
        assert!(!asked_head(&facts), "HEAD is asked for {command:?}");
    }
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&words(&["cargo", "build"])), &facts));
    assert_eq!(run.words, ["cargo", "build"], "another build tool's command is left alone");
    assert!(!asked_head(&facts), "HEAD is asked for cargo build");
}

#[test]
fn a_command_that_already_sets_the_source_revision_is_left_alone() {
    let commands: [&[&str]; 7] = [
        &["dotnet", "build", "-p:SourceRevisionId=abc"],
        &["dotnet", "build", "/p:SourceRevisionId=abc"],
        &["dotnet", "build", "--property:sourcerevisionid=abc"],
        &["dotnet", "build", "-property:A=1;SourceRevisionId=abc"],
        &["dotnet", "build", "-p:Configuration=Release,SourceRevisionId=abc"],
        &["dotnet", "build", "-P:SourceRevisionId=1"],
        &["dotnet", "build", "/PROPERTY:sourcerevisionid=1"],
    ];
    for command in commands {
        let facts = FakeFacts::with_config(routing_all());
        let run = routed(decide(&heavy(&words(command)), &facts));
        assert_eq!(run.words, words(command), "{command:?}");
        assert!(!asked_head(&facts), "HEAD is asked for {command:?}");
    }
}

#[test]
fn a_word_that_merely_holds_the_target_name_is_no_source_revision() {
    let commands: [&[&str]; 4] = [
        &["dotnet", "build", "-p:A=1;B=SourceRevisionId"],
        &["dotnet", "build", "-p:SourceRevisionIdX=1"],
        &["dotnet", "build", "-pp:SourceRevisionId=1"],
        &["dotnet", "build", "-p:Tag=SourceRevisionId"],
    ];
    for command in commands {
        let facts = FakeFacts::with_config(routing_all());
        let run = routed(decide(&heavy(&words(command)), &facts));
        let mut expected = words(command);
        expected.push(stamp());
        assert_eq!(run.words, expected, "{command:?}");
        assert!(asked_head(&facts), "HEAD is asked for {command:?}");
    }
}

#[test]
fn a_source_revision_after_the_double_dash_does_not_count() {
    let run = routed(decide(
        &heavy(&words(&["dotnet", "run", "--", "-p:SourceRevisionId=abc"])),
        &FakeFacts::at_root(),
    ));
    assert_eq!(run.words, ["dotnet", "run", stamp().as_str(), "--", "-p:SourceRevisionId=abc"]);
}

#[test]
fn a_head_that_cannot_be_read_leaves_the_build_alone() {
    let heads: [Result<Option<String>, String>; 2] = [Ok(None), Err("git could not be started".to_owned())];
    for head in heads {
        let command = words(&["dotnet", "build"]);
        let facts = FakeFacts {
            head,
            ..FakeFacts::at_root()
        };
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.words, ["dotnet", "build"], "{:?}", facts.head);
        assert_eq!(run.fallback, ["dotnet", "build"], "{:?}", facts.head);
    }
}

#[test]
fn a_dotnet_program_named_by_an_in_worktree_path_still_builds_stamped() {
    let command = words(&[r"X:\dev\yaat.wt\a\dotnet.exe", "pack"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(routing_all())));
    assert_eq!(run.words, ["dotnet.exe", "pack", stamp().as_str()]);
}

#[test]
fn a_dotnet_program_outside_the_worktree_runs_here_with_no_word() {
    let command = words(&[r"C:\Program Files\dotnet\dotnet.exe", "pack"]);
    let facts = FakeFacts::with_config(routing_all());
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some(r"gate: not routed: C:\Program Files\dotnet\dotnet.exe is outside the worktree")
    );
    assert!(!asked_head(&facts), "no word is made for a command that runs here");
}

#[test]
fn a_missing_sibling_keeps_the_run_local_saying_so() {
    let command = words(&["dotnet", "test"]);
    let facts = FakeFacts::with_siblings("", &["b", "yaat-server", "c"], &["b"]);
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: sibling yaat-server not found beside the worktree")
    );
    assert_eq!(*facts.presence_asked.borrow(), [sibling_root("b"), sibling_root("yaat-server")]);
    assert!(!asked_head(&facts), "no word is made for a command that runs here");
}

#[test]
fn a_bad_siblings_file_keeps_the_run_local_saying_so() {
    let command = words(&["dotnet", "test"]);
    for (problem, line) in [
        ("siblings[0] is empty", "gate: not routed: .gate/siblings.toml: siblings[0] is empty"),
        (
            "line 1, column 13: invalid array\nexpected `]`",
            "gate: not routed: .gate/siblings.toml: line 1, column 13: invalid array expected `]`",
        ),
    ] {
        let facts = FakeFacts {
            siblings: Err(problem.to_owned()),
            ..FakeFacts::at_root()
        };
        let route = decide(&heavy(&command), &facts);
        assert_eq!(local_line(&route).as_deref(), Some(line), "{problem:?}");
        assert!(facts.presence_asked.borrow().is_empty(), "no sibling is looked for");
    }
}

#[test]
fn a_leaf_that_cannot_be_laid_out_keeps_a_run_with_siblings_local() {
    let command = words(&["dotnet", "test"]);
    let in_wt = |root: &str, names: &[&str]| FakeFacts {
        place: Ok(Some(Place {
            root: root.to_owned(),
            prefix: String::new(),
            checkout: "yaat".to_owned(),
            folder: root.to_owned(),
        })),
        siblings: Ok(words(names)),
        present: vec!["X:/dev/yaat.wt/b".to_owned()],
        ..FakeFacts::at_root()
    };
    for (root, leaf) in [
        ("X:/dev/yaat.wt/my wt", "my wt"),
        ("X:/dev/yaat.wt/ü", "ü"),
        ("X:/", "X:"),
        ("X:/dev/yaat.wt/a.", "a."),
        ("X:/dev/yaat.wt/Nul.x", "Nul.x"),
    ] {
        let route = decide(&heavy(&command), &in_wt(root, &["b"]));
        assert_eq!(
            local_line(&route),
            Some(format!(
                "gate: not routed: the worktree folder name {leaf} cannot be laid out on the builder"
            )),
            "{root}"
        );
    }
    let run = routed(decide(&heavy(&command), &in_wt("X:/dev/yaat.wt/my wt", &[])));
    assert_eq!(run.leaf, "my wt", "a repo with no siblings is unaffected by the leaf rule");
}

#[test]
fn no_siblings_changes_nothing() {
    let command = words(&["dotnet", "test", "../yaat-server/S.csproj"]);
    let facts = FakeFacts::at_root();
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: ../yaat-server/S.csproj is outside the worktree")
    );
    assert_eq!(facts.siblings_read.get(), 1);
    assert!(facts.presence_asked.borrow().is_empty());
    let run = routed(decide(&heavy(&words(&["dotnet", "test"])), &FakeFacts::at_root()));
    assert_eq!(run.lines(), Vec::<String>::new(), "no line names siblings");
    assert_eq!((run.leaf.as_str(), run.siblings), ("a", Vec::new()));
    let unallowed = FakeFacts::with_config(Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}, "siblings": 5}"#).expect("usable"));
    let run = routed(decide(&heavy(&words(&["dotnet", "test", "--no-build"])), &unallowed));
    assert_eq!(
        run.lines(),
        [DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE],
        "a bad siblings value changes nothing for a repo declaring none"
    );
}

/// A gate at the root declaring `declared`, all beside the root, under a routing file whose `siblings` value is
/// `siblings` (no key at all for `None`).
fn consenting(siblings: Option<&str>, declared: &[&str]) -> FakeFacts {
    let key = siblings.map_or_else(String::new, |value| format!(r#", "siblings": {value}"#));
    let config = Config::parse(&format!(r#"{{"host": "builder-1", "repos": {{"yaat": ["*"]}}{key}}}"#)).expect("usable");
    FakeFacts {
        config: Ok(Some(config)),
        ..FakeFacts::with_siblings("", declared, declared)
    }
}

#[test]
fn only_the_siblings_the_routing_file_allows_are_carried() {
    let command = words(&["dotnet", "test"]);
    for (case, siblings, declared, line) in [
        ("no key", None, &["b"][..], "gate: not routed: sibling b is not allowed in remote.json"),
        (
            "no entry",
            Some(r#"{"other": ["b"]}"#),
            &["b"],
            "gate: not routed: sibling b is not allowed in remote.json",
        ),
        (
            "an empty list",
            Some(r#"{"yaat": []}"#),
            &["b"],
            "gate: not routed: sibling b is not allowed in remote.json",
        ),
        (
            "another name",
            Some(r#"{"yaat": ["c"]}"#),
            &["b"],
            "gate: not routed: sibling b is not allowed in remote.json",
        ),
        (
            "another case",
            Some(r#"{"yaat": ["B"]}"#),
            &["b"],
            "gate: not routed: sibling b is not allowed in remote.json",
        ),
        (
            "several refused",
            Some(r#"{"yaat": ["c"]}"#),
            &["b", "c", "yaat-server"],
            "gate: not routed: siblings b, yaat-server are not allowed in remote.json",
        ),
        (
            "not an object",
            Some(r#"["b"]"#),
            &["b"],
            "gate: not routed: sibling b is not allowed in remote.json: siblings must be a JSON object",
        ),
        (
            "a bad entry",
            Some(r#"{"yaat": ["b", 3]}"#),
            &["b", "c"],
            "gate: not routed: siblings b, c are not allowed in remote.json: siblings.\"yaat\"[1] is not a non-empty string",
        ),
        (
            "refused before the root's own name",
            Some(r#"{"yaat": ["b"]}"#),
            &["b", "A"],
            "gate: not routed: sibling A is not allowed in remote.json",
        ),
    ] {
        let facts = consenting(siblings, declared);
        assert_eq!(local_line(&decide(&heavy(&command), &facts)).as_deref(), Some(line), "{case}");
        assert!(facts.presence_asked.borrow().is_empty(), "{case}: no sibling is looked for");
        assert_eq!(facts.siblings_read.get(), 1, "{case}");
    }
    let facts = consenting(Some(r#"{"YAAT": ["yaat-server", "b", "c"]}"#), &["b"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.siblings, [carried("b")], "only the declared sibling is carried");
    assert_eq!(*facts.presence_asked.borrow(), [sibling_root("b")]);
}

#[test]
fn a_declared_name_that_is_no_folder_name_never_reaches_the_folder_check() {
    let command = words(&["dotnet", "test"]);
    for name in [r"x\y", "x/y", "..", "c:"] {
        let allowed = format!(r#"{{"yaat": ["b", "{}"]}}"#, name.replace('\\', r"\\"));
        let facts = consenting(Some(&allowed), &["b", name]);
        assert_eq!(
            local_line(&decide(&heavy(&command), &facts)),
            Some(format!(
                "gate: not routed: .gate/siblings.toml: siblings[1] is not a folder name the builder can lay out: {name:?}"
            )),
            "{name:?} keeps the run here, saying why, whatever remote.json allows"
        );
        assert!(facts.presence_asked.borrow().is_empty(), "{name:?}: no folder is looked for");
    }
}

#[test]
fn a_siblings_file_problem_is_shown_with_its_marks_escaped() {
    let facts = FakeFacts {
        siblings: Err("line 1:\r\n  bad\u{1b}[31m \u{202e}key".to_owned()),
        ..consenting(Some(r#"{"yaat": ["b"]}"#), &["b"])
    };
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test"])), &facts)).as_deref(),
        Some(r"gate: not routed: .gate/siblings.toml: line 1: bad\u{1b}[31m \u{202e}key"),
        "line breaks folded, then every control character and reordering mark escaped"
    );
}

#[test]
fn a_word_into_an_allowed_sibling_that_is_not_declared_is_outside() {
    let facts = consenting(Some(r#"{"yaat": ["yaat-server", "b"]}"#), &["yaat-server"]);
    let route = decide(&heavy(&words(&["dotnet", "test", r"..\b\x"])), &facts);
    assert_eq!(local_line(&route).as_deref(), Some(r"gate: not routed: ..\b\x is outside the worktree"));
    assert_eq!(facts.siblings_read.get(), 1, "the declaration is read once");
}

#[test]
fn an_unlisted_repo_never_reads_the_siblings_file() {
    let command = words(&["dotnet", "test"]);
    let declaring = || FakeFacts::with_siblings("", &["yaat-server"], &["yaat-server"]);
    let other_repo = FakeFacts {
        place: Ok(Some(place("", "other"))),
        ..declaring()
    };
    assert_eq!(decide(&heavy(&command), &other_repo), SILENT);
    assert_eq!(other_repo.siblings_read.get(), 0);
    for (case, inputs) in [
        (
            "light",
            Inputs {
                slot: Slot::Light,
                ..heavy(&command)
            },
        ),
        (
            "local",
            Inputs {
                local: true,
                ..heavy(&command)
            },
        ),
        (
            "hooked",
            Inputs {
                git_hook: true,
                ..heavy(&command)
            },
        ),
        (
            "covered",
            Inputs {
                covering_parent: true,
                ..heavy(&command)
            },
        ),
    ] {
        let facts = declaring();
        assert!(matches!(decide(&inputs, &facts), Route::Local { .. }), "{case}");
        assert_eq!(facts.siblings_read.get(), 0, "{case}");
    }
    let listed = declaring();
    assert_eq!(decide(&heavy(&words(&["node", "x"])), &listed), SILENT, "no pattern matches");
    assert_eq!(
        listed.siblings_read.get(),
        1,
        "a listed repo reads it once, before the rewrite and the match"
    );
    let facts = declaring();
    routed(decide(&heavy(&command), &facts));
    assert_eq!(facts.siblings_read.get(), 1, "a matched command reads it once");
}

#[test]
fn an_unmatched_command_says_nothing_of_the_siblings() {
    let broken = || FakeFacts {
        siblings: Err("line 1, column 13: invalid array".to_owned()),
        ..FakeFacts::at_root()
    };
    let unmatched = words(&["node", "x"]);
    let missing = FakeFacts::with_siblings("", &["yaat-server"], &[]);
    assert_eq!(decide(&heavy(&unmatched), &missing), SILENT, "a missing sibling");
    assert!(missing.presence_asked.borrow().is_empty(), "no sibling is looked for");
    let unreadable = broken();
    assert_eq!(decide(&heavy(&unmatched), &unreadable), SILENT, "a broken file");
    assert_eq!(unreadable.siblings_read.get(), 1, "the file is read once all the same");

    let matched = words(&["dotnet", "test"]);
    let missing = FakeFacts::with_siblings("", &["yaat-server"], &[]);
    assert_eq!(
        local_line(&decide(&heavy(&matched), &missing)).as_deref(),
        Some("gate: not routed: sibling yaat-server not found beside the worktree")
    );
    assert_eq!(
        local_line(&decide(&heavy(&matched), &broken())).as_deref(),
        Some("gate: not routed: .gate/siblings.toml: line 1, column 13: invalid array")
    );
}

#[test]
fn a_sibling_named_like_the_worktree_keeps_the_run_local_saying_so() {
    let facts = FakeFacts::with_siblings("", &["b", "A"], &["b", "A"]);
    let route = decide(&heavy(&words(&["dotnet", "test"])), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: .gate/siblings.toml: siblings[1] is the worktree's own folder: \"A\"")
    );
    assert!(facts.presence_asked.borrow().is_empty(), "no sibling is looked for");
}

#[test]
fn a_relative_climb_past_the_subst_drive_root_the_gate_is_spelled_on_is_outside() {
    // A gate at `prefix`, its folder spelled `folder` on S:, which stands for `target`, declaring yaat-server beside
    // the root.
    let on_s = |prefix: &str, folder: &str, target: &str| FakeFacts {
        place: Ok(Some(Place {
            folder: folder.to_owned(),
            ..place_at(prefix)
        })),
        devices: vec![('S', format!(r"\??\{target}"))],
        ..FakeFacts::with_siblings(prefix, &["yaat-server"], &["yaat-server"])
    };
    let root = r"X:\dev\yaat.wt\a";
    for (prefix, folder, target, word) in [
        ("", r"S:\", root, r"..\yaat-server\src\x.csproj"),
        ("crates/", r"S:\crates", root, r"..\..\yaat-server\x"),
        ("crates/", r"S:\", r"X:\dev\yaat.wt\a\crates", r"..\src\x"),
    ] {
        let facts = on_s(prefix, folder, target);
        let route = decide(&heavy(&words(&["dotnet", "test", word])), &facts);
        assert_eq!(
            local_line(&route),
            Some(format!("gate: not routed: {word} is outside the worktree")),
            "{word:?} from {folder:?}: Windows stops `..` at the drive's root"
        );
        assert!(
            !facts.asked().contains(&"subst_target S".to_owned()),
            "the folder's spelling needs no lookup"
        );
    }
    let inside = routed(decide(
        &heavy(&words(&["dotnet", "test", r"..\src\x"])),
        &on_s("crates/", r"S:\crates", root),
    ));
    assert_eq!(inside.words, stamped(&["dotnet", "test", r"..\src\x"]), "a climb that stays on the drive");
    let real = routed(decide(
        &heavy(&words(&["dotnet", "test", r"..\..\yaat-server\x"])),
        &FakeFacts::with_siblings("crates/", &["yaat-server"], &["yaat-server"]),
    ));
    assert_eq!(
        real.words,
        stamped(&["dotnet", "test", r"..\..\yaat-server\x"]),
        "a folder on the root's real spelling climbs into the sibling"
    );
}

#[test]
fn remote_run_carries_the_leaf_and_each_sibling_root() {
    let command = words(&["dotnet", "test", "X:/dev/yaat.wt/yaat-server/S.csproj", "-p:Other=../../b/x"]);
    let facts = FakeFacts::with_siblings("src/", &["yaat-server", "b"], &["yaat-server", "b"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.leaf, "a");
    assert_eq!(run.siblings, [carried("yaat-server"), carried("b")]);
    assert_eq!(
        run.lines(),
        ["gate: routed with siblings: yaat-server (X:/dev/yaat.wt/yaat-server), b (X:/dev/yaat.wt/b)"]
    );
    assert_eq!(
        run.words,
        stamped(&["dotnet", "test", "../../yaat-server/S.csproj", "-p:Other=../../b/x"])
    );
    assert_eq!(run.fallback, command);
}

/// A gate at the root declaring `b`, `c` and `yaat-server`, all present, with the folder check giving `folders` for
/// some of their roots.
fn checked(folders: Vec<(String, Result<SiblingFolder, String>)>) -> FakeFacts {
    FakeFacts {
        folders,
        ..FakeFacts::with_siblings("", &["b", "c", "yaat-server"], &["b", "c", "yaat-server"])
    }
}

#[test]
fn a_refused_sibling_folder_keeps_the_run_local_and_asks_no_later_one() {
    let refused = checked(vec![(
        sibling_root("c"),
        Ok(SiblingFolder::Refused("is a link or a junction".to_owned())),
    )]);
    let route = decide(&heavy(&words(&["dotnet", "test"])), &refused);
    assert_eq!(local_line(&route).as_deref(), Some("gate: not routed: sibling c is a link or a junction"));
    assert_eq!(*refused.presence_asked.borrow(), [sibling_root("b"), sibling_root("c")]);
}

#[test]
fn a_sibling_folder_that_cannot_be_read_keeps_the_run_local_saying_what() {
    let unreadable = checked(vec![(
        sibling_root("c"),
        Err("cannot read the sibling folder X:/dev/yaat.wt/c: access denied\n(os error 5)".to_owned()),
    )]);
    let route = decide(&heavy(&words(&["dotnet", "test"])), &unreadable);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: cannot read the sibling folder X:/dev/yaat.wt/c: access denied (os error 5)"),
        "never routed with the second sibling dropped"
    );
    assert_eq!(*unreadable.presence_asked.borrow(), [sibling_root("b"), sibling_root("c")]);
    let no_root = FakeFacts {
        root_id: Err("cannot read the worktree folder's id: access denied".to_owned()),
        ..checked(Vec::new())
    };
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test"])), &no_root)).as_deref(),
        Some("gate: not routed: cannot read the worktree folder's id: access denied")
    );
}

#[test]
fn a_single_repo_root_whose_id_cannot_be_read_stays_local() {
    let no_root = FakeFacts {
        root_id: Err("cannot read the worktree folder's id: access denied".to_owned()),
        ..FakeFacts::at_root()
    };
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test"])), &no_root)).as_deref(),
        Some("gate: not routed: cannot read the worktree folder's id: access denied"),
        "a run that declares no sibling still needs its root's id for the sync-back's lock"
    );
}

#[test]
fn a_sibling_folder_that_is_the_root_or_another_sibling_keeps_the_run_local() {
    let command = words(&["dotnet", "test"]);
    let same_as_root = checked(vec![(sibling_root("c"), Ok(SiblingFolder::Present(fake_id(ROOT))))]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &same_as_root)).as_deref(),
        Some("gate: not routed: sibling c is the worktree's own folder")
    );
    let same_as_b = checked(vec![(
        sibling_root("yaat-server"),
        Ok(SiblingFolder::Present(fake_id(&sibling_root("b")))),
    )]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &same_as_b)).as_deref(),
        Some("gate: not routed: sibling yaat-server is the same folder as sibling b")
    );
    let distinct = checked(Vec::new());
    let run = routed(decide(&heavy(&command), &distinct));
    assert_eq!(run.siblings, [carried("b"), carried("c"), carried("yaat-server")]);
}
