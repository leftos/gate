//! The routing decision: each rule in its order, the cases where the rewrite and the bare first word differ from the
//! PowerShell gate, the subst-drive table, and which facts are asked when. Raising `--stall` for a remote run is the
//! remote client's, not the router's.

use std::path::PathBuf;

use gate_router::{
    Config, ConfigProblem, DROPPED_NO_BUILD_LINE, Inputs, LocalReason, NO_OUTPUTS_LINE, Place, RemoteRun, Route, Slot, decide, work_folder_key,
};

use crate::support::{FakeFacts, HEAD, ROOT, heavy, words};

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
        root: ROOT.to_owned(),
        prefix: prefix.to_owned(),
        checkout: checkout.to_owned(),
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

#[test]
fn a_repo_with_its_own_outputs_gets_them() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}, "outputs": {"Yaat": ["target/**"]}}"#).expect("usable");
    let command = words(&["cargo", "build"]);
    assert_eq!(routed(decide(&heavy(&command), &FakeFacts::with_config(config))).outputs, ["target/**"]);
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
