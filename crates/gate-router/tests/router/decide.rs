//! The routing decision: each rule in its order, the cases where the rewrite and the bare first word differ from the
//! PowerShell gate, the subst-drive table, and which facts are asked when. Raising `--stall` for a remote run is the
//! remote client's, not the router's.

use std::path::PathBuf;

use gate_router::windows::WindowsChange;
use gate_router::{
    BuildSettings, BuildTarget, Builder, BuilderOs, CARRIED_INPUT, Config, ConfigProblem, DROPPED_NO_BUILD_LINE, DotnetTarget, Facts,
    GitignoredInput, Inputs, LocalReason, NO_OUTPUTS_LINE, PROJECT_EXTENSIONS, Pattern, PinRefusal, Place, RemoteRun, Route, SiblingFolder, Slot,
    Target, build_settings, builder_words, decide, dotnet_builds, dotnet_target, is_cargo, is_dotnet, is_usable_host, later_run, sdk_build_target,
    target, without_no_build, work_folder_key,
};

use crate::support::{FakeFacts, HEAD, ROOT, carried, fake_id, heavy, place_at, sibling_root, words};

const SILENT: Route = Route::Local { reason: None };

fn routed(route: Route) -> RemoteRun {
    assert!(matches!(route, Route::Routed(_)), "the command runs here: {:?}", local_line(&route));
    match route {
        Route::Routed(run) => *run,
        Route::Local { .. } | Route::Refused(_) => unreachable!("asserted to route"),
    }
}

fn local_line(route: &Route) -> Option<String> {
    match route {
        Route::Local { reason } => reason.as_ref().map(LocalReason::line),
        Route::Refused(refusal) => Some(refusal.line()),
        Route::Routed(_) => Some("routed".to_owned()),
    }
}

fn place(prefix: &str, checkout: &str) -> Place {
    Place {
        checkout: checkout.to_owned(),
        ..place_at(prefix)
    }
}

/// The top-level `host` of a routing file as the builder a run goes to: Windows, the default root, no id.
fn host_builder(alias: &str, slots: u32) -> Builder {
    Builder {
        alias: alias.to_owned(),
        os: BuilderOs::Windows,
        root: None,
        slots,
        id: None,
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

/// How many times the decision asked for the worktree's HEAD: once for every routed run, none for one that runs here.
fn head_asks(facts: &FakeFacts) -> usize {
    facts.asked().iter().filter(|question| question.as_str() == "head_sha").count()
}

#[test]
fn the_control_command_routes_from_the_root() {
    let command = words(&["dotnet", "test", "X:/dev/yaat.wt/a/tests/T.csproj"]);
    let facts = FakeFacts::at_root();
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(
        run,
        RemoteRun {
            builder: host_builder("builder-1", 2),
            root: ROOT.to_owned(),
            root_id: fake_id(ROOT),
            leaf: "a".to_owned(),
            siblings: Vec::new(),
            repo: "yaat".to_owned(),
            key: work_folder_key(ROOT),
            subfolder: String::new(),
            pattern: "dotnet *".to_owned(),
            words: stamped(&["dotnet", "test", "tests/T.csproj"]),
            fallback: command.clone(),
            program_named: false,
            outputs: words(&["**/bin/**", "**/.godot/mono/temp/bin/**"]),
            inputs: Vec::new(),
            dropped_no_build: false,
            send: None,
            outputs_withheld: false,
            desktop: false,
            source_revision: Some(HEAD.to_owned()),
            later: Vec::new(),
        }
    );
    assert_eq!(run.lines(), Vec::<&str>::new());
    assert_eq!(facts.asked(), ["config", "place", "file_in_folder dotnet", "head_sha"]);
    assert_eq!(facts.desktop_read.get(), 1, "a routed command reads the desktop file once");
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

/// A gate at the root of `yaat` under a routing file routing `dotnet *` whose `hooks` value is `hooks` (no key at all
/// for `None`).
fn hooks_facts(hooks: Option<&str>) -> FakeFacts {
    let key = hooks.map_or_else(String::new, |value| format!(r#", "hooks": {value}"#));
    let config = Config::parse(&format!(r#"{{"host": "builder-1", "slots": 2, "repos": {{"yaat": ["dotnet *"]}}{key}}}"#)).expect("usable");
    FakeFacts::with_config(config)
}

#[test]
fn a_hook_run_in_a_consented_repo_routes() {
    let command = words(&["dotnet", "test"]);
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    let facts = hooks_facts(Some(r#"["Yaat"]"#));
    let run = routed(decide(&inputs, &facts));

    assert_eq!(run.repo, "yaat");
    assert_eq!(run.pattern, "dotnet *", "the repo's usual pattern match");
    assert_eq!(facts.asked(), ["config", "place", "file_in_folder dotnet", "head_sha"]);
}

#[test]
fn a_hook_run_in_a_repo_hooks_does_not_name_stays_local_silently() {
    let command = words(&["dotnet", "test"]);
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    let facts = hooks_facts(Some(r#"["other"]"#));
    let route = decide(&inputs, &facts);

    assert_eq!(route, SILENT);
    assert_eq!(local_line(&route), None, "no line");
    assert_eq!(facts.asked(), ["config", "place"], "the repo is read, nothing more");
    assert_eq!(facts.siblings_read.get(), 0, "no siblings file is read for a repo hooks does not name");
}

#[test]
fn a_hook_run_whose_place_cannot_be_read_stays_local_silently() {
    let command = words(&["dotnet", "test"]);
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    let facts = FakeFacts {
        place: Err("git rev-parse --show-toplevel failed".to_owned()),
        ..hooks_facts(Some(r#"["yaat"]"#))
    };

    assert_eq!(decide(&inputs, &facts), SILENT);
    assert_eq!(facts.asked(), ["config", "place"], "no git fact is asked after the failed place");
}

#[test]
fn a_hook_run_asks_git_nothing_more_when_hooks_names_no_repo() {
    let command = words(&["dotnet", "test"]);
    let inputs = Inputs {
        git_hook: true,
        ..heavy(&command)
    };
    for hooks in [None, Some("[]"), Some(r#""yaat""#), Some(r#"[""]"#)] {
        let facts = hooks_facts(hooks);
        assert_eq!(decide(&inputs, &facts), SILENT, "{hooks:?}");
        assert_eq!(facts.asked(), ["config"], "{hooks:?}");
        assert_eq!(facts.siblings_read.get(), 0, "{hooks:?}");
    }
}

#[test]
fn a_consented_hook_run_whose_first_word_is_git_or_prek_stays_local() {
    for command in [&["git", "commit"][..], &["prek", "run"]] {
        let words = words(command);
        let inputs = Inputs {
            git_hook: true,
            ..heavy(&words)
        };
        let facts = hooks_facts(Some(r#"["yaat"]"#));
        assert_eq!(decide(&inputs, &facts), SILENT, "{command:?}");
        assert_eq!(facts.asked(), ["config"], "{command:?}");
    }
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
    // A path sent as a program's name is read the same way: a `git` or `prek` the rewrite would send stays here, before
    // any pattern is asked.
    for command in [[r"C:\x\git.exe.exe", "status"], [r"C:\x\prek.exe.exe", "run"]] {
        let command = words(&command);
        let facts = FakeFacts::with_config(routing_all());
        assert_eq!(decide(&heavy(&command), &facts), SILENT, "{command:?}");
        assert_eq!(facts.asked(), ["config", "place", "subst_target C"], "{command:?}");
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

/// A gate at the root of [`ROOT`] whose folders `trees` hold a `.git`, each a worktree of `yaat`, which the routing
/// file lists.
fn with_trees(trees: &[&str]) -> FakeFacts {
    FakeFacts {
        trees: trees.iter().map(PathBuf::from).collect(),
        checkouts: trees.iter().map(|tree| (PathBuf::from(tree), "yaat".to_owned())).collect(),
        ..FakeFacts::at_root()
    }
}

#[test]
fn a_matched_command_in_another_worktree_says_to_cd_into_it() {
    let other = r"X:\dev\yaat.wt\yaat-780";
    let line = format!("gate: not routed: the command works in {other}; run it with --cd {other}");
    let command = words(&["dotnet", "test", r"X:\dev\yaat.wt\yaat-780\src\x.csproj"]);
    let facts = with_trees(&[r"X:\dev", other]);
    assert_eq!(local_line(&decide(&heavy(&command), &facts)), Some(line.clone()), "the nearest tree");
    assert_eq!(*facts.trees_asked.borrow(), [PathBuf::from(r"X:\dev\yaat.wt\yaat-780\src\x.csproj")]);

    let climb = words(&["dotnet", "test", "../yaat-780/src/x.csproj"]);
    assert_eq!(
        local_line(&decide(&heavy(&climb), &with_trees(&[other]))),
        Some(line),
        "a climb out of the root, read from the gate's folder"
    );

    let spaced = words(&["dotnet", "test", r"X:\dev\my repo.wt\b\x.csproj"]);
    assert_eq!(
        local_line(&decide(&heavy(&spaced), &with_trees(&[r"X:\dev\my repo.wt\b"]))).as_deref(),
        Some(r#"gate: not routed: the command works in "X:\dev\my repo.wt\b"; run it with --cd "X:\dev\my repo.wt\b""#),
        "a tree with a space is quoted"
    );
    // A word holding most of these is a script's text, which names no path; a subst drive standing for the tree does.
    let command = words(&["dotnet", "test", r"S:\src\x.csproj"]);
    for tree in [
        r"X:\dev\a;b",
        r"X:\dev\a&b",
        r"X:\dev\a'b",
        r"X:\dev\a(b)",
        r"X:\dev\a,b",
        r"X:\dev\a=b",
        r"X:\dev\a$b",
        r"X:\dev\a`b",
    ] {
        let facts = FakeFacts {
            devices: vec![('S', format!(r"\??\{tree}"))],
            ..with_trees(&[tree])
        };
        assert_eq!(
            local_line(&decide(&heavy(&command), &facts)),
            Some(format!(r#"gate: not routed: the command works in "{tree}"; run it with --cd "{tree}""#)),
            "a tree with a character a shell reads apart is quoted"
        );
    }
}

#[test]
fn an_outside_word_in_no_worktree_keeps_the_outside_line() {
    let command = words(&["dotnet", "test", r"X:\dev\other\x.csproj"]);
    let facts = with_trees(&[]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &facts)).as_deref(),
        Some(r"gate: not routed: X:\dev\other\x.csproj is outside the worktree")
    );
    assert_eq!(*facts.trees_asked.borrow(), [PathBuf::from(r"X:\dev\other\x.csproj")]);

    let share = words(&["dotnet", "test", r"\\host\share\x.csproj"]);
    let anywhere = with_trees(&[r"\\host\share", r"X:\"]);
    assert_eq!(
        local_line(&decide(&heavy(&share), &anywhere)).as_deref(),
        Some(r"gate: not routed: \\host\share\x.csproj is outside the worktree"),
        "a share path names no path to look up"
    );
    assert!(anywhere.trees_asked.borrow().is_empty(), "{:?}", anywhere.trees_asked);
}

#[test]
fn an_outside_path_in_a_declared_sibling_is_not_told_to_cd() {
    // A path climbing out of a subst drive standing for a root stays where Windows stops it, at the drive's root: the
    // rewrite judges it outside, and the tree it lies in is the run's own or a sibling's.
    let command = words(&["dotnet", "test", r"S:\..\x\y.csproj"]);
    for (stands_for, tree) in [
        (r"\??\X:\dev\yaat.wt\b", r"X:\dev\yaat.wt\b"),
        (r"\??\X:\dev\yaat.wt\a", r"X:\dev\yaat.wt\a"),
    ] {
        let facts = FakeFacts {
            devices: vec![('S', stands_for.to_owned())],
            trees: vec![PathBuf::from(tree)],
            checkouts: vec![(PathBuf::from(tree), "yaat".to_owned())],
            ..FakeFacts::with_siblings("", &["b"], &["b"])
        };
        assert_eq!(
            local_line(&decide(&heavy(&command), &facts)).as_deref(),
            Some(r"gate: not routed: S:\..\x\y.csproj is outside the worktree"),
            "{tree}"
        );
        assert_eq!(*facts.trees_asked.borrow(), [PathBuf::from(tree).join(r"x\y.csproj")], "{tree}");
    }
}

#[test]
fn a_command_in_another_worktree_of_an_unlisted_repo_keeps_the_outside_line() {
    let command = words(&["dotnet", "test", r"X:\dev\tools\x.csproj"]);
    let line = Some(r"gate: not routed: X:\dev\tools\x.csproj is outside the worktree");
    let listed = with_trees(&[r"X:\dev\tools"]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &listed)).as_deref(),
        Some(r"gate: not routed: the command works in X:\dev\tools; run it with --cd X:\dev\tools"),
        "a tree of a listed repo"
    );
    for (checkouts, why) in [
        (vec![(PathBuf::from(r"X:\dev\tools"), "tools".to_owned())], "unlisted"),
        (Vec::new(), "unreadable"),
    ] {
        let facts = FakeFacts {
            checkouts,
            ..with_trees(&[r"X:\dev\tools"])
        };
        assert_eq!(local_line(&decide(&heavy(&command), &facts)).as_deref(), line, "{why}");
    }
}

#[test]
fn another_spelling_of_the_own_root_is_not_told_to_cd() {
    // A junction to the root: another spelling, the root's folder id.
    let command = words(&["dotnet", "test", r"X:\links\yaat-a\src\x.csproj"]);
    let junction = FakeFacts {
        ids: vec![(r"X:\links\yaat-a".to_owned(), Ok(fake_id(ROOT)))],
        ..with_trees(&[r"X:\links\yaat-a"])
    };
    assert_eq!(
        local_line(&decide(&heavy(&command), &junction)).as_deref(),
        Some(r"gate: not routed: X:\links\yaat-a\src\x.csproj is outside the worktree")
    );

    // A tree whose id cannot be read is compared by its spelling: another letter case and a trailing separator.
    let climb = words(&["dotnet", "test", r"S:\..\x\y.csproj"]);
    let spelled = r"x:\DEV\yaat.wt\A\";
    let facts = FakeFacts {
        devices: vec![('S', r"\??\x:\DEV\yaat.wt\A".to_owned())],
        ids: vec![(spelled.to_owned(), Err("access denied".to_owned()))],
        ..with_trees(&[spelled])
    };
    assert_eq!(
        local_line(&decide(&heavy(&climb), &facts)).as_deref(),
        Some(r"gate: not routed: S:\..\x\y.csproj is outside the worktree")
    );
    assert_eq!(*facts.trees_asked.borrow(), [PathBuf::from(r"x:\DEV\yaat.wt\A\x\y.csproj")]);
    let elsewhere = FakeFacts {
        devices: vec![('S', r"\??\x:\DEV\yaat.wt\B".to_owned())],
        ids: vec![(r"x:\DEV\yaat.wt\B".to_owned(), Err("access denied".to_owned()))],
        ..with_trees(&[r"x:\DEV\yaat.wt\B"])
    };
    assert_eq!(
        local_line(&decide(&heavy(&climb), &elsewhere)).as_deref(),
        Some(r"gate: not routed: the command works in x:\DEV\yaat.wt\B; run it with --cd x:\DEV\yaat.wt\B"),
        "a spelling of no own root"
    );
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

/// The facts of a gate at the root whose routing file routes `yaat`'s dotnet commands and, when `send` holds it, lets it
/// send the PC's build (`sendBuild`).
fn send_facts(send: &str) -> FakeFacts {
    let text = format!(r#"{{"host": "builder-1", "repos": {{"yaat": ["dotnet *"]}}, "sendBuild": {send}}}"#);
    FakeFacts::with_config(Config::parse(&text).expect("usable"))
}

#[test]
fn a_no_build_test_of_a_repo_send_build_does_not_list_has_no_send_plan() {
    let command = words(&["dotnet", "test", "App.csproj", "-c", "Release", "--no-build"]);
    for send in ["[]", r#"["other"]"#, r#""yaat""#, "[3]"] {
        let facts = send_facts(send);
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.send, None, "{send}");
        assert!(run.dropped_no_build, "{send}");
        assert!(run.lines().iter().any(|line| line == DROPPED_NO_BUILD_LINE), "today's line: {send}");
        assert!(
            !facts.asked().iter().any(|asked| asked == "moves_build_output"),
            "no build file read: {send}"
        );
    }
}

#[test]
fn a_no_build_test_of_one_project_carries_both_word_sets_and_a_send_plan() {
    let command = words(&["dotnet", "test", "App.csproj", "-c", "Release", "--no-build"]);
    let run = routed(decide(&heavy(&command), &send_facts(r#"["Yaat"]"#)));
    assert_eq!(
        run.words,
        ["dotnet", "test", "App.csproj", "-c", "Release", stamp().as_str()],
        "dropped and stamped"
    );
    let send = run.send.as_ref().expect("the run has a send plan");
    assert_eq!(send.words, command, "kept and never stamped");
    assert_eq!(send.bin, "bin/Release");
    assert!(run.dropped_no_build);
    assert!(
        !run.lines().iter().any(|line| line == DROPPED_NO_BUILD_LINE),
        "the client says the dropped line only when the send does not land: {:?}",
        run.lines()
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

/// The target and the words a worktree's [`target`] and [`builder_words`] give for `command`, each checked against the
/// [`RemoteRun`] the same worktree's decision builds.
fn assert_builder_words_match(command: &[String], facts: &FakeFacts) -> (Target, Vec<String>) {
    let run = routed(decide(&heavy(command), facts));
    let place = facts.place().expect("the place reads").expect("there is a worktree");
    let builder_target = target(&crate::support::config(), place, facts).expect("the worktree routes");
    let built = builder_words(&builder_target, command, facts);
    assert_eq!(built.words, run.words, "{command:?}");
    assert_eq!(built.outside, None, "{command:?}");
    assert_eq!(builder_target.builder, run.builder, "{command:?}");
    assert_eq!(builder_target.key, run.key, "{command:?}");
    assert_eq!(builder_target.repo, run.repo, "{command:?}");
    assert_eq!(builder_target.leaf, run.leaf, "{command:?}");
    assert_eq!(builder_target.root_id, run.root_id, "{command:?}");
    assert_eq!(builder_target.place.root, run.root, "{command:?}");
    assert_eq!(builder_target.siblings, run.siblings, "{command:?}");
    assert_eq!(builder_target.inputs, run.inputs, "{command:?}");
    (builder_target, built.words)
}

/// The file `file` under the sibling root `name`, as the fixtures spell that root ([`sibling_root`]).
fn sibling_path(name: &str, file: &str) -> String {
    format!("{}/{}", sibling_root(name), file)
}

#[test]
fn builder_words_match_the_routed_run_for_the_same_worktree() {
    let command = words(&["dotnet", "test", "--no-build"]);
    let (_, built) = assert_builder_words_match(&command, &FakeFacts::at_root());
    assert!(built.contains(&stamp()), "the worktree's sha is pinned: {built:?}");
    assert!(!built.iter().any(|word| word == "--no-build"), "the drop is the routed run's: {built:?}");

    let command = words(&["tool", "--x"]);
    let facts = FakeFacts {
        files: words(&["tool"]),
        ..FakeFacts::at_root()
    };
    let (_, built) = assert_builder_words_match(&command, &facts);
    assert_eq!(
        built.first().map(String::as_str),
        Some("./tool"),
        "the gate's own file is marked: {built:?}"
    );

    let command = words(&["dotnet", "build", &sibling_path("b", "One.csproj"), &sibling_path("c", "Two.csproj")]);
    let facts = FakeFacts::with_siblings("sub/", &["b", "c"], &["b", "c"]);
    let (builder_target, built) = assert_builder_words_match(&command, &facts);
    assert_eq!(
        builder_target.siblings,
        [carried("b"), carried("c")],
        "both siblings are carried, in declared order"
    );
    assert!(built.contains(&stamp()), "the worktree's sha is pinned: {built:?}");
    assert_eq!(
        &built[2..4],
        ["../../b/One.csproj", "../../c/Two.csproj"],
        "each sibling's path is sent with its declared name, in the routed run's order: {built:?}"
    );
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
fn a_repo_s_inputs_reach_the_run_and_a_repo_without_an_entry_has_none() {
    let config =
        Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"], "other": ["*"]}, "inputs": {"Yaat": [".tmp/helper/**", "keys.json"]}}"#)
            .expect("usable");
    let command = words(&["cargo", "build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.inputs, [".tmp/helper/**", "keys.json"]);

    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}, "inputs": {"other": ["keys.json"]}}"#).expect("usable");
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.inputs, Vec::<String>::new());
}

#[test]
fn a_dotnet_no_build_command_keeps_its_inputs_while_its_outputs_are_withheld() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "inputs": {"yaat": [".tmp/helper/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "test", "--no-build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert!(run.outputs_withheld);
    assert_eq!(run.outputs, Vec::<String>::new());
    assert_eq!(run.inputs, [".tmp/helper/**"], "the drop withholds outputs, not inputs");
}

#[test]
fn a_repo_s_own_outputs_replace_the_cargo_default() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["cargo *"]}, "outputs": {"yaat": ["target/**"]}}"#).expect("usable");
    let command = words(&["cargo", "build"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["target/**"], "the repo's own list, not the cargo default");
}

#[test]
fn a_dotnet_build_of_one_project_brings_back_its_bin_beside_the_repo_s_own_outputs() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/App/App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn a_dotnet_build_of_a_project_option_folder_brings_back_that_folder_s_bin() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "--project", "src\\App"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn a_dotnet_build_in_a_subfolder_brings_back_the_bin_under_it() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let facts = FakeFacts {
        config: Ok(Some(config)),
        ..FakeFacts::at("src/c/")
    };
    let command = words(&["dotnet", "build", "App.csproj"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/c/bin/**"]);
}

#[test]
fn a_dotnet_build_whose_glob_is_already_listed_adds_no_copy() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["src/App/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/App/App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["src/App/bin/**"]);
}

#[test]
fn no_bin_is_derived_for_a_solution_or_an_output_folder() {
    for command in [
        words(&["dotnet", "build", "App.sln"]),
        words(&["dotnet", "build", "src/App/App.csproj", "-o", "out"]),
        words(&["dotnet", "build"]),
    ] {
        let config =
            Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
        let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
        assert_eq!(run.outputs, ["tools/X/bin/**"], "{command:?}");
    }
}

#[test]
fn no_bin_is_derived_for_an_empty_outputs_list() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": []}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/App/App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, Vec::<String>::new());
}

#[test]
fn no_bin_is_derived_on_a_macos_builder() {
    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet build*"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}]"#;
    let config = with_builders(r#"{"yaat": ["*"]}"#, mac);
    let command = words(&["dotnet", "build", "src/App/App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_build_of_a_folder() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/Yaat.Client"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/Yaat.Client/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_test_of_a_project_folder() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "test", "tests/A.Tests", "-c", "Release"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "tests/A.Tests/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_test_of_a_project_file() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "test", "src/App/App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn no_bin_is_derived_for_a_test_on_a_macos_builder() {
    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet test*"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}]"#;
    let config = with_builders(r#"{"yaat": ["*"]}"#, mac);
    let command = words(&["dotnet", "test", "tests/A.Tests", "-c", "Release"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**"]);
}

#[test]
fn no_bin_is_derived_for_a_test_of_a_solution() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "test", "App.sln"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**"]);
}

#[test]
fn the_bin_is_derived_under_the_subfolder_for_a_build_with_no_target() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let facts = FakeFacts {
        config: Ok(Some(config)),
        ..FakeFacts::at("src/c/")
    };
    let command = words(&["dotnet", "build"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/c/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_build_of_a_project_file_at_the_root() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", ".\\App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_build_of_a_project_file_written_with_extra_segments() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src//App/./App.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_platform_property() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/App/App.csproj", "-p:Platform=x64"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn no_bin_is_derived_for_an_output_path_property() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "src/App/App.csproj", "-p:OutputPath=x"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**"]);
}

#[test]
fn the_bin_is_derived_for_a_project_option_in_every_spelling() {
    for command in [
        words(&["dotnet", "build", "--project=src/App"]),
        words(&["dotnet", "build", "--project:src/App"]),
    ] {
        let config =
            Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
        let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
        assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"], "{command:?}");
    }
}

#[test]
fn the_bin_is_derived_for_a_build_spelled_in_any_case() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["DOTNET", "BUILD", "src/App/App.CSPROJ"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**", "src/App/bin/**"]);
}

#[test]
fn no_bin_is_derived_for_an_empty_project_option() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet *"]}, "outputs": {"yaat": ["tools/X/bin/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "build", "--project", ""]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.outputs, ["tools/X/bin/**"]);
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
    assert_eq!(facts.asked(), ["config", "place", "file_in_folder build.cmd", "head_sha"]);
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
    assert_eq!(facts.asked(), ["config", "place", "head_sha"]);
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
        assert_eq!(head_asks(&facts), 1, "HEAD is asked once for {command:?}");
    }
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&words(&["cargo", "build"])), &facts));
    assert_eq!(run.words, ["cargo", "build"], "another build tool's command is left alone");
    assert_eq!(head_asks(&facts), 1, "HEAD is asked once for cargo build");
}

#[test]
fn a_routed_command_without_the_word_still_carries_the_worktree_head() {
    let command = words(&["dotnet", "format"]);
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, command, "the words get no stamp");
    assert_eq!(run.source_revision.as_deref(), Some(HEAD), "the run carries the worktree's HEAD");

    let heads: [Result<Option<String>, String>; 2] = [Ok(None), Err("git could not be started".to_owned())];
    for head in heads {
        let facts = FakeFacts {
            head,
            ..FakeFacts::with_config(routing_all())
        };
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.source_revision, None, "{:?}", facts.head);
    }
}

#[test]
fn a_routed_build_asks_head_once_for_its_word_and_its_revision() {
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &facts));
    assert_eq!(run.words, stamped(&["dotnet", "build"]));
    assert_eq!(run.source_revision.as_deref(), Some(HEAD));
    assert_eq!(head_asks(&facts), 1, "one read serves the word and the revision");
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
        assert_eq!(head_asks(&facts), 1, "HEAD is asked once for {command:?}");
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
        assert_eq!(head_asks(&facts), 1, "HEAD is asked once for {command:?}");
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

/// The builder's root the path-map fixtures lay the work folder out under.
const BUILDER: &str = r"D:\b";

/// `facts` with the box's trees laid out under [`BUILDER`].
fn mapped(facts: FakeFacts) -> FakeFacts {
    FakeFacts {
        builder_root: Some(BUILDER.to_owned()),
        ..facts
    }
}

/// The fixture worktree's work folder on [`BUILDER`], or its tree `name` when one is given.
fn box_tree(name: Option<&str>) -> String {
    let work = format!(r"{BUILDER}\work\{}", work_folder_key(ROOT));
    name.map_or_else(|| work.clone(), |name| format!(r"{work}\{name}"))
}

/// The path-map word of a run of the fixture worktree alone: its work folder mapped to its root.
fn path_map() -> String {
    format!(r"-p:PathMap={}\=X:\dev\yaat.wt\a", box_tree(None))
}

#[test]
fn a_routed_dotnet_build_maps_its_work_folder_to_the_worktree_root_after_its_source_revision() {
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &mapped(FakeFacts::at_root())));
    assert_eq!(run.words, ["dotnet".to_owned(), "build".to_owned(), stamp(), path_map()]);
    assert_eq!(run.fallback, ["dotnet", "build"], "the fallback carries no word of the builder's");

    let command = words(&["dotnet", "test", "proj.csproj", "--", "NUnit.Where=x"]);
    let run = routed(decide(&heavy(&command), &mapped(FakeFacts::at_root())));
    assert_eq!(
        run.words,
        [
            "dotnet",
            "test",
            "proj.csproj",
            stamp().as_str(),
            path_map().as_str(),
            "--",
            "NUnit.Where=x"
        ],
        "both words stand before the dotnet command's own double dash"
    );
}

#[test]
fn a_pair_run_maps_each_tree_to_its_own_root_joined_by_an_escaped_comma() {
    let facts = mapped(FakeFacts::with_siblings("", &["b", "c"], &["b", "c"]));
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &facts));
    let expected = format!(
        r"-p:PathMap={}\=X:\dev\yaat.wt\a%2C{}\=X:\dev\yaat.wt\b%2C{}\=X:\dev\yaat.wt\c",
        box_tree(Some("a")),
        box_tree(Some("b")),
        box_tree(Some("c"))
    );
    assert_eq!(run.words, ["dotnet".to_owned(), "build".to_owned(), stamp(), expected]);
}

#[test]
fn the_box_side_ends_in_one_separator_and_the_pc_side_in_none() {
    let facts = mapped(FakeFacts {
        given_trees: Some(vec![
            (r"D:\b\work\k".to_owned(), r"X:\dev\one\".to_owned()),
            (r"D:\b\work\k\lib\\".to_owned(), r"X:\dev\lib\\".to_owned()),
        ]),
        ..FakeFacts::at_root()
    });
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &facts));
    let expected = r"-p:PathMap=D:\b\work\k\=X:\dev\one%2CD:\b\work\k\lib\=X:\dev\lib";
    assert_eq!(run.words, ["dotnet".to_owned(), "build".to_owned(), stamp(), expected.to_owned()]);
}

#[test]
fn a_build_whose_head_cannot_be_read_still_maps_its_work_folder() {
    let heads: [Result<Option<String>, String>; 2] = [Ok(None), Err("git could not be started".to_owned())];
    for head in heads {
        let facts = mapped(FakeFacts {
            head,
            ..FakeFacts::at_root()
        });
        let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &facts));
        assert_eq!(run.words, ["dotnet".to_owned(), "build".to_owned(), path_map()], "{:?}", facts.head);
    }
}

#[test]
fn a_build_that_sets_a_path_property_itself_gets_no_path_map_but_keeps_its_source_revision() {
    let commands: [&[&str]; 5] = [
        &["dotnet", "build", "-p:PathMap=a=b"],
        &["dotnet", "build", "/p:ContinuousIntegrationBuild=true"],
        &["dotnet", "build", "-property:A=1;DeterministicSourcePaths=true"],
        &["dotnet", "build", "--property:Configuration=Release,pathmap=x"],
        &["dotnet", "build", "/PROPERTY:continuousintegrationbuild=true"],
    ];
    for command in commands {
        let run = routed(decide(&heavy(&words(command)), &mapped(FakeFacts::with_config(routing_all()))));
        assert_eq!(run.words, stamped(command), "{command:?}");
    }
}

#[test]
fn a_build_that_sets_a_property_in_two_words_opts_out_of_that_word_alone() {
    let path_map_set: [&[&str]; 3] = [
        &["dotnet", "build", "-p", "PathMap=x"],
        &["dotnet", "build", "--property", "PathMap=x"],
        &["dotnet", "build", "-PROPERTY", "ContinuousIntegrationBuild=true"],
    ];
    for command in path_map_set {
        let run = routed(decide(&heavy(&words(command)), &mapped(FakeFacts::with_config(routing_all()))));
        assert_eq!(run.words, stamped(command), "{command:?}");
    }

    let command = ["dotnet", "build", "/p", "SourceRevisionId=y"];
    let run = routed(decide(&heavy(&words(&command)), &mapped(FakeFacts::with_config(routing_all()))));
    let mut expected = words(&command);
    expected.push(path_map());
    assert_eq!(run.words, expected, "the caller's revision stands and the path map still goes in");
}

#[test]
fn a_build_that_sets_a_property_through_any_switch_spelling_opts_out_of_that_word_alone() {
    let path_map_set: [&[&str]; 4] = [
        &["dotnet", "build", "--property=PathMap=x"],
        &["dotnet", "build", "-p=PathMap=x"],
        &["dotnet", "build", "--p:PathMap=x"],
        &["dotnet", "build", "--p", "PathMap=x"],
    ];
    for command in path_map_set {
        let run = routed(decide(&heavy(&words(command)), &mapped(FakeFacts::with_config(routing_all()))));
        assert_eq!(run.words, stamped(command), "{command:?}");
    }

    let command = ["dotnet", "build", "/p=SourceRevisionId=y"];
    let run = routed(decide(&heavy(&words(&command)), &mapped(FakeFacts::with_config(routing_all()))));
    let mut expected = words(&command);
    expected.push(path_map());
    assert_eq!(run.words, expected, "the caller's revision stands and the path map still goes in");
}

#[test]
fn a_setting_name_with_space_or_quotes_around_it_still_opts_out() {
    let commands: [&[&str]; 2] = [&["dotnet", "build", r#"-p:"A=1; PathMap=x""#], &["dotnet", "build", r#"-p:"PathMap=x""#]];
    for command in commands {
        let run = routed(decide(&heavy(&words(command)), &mapped(FakeFacts::with_config(routing_all()))));
        assert_eq!(run.words, stamped(command), "{command:?}");
    }
}

#[test]
fn a_project_after_a_bare_p_sets_no_property() {
    let command = ["dotnet", "run", "-p", "proj.csproj"];
    let run = routed(decide(&heavy(&words(&command)), &mapped(FakeFacts::with_config(routing_all()))));
    let mut expected = stamped(&command);
    expected.push(path_map());
    assert_eq!(run.words, expected, "an older SDK's -p names the project");
}

#[test]
fn a_build_that_sets_its_own_source_revision_still_maps_its_work_folder() {
    let command = ["dotnet", "build", "-p:SourceRevisionId=abc"];
    let run = routed(decide(&heavy(&words(&command)), &mapped(FakeFacts::at_root())));
    let mut expected = words(&command);
    expected.push(path_map());
    assert_eq!(run.words, expected);
}

#[test]
fn a_response_file_or_a_command_that_does_not_build_gets_no_path_map() {
    let commands: [&[&str]; 3] = [&["dotnet", "build", "@x.rsp"], &["dotnet", "--version"], &["dotnet", "restore"]];
    for command in commands {
        let run = routed(decide(&heavy(&words(command)), &mapped(FakeFacts::at_root())));
        assert_eq!(run.words, words(command), "{command:?}");
    }
}

#[test]
fn the_warm_up_words_carry_the_routed_run_path_map() {
    let (_, built) = assert_builder_words_match(&words(&["dotnet", "build"]), &mapped(FakeFacts::at_root()));
    assert!(built.contains(&path_map()), "{built:?}");

    let facts = mapped(FakeFacts::with_siblings("", &["b", "c"], &["b", "c"]));
    let (_, built) = assert_builder_words_match(&words(&["dotnet", "test", "--no-build"]), &facts);
    assert!(built.iter().any(|word| word.contains("%2C")), "{built:?}");

    let facts = mapped(FakeFacts {
        head: Ok(None),
        ..FakeFacts::at_root()
    });
    let (_, built) = assert_builder_words_match(&words(&["dotnet", "build"]), &facts);
    assert_eq!(built, ["dotnet".to_owned(), "build".to_owned(), path_map()]);
}

#[test]
fn a_dotnet_program_named_by_an_in_worktree_path_still_builds_stamped() {
    let command = words(&[r"X:\dev\yaat.wt\a\dotnet.exe", "pack"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(routing_all())));
    assert_eq!(run.words, ["dotnet.exe", "pack", stamp().as_str()]);
}

#[test]
fn a_program_path_outside_the_worktree_in_a_later_word_runs_here_with_no_word() {
    let command = words(&["dotnet", "pack", r"C:\Program Files\dotnet\dotnet.exe"]);
    let facts = FakeFacts::with_config(routing_all());
    let route = decide(&heavy(&command), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some(r"gate: not routed: C:\Program Files\dotnet\dotnet.exe is outside the worktree")
    );
    assert_eq!(head_asks(&facts), 0, "HEAD is not asked for a command that runs here");
}

#[test]
fn a_dotnet_program_named_by_a_path_outside_the_worktree_routes_as_dotnet() {
    let command = words(&[r"C:\Program Files\dotnet\dotnet.exe", "pack"]);
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, ["dotnet", "pack", stamp().as_str()]);
    assert!(run.program_named, "the word was sent as a program's name");
    assert_eq!(run.fallback, command, "the fallback keeps the path the caller gave");
}

#[test]
fn a_program_named_by_a_path_outside_matches_a_pattern_on_its_name() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet build *"]}}"#).expect("usable");
    let command = words(&[r"C:\Program Files\dotnet\dotnet.exe", "build", "X:/dev/yaat.wt/a/src/X.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.pattern, "dotnet build *", "the pattern reads the program's name");
    assert_eq!(run.words, ["dotnet", "build", "src/X.csproj", stamp().as_str()]);
    assert!(run.program_named);
}

#[test]
fn a_program_name_the_rewrite_derived_is_no_file_of_the_gate_s_folder() {
    let derived = FakeFacts {
        files: words(&["dotnet"]),
        ..FakeFacts::with_config(routing_all())
    };
    let command = words(&[r"C:\Program Files\dotnet\dotnet.exe", "build"]);
    let run = routed(decide(&heavy(&command), &derived));
    assert_eq!(
        run.words[0], "dotnet",
        "the caller asked for the program outside, not the file here: {:?}",
        run.words
    );
    assert!(
        !derived.asked().iter().any(|question| question.starts_with("file_in_folder")),
        "no file of the gate's folder is looked for: {:?}",
        derived.asked()
    );

    let bare = FakeFacts {
        files: words(&["dotnet"]),
        ..FakeFacts::with_config(routing_all())
    };
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &bare));
    assert_eq!(
        run.words[0], "./dotnet",
        "a bare word naming a file of the folder is still marked: {:?}",
        run.words
    );
}

#[test]
fn a_word_sent_as_a_program_s_name_is_said_before_the_run_s_other_lines() {
    let command = words(&[r"C:\Program Files\dotnet\dotnet.exe", "build"]);
    let plain = routed(decide(&heavy(&command), &FakeFacts::with_config(routing_all())));
    assert_eq!(plain.lines(), [r"gate: routed C:\Program Files\dotnet\dotnet.exe as dotnet".to_owned()]);

    let no_build = words(&[r"C:\Program Files\dotnet\dotnet.exe", "test", "--no-build"]);
    let run = routed(decide(&heavy(&no_build), &FakeFacts::with_config(routing_all())));
    assert_eq!(
        run.lines(),
        [
            r"gate: routed C:\Program Files\dotnet\dotnet.exe as dotnet".to_owned(),
            DROPPED_NO_BUILD_LINE.to_owned(),
            NO_OUTPUTS_LINE.to_owned(),
        ],
        "the line comes before the run's other lines"
    );

    let tabbed = words(&["F:\\tools\\a\tb\\tool.exe", "arg"]);
    let escaped = routed(decide(&heavy(&tabbed), &FakeFacts::with_config(routing_all())));
    assert_eq!(
        escaped.lines(),
        ["gate: routed F:\\tools\\a\\tb\\tool.exe as tool".to_owned()],
        "both words are escaped as the run's lines escape words"
    );
}

#[test]
fn an_upper_case_dotnet_program_named_by_a_path_outside_routes_stamped() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["dotnet build *"]}}"#).expect("usable");
    let command = words(&[r"C:\Program Files\dotnet\DOTNET.EXE", "build", "X:/dev/yaat.wt/a/src/X.csproj"]);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config)));
    assert_eq!(run.pattern, "dotnet build *", "the pattern reads the name in any letter case");
    assert_eq!(run.words, ["DOTNET", "build", "src/X.csproj", stamp().as_str()]);
    assert!(run.program_named);
}

#[test]
fn a_program_named_by_a_path_outside_after_an_empty_word_is_still_no_file_of_the_folder() {
    let derived = FakeFacts {
        files: words(&["dotnet"]),
        ..FakeFacts::with_config(routing_all())
    };
    let command = words(&["", r"C:\Program Files\dotnet\dotnet.exe", "build"]);
    let run = routed(decide(&heavy(&command), &derived));
    assert_eq!(
        run.words,
        ["", "dotnet", "build", stamp().as_str()],
        "the first non-empty word is the one sent as a program's name"
    );
    assert!(run.program_named);
    assert!(
        !derived.asked().iter().any(|question| question.starts_with("file_in_folder")),
        "no file of the gate's folder is looked for: {:?}",
        derived.asked()
    );
}

#[test]
fn a_subst_drive_into_the_worktree_names_a_file_of_the_folder_not_a_program() {
    let facts = FakeFacts {
        files: words(&["x"]),
        devices: vec![('D', r"\??\X:\dev\yaat.wt\a".to_owned())],
        ..FakeFacts::with_config(routing_all())
    };
    let command = words(&[r"D:\x", "arg"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, ["./x", "arg"], "the worktree's own file is marked, never looked up on PATH");
    assert!(!run.program_named, "the word stayed a path of the worktree");
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
    assert_eq!(head_asks(&facts), 0, "HEAD is not asked for a command that runs here");
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

/// The patterns `texts`, each parsed as `.gate/desktop.toml` declares it.
fn patterns(texts: &[&str]) -> Vec<Pattern> {
    texts.iter().map(|text| Pattern::parse(text).expect("a valid pattern")).collect()
}

/// A gate at the root whose `.gate/desktop.toml` declares `declared`, under a routing file routing `dotnet *` and
/// `tool *` of `yaat` whose `desktop` value is `desktop` (no key at all for `None`).
fn desktop_facts(desktop: Option<&str>, declared: &[&str]) -> FakeFacts {
    let key = desktop.map_or_else(String::new, |value| format!(r#", "desktop": {value}"#));
    let config = Config::parse(&format!(r#"{{"host": "builder-1", "repos": {{"yaat": ["dotnet *", "tool *"]}}{key}}}"#)).expect("usable");
    FakeFacts {
        desktop: Ok(patterns(declared)),
        ..FakeFacts::with_config(config)
    }
}

#[test]
fn a_command_matching_a_consented_desktop_pattern_asks_for_the_desktop() {
    let facts = desktop_facts(Some(r#"{"YAAT": ["dotnet build*", "dotnet test*"]}"#), &["dotnet test*", "dotnet build*"]);
    let run = routed(decide(&heavy(&words(&["dotnet", "test"])), &facts));
    assert!(run.desktop, "the first declared pattern that matches is listed");
    assert_eq!(run.pattern, "dotnet *", "the repos pattern still names the route");
    assert_eq!(facts.desktop_read.get(), 1);
}

#[test]
fn a_routed_command_matching_no_declared_pattern_routes_without_the_desktop() {
    let facts = desktop_facts(Some(r#"{"yaat": ["dotnet test*"]}"#), &["dotnet test*"]);
    let run = routed(decide(&heavy(&words(&["tool", "x"])), &facts));
    assert!(!run.desktop);
}

#[test]
fn a_declared_desktop_pattern_remote_json_does_not_list_keeps_its_commands_here() {
    for (case, desktop, line) in [
        (
            "no key",
            None,
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json",
        ),
        (
            "no entry",
            Some(r#"{"other": ["dotnet test*"]}"#),
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json",
        ),
        (
            "another case",
            Some(r#"{"yaat": ["Dotnet test*"]}"#),
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json",
        ),
        (
            "another pattern",
            Some(r#"{"yaat": ["tool *"]}"#),
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json",
        ),
        (
            "not an object",
            Some(r#"["dotnet test*"]"#),
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json: desktop must be a JSON object",
        ),
        (
            "a bad entry",
            Some(r#"{"yaat": ["dotnet test*", 3]}"#),
            "gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json: desktop.\"yaat\"[1] is not a non-empty string",
        ),
    ] {
        let facts = desktop_facts(desktop, &["dotnet test*", "tool *"]);
        assert_eq!(
            local_line(&decide(&heavy(&words(&["dotnet", "test"])), &facts)).as_deref(),
            Some(line),
            "{case}"
        );
    }
    let facts = desktop_facts(Some(r#"{"yaat": ["tool *"]}"#), &["dotnet test*", "tool *"]);
    assert!(
        routed(decide(&heavy(&words(&["tool", "x"])), &facts)).desktop,
        "an unlisted pattern stops only the commands it matches"
    );
    assert!(!routed(decide(&heavy(&words(&["dotnet", "build"])), &facts)).desktop);
}

#[test]
fn the_first_declared_pattern_that_matches_decides_even_when_a_later_one_is_allowed() {
    let facts = desktop_facts(Some(r#"{"yaat": ["dotnet test*"]}"#), &["dotnet *", "dotnet test*"]);
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test"])), &facts)).as_deref(),
        Some("gate: not routed: desktop pattern 'dotnet *' not allowed in remote.json")
    );
}

#[test]
fn a_not_allowed_desktop_pattern_is_said_before_a_word_outside_the_worktree() {
    let command = words(&["dotnet", "test", r"..\other\x.csproj"]);
    let undeclared = desktop_facts(None, &[]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &undeclared)).as_deref(),
        Some(r"gate: not routed: ..\other\x.csproj is outside the worktree"),
        "the command alone names a path outside"
    );
    let declared = desktop_facts(None, &["dotnet test*"]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &declared)).as_deref(),
        Some("gate: not routed: desktop pattern 'dotnet test*' not allowed in remote.json")
    );
}

#[test]
fn a_not_allowed_desktop_pattern_is_shown_escaped() {
    let facts = desktop_facts(None, &["dotnet test\u{1b}*"]);
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test\u{1b}x"])), &facts)).as_deref(),
        Some(r"gate: not routed: desktop pattern 'dotnet test\u{1b}*' not allowed in remote.json")
    );
}

#[test]
fn a_desktop_file_problem_keeps_every_routed_command_here() {
    for command in [&["dotnet", "test"][..], &["tool", "x"]] {
        let facts = FakeFacts {
            desktop: Err("line 1, column 3:\r\n  bad\u{1b}[31m".to_owned()),
            ..desktop_facts(Some(r#"{"yaat": ["dotnet test*"]}"#), &[])
        };
        assert_eq!(
            local_line(&decide(&heavy(&words(command)), &facts)).as_deref(),
            Some(r"gate: not routed: .gate/desktop.toml: line 1, column 3: bad\u{1b}[31m"),
            "{command:?}"
        );
    }
}

/// A gate at the root whose `.gate/local.toml` declares `declared`, under the routing file [`desktop_facts`] writes
/// with no `desktop` key.
fn local_facts(declared: &[&str]) -> FakeFacts {
    FakeFacts {
        local: Ok(patterns(declared)),
        ..desktop_facts(None, &[])
    }
}

#[test]
fn a_command_matching_a_declared_local_pattern_runs_here_naming_the_first_that_matches() {
    let facts = local_facts(&["dotnet test*", "DOTNET *"]);
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test", "x"])), &facts)).as_deref(),
        Some("gate: not routed: .gate/local.toml keeps it here (dotnet test*)")
    );
    assert_eq!(
        local_line(&decide(&heavy(&words(&["Dotnet", "Build"])), &facts)).as_deref(),
        Some("gate: not routed: .gate/local.toml keeps it here (DOTNET *)"),
        "matched in any letter case, as the repo's patterns are"
    );
    assert_eq!(facts.local_read.get(), 2);
}

#[test]
fn a_routed_command_matching_no_declared_local_pattern_routes() {
    let facts = local_facts(&["dotnet test*"]);
    let run = routed(decide(&heavy(&words(&["tool", "x"])), &facts));
    assert_eq!(run.pattern, "tool *");
    assert_eq!(facts.local_read.get(), 1);
    let none = local_facts(&[]);
    assert_eq!(
        routed(decide(&heavy(&words(&["dotnet", "test"])), &none)).pattern,
        "dotnet *",
        "an empty list keeps nothing here"
    );
}

#[test]
fn a_declared_local_pattern_is_shown_escaped() {
    let facts = local_facts(&["dotnet test\u{1b}*"]);
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test\u{1b}x"])), &facts)).as_deref(),
        Some(r"gate: not routed: .gate/local.toml keeps it here (dotnet test\u{1b}*)")
    );
}

#[test]
fn a_local_file_problem_keeps_every_routed_command_here() {
    for command in [&["dotnet", "test"][..], &["tool", "x"]] {
        let facts = FakeFacts {
            local: Err("line 1, column 3:\r\n  bad\u{1b}[31m".to_owned()),
            ..local_facts(&[])
        };
        assert_eq!(
            local_line(&decide(&heavy(&words(command)), &facts)).as_deref(),
            Some(r"gate: not routed: .gate/local.toml: line 1, column 3: bad\u{1b}[31m"),
            "{command:?}"
        );
    }
}

#[test]
fn a_declared_local_pattern_wins_over_a_desktop_match_and_sibling_problems() {
    let line = Some("gate: not routed: .gate/local.toml keeps it here (dotnet test*)");
    let desktop = FakeFacts {
        local: Ok(patterns(&["dotnet test*"])),
        ..desktop_facts(None, &["dotnet test*"])
    };
    assert_eq!(local_line(&decide(&heavy(&words(&["dotnet", "test"])), &desktop)).as_deref(), line);
    assert_eq!(desktop.desktop_read.get(), 0, "the desktop file is never read");
    let broken = FakeFacts {
        siblings: Err("bad".to_owned()),
        ..local_facts(&["dotnet test*"])
    };
    assert_eq!(local_line(&decide(&heavy(&words(&["dotnet", "test"])), &broken)).as_deref(), line);
    let missing = FakeFacts {
        siblings: Ok(words(&["b"])),
        ..local_facts(&["dotnet test*"])
    };
    assert_eq!(local_line(&decide(&heavy(&words(&["dotnet", "test"])), &missing)).as_deref(), line);
    assert!(missing.presence_asked.borrow().is_empty(), "no sibling is looked for");
    let failing = FakeFacts {
        local: Err("bad".to_owned()),
        siblings: Err("worse".to_owned()),
        ..desktop_facts(None, &["dotnet test*"])
    };
    assert_eq!(
        local_line(&decide(&heavy(&words(&["dotnet", "test"])), &failing)).as_deref(),
        Some("gate: not routed: .gate/local.toml: bad")
    );
}

#[test]
fn a_command_the_repos_patterns_do_not_route_reads_no_local_file() {
    for local in [Ok(patterns(&["cargo *"])), Err("bad".to_owned())] {
        let facts = FakeFacts { local, ..local_facts(&[]) };
        assert_eq!(decide(&heavy(&words(&["cargo", "test"])), &facts), SILENT);
        assert_eq!(facts.local_read.get(), 0);
    }
}

#[test]
fn a_command_the_repos_patterns_do_not_route_reads_no_desktop_file() {
    for desktop in [Ok(patterns(&["cargo *"])), Err("bad".to_owned())] {
        let facts = FakeFacts {
            desktop,
            ..desktop_facts(Some(r#"{"yaat": ["cargo *"]}"#), &[])
        };
        assert_eq!(decide(&heavy(&words(&["cargo", "test"])), &facts), SILENT);
        assert_eq!(facts.desktop_read.get(), 0);
    }
}

/// A routing file whose `host`, `builder-1`, lists the repos `host_repos`, with the `builders` entries `builders`.
fn with_builders(host_repos: &str, builders: &str) -> Config {
    Config::parse(&format!(r#"{{"host": "builder-1", "repos": {host_repos}, "builders": {builders}}}"#)).expect("usable")
}

/// The `builders` entry `mac-1`, a macOS builder, as a run goes to it.
fn mac_builder() -> Builder {
    Builder {
        alias: "mac-1".to_owned(),
        os: BuilderOs::Macos,
        root: Some("/Users/me/gate".to_owned()),
        slots: 1,
        id: Some("1f0efc62".to_owned()),
    }
}

/// The `builders` list of the one entry `mac-1`, running `yaat`'s `dotnet build` commands.
const MAC_BUILD: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet build*"]}}]"#;

#[test]
fn a_command_matching_a_builder_entry_s_pattern_routes_to_that_builder() {
    let config = with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD);
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &FakeFacts::with_config(config)));
    assert_eq!(run.builder, mac_builder());
    assert_eq!(run.pattern, "dotnet build*");
}

#[test]
fn a_command_matching_only_the_top_level_patterns_routes_to_host_without_an_id() {
    let config = with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD);
    let run = routed(decide(&heavy(&words(&["dotnet", "test"])), &FakeFacts::with_config(config)));
    assert_eq!(run.builder, host_builder("builder-1", 1));
    assert_eq!(run.pattern, "dotnet *");
}

#[test]
fn the_first_builder_entry_in_file_order_wins_when_two_match() {
    let config = with_builders(
        r#"{"yaat": ["dotnet *"]}"#,
        r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet *"]}},
            {"host": "win-2", "os": "windows", "slots": 3, "repos": {"yaat": ["*"]}}]"#,
    );
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &FakeFacts::with_config(config.clone())));
    assert_eq!(run.builder, mac_builder());
    let run = routed(decide(&heavy(&words(&["cargo", "build"])), &FakeFacts::with_config(config)));
    assert_eq!(
        run.builder,
        Builder {
            alias: "win-2".to_owned(),
            os: BuilderOs::Windows,
            root: None,
            slots: 3,
            id: Some("084c286b".to_owned()),
        }
    );
}

#[test]
fn a_repo_named_only_in_a_builder_entry_routes_to_that_builder_alone() {
    let config = with_builders(r#"{"other": ["*"]}"#, MAC_BUILD);
    let facts = FakeFacts::with_config(config.clone());
    assert_eq!(routed(decide(&heavy(&words(&["dotnet", "build"])), &facts)).builder, mac_builder());
    let facts = FakeFacts::with_config(config.clone());
    assert_eq!(decide(&heavy(&words(&["dotnet", "test"])), &facts), SILENT);
    let facts = FakeFacts::with_config(config.clone());
    let place = facts.place().expect("the place reads").expect("there is a worktree");
    assert_eq!(
        target(&config, place, &facts),
        Err(None),
        "a warm-up goes to host, which does not list the repo"
    );
}

#[test]
fn a_warm_up_target_goes_to_host_even_when_a_builder_entry_lists_the_repo() {
    let config = with_builders(r#"{"yaat": ["dotnet test *"]}"#, MAC_BUILD);
    let facts = FakeFacts::with_config(config.clone());
    let place = facts.place().expect("the place reads").expect("there is a worktree");
    let warm = target(&config, place, &facts).expect("the worktree has a target");
    assert_eq!(warm.builder, host_builder("builder-1", 1));
}

/// The builders [`RemoteRun::later`] lists after the first pick, in try order, by alias.
fn later_aliases(run: &RemoteRun) -> Vec<&str> {
    run.later.iter().map(|later| later.alias.as_str()).collect()
}

#[test]
fn a_routed_command_lists_every_other_matching_builder_in_file_order_then_host() {
    let config = with_builders(
        r#"{"yaat": ["dotnet *"]}"#,
        r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet build*"]}},
            {"host": "win-2", "os": "windows", "slots": 3, "repos": {"yaat": ["cargo *", "dotnet build*"]}}]"#,
    );
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(config);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder, mac_builder());
    assert_eq!(later_aliases(&run), ["win-2", "builder-1"]);
    let patterns: Vec<String> = (0..2)
        .map(|at| later_of(&run, at, &command, &facts).expect("each candidate can be tried").pattern)
        .collect();
    assert_eq!(
        patterns,
        ["dotnet build*", "dotnet *"],
        "each candidate runs under the first pattern of its own list that matches"
    );
}

#[test]
fn a_builder_whose_patterns_do_not_match_is_not_a_later_candidate() {
    let config = with_builders(
        r#"{"yaat": ["dotnet *"]}"#,
        r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["cargo *"]}},
            {"host": "win-3", "os": "windows", "repos": {"yaat": ["dotnet build*"]}},
            {"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["cargo build*"]}}]"#,
    );
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &FakeFacts::with_config(config)));
    assert_eq!(run.builder.alias, "win-3", "the first entry whose patterns match");
    assert_eq!(later_aliases(&run), ["builder-1"], "only host's patterns match too");
}

#[test]
fn a_single_builder_route_has_no_later_candidates() {
    let facts = FakeFacts::with_config(routing_all());
    let run = routed(decide(&heavy(&words(&["dotnet", "build"])), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert!(run.later.is_empty(), "{:?}", later_aliases(&run));
}

#[test]
fn building_the_later_candidates_asks_no_fact() {
    let builders = r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["*"]}},
        {"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["*"]}}]"#;
    let with = FakeFacts::with_config(with_builders(r#"{"yaat": ["*"]}"#, builders));
    let alone = FakeFacts::with_config(routing_all());
    let tool = words(&[r".\tool.exe"]);
    let run = routed(decide(&heavy(&tool), &with));
    assert_eq!(run.builder.alias, "win-2");
    assert_eq!(later_aliases(&run), ["mac-1", "builder-1"], "both later builders are listed");
    assert_eq!(routed(decide(&heavy(&tool), &alone)).builder.alias, "builder-1");
    assert_eq!(with.asked(), alone.asked(), "the later candidates ask nothing");

    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["*"]}}]"#;
    let listed = |host_repos: &str, builders: Option<&str>| {
        let builders = builders.map_or_else(String::new, |builders| format!(r#", "builders": {builders}"#));
        Config::parse(&format!(
            r#"{{"host": "builder-1", "repos": {host_repos}, "sendBuild": ["yaat"]{builders}}}"#
        ))
        .expect("usable")
    };
    let with = FakeFacts::with_config(listed(r#"{"yaat": ["*"]}"#, Some(mac)));
    let alone = FakeFacts::with_config(listed("{}", Some(mac)));
    let command = words(&["dotnet", "test", "App.csproj", "-c", "Release", "--no-build"]);
    let run = routed(decide(&heavy(&command), &with));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(later_aliases(&run), ["builder-1"], "host may be tried next");
    assert_eq!(routed(decide(&heavy(&command), &alone)).builder.alias, "mac-1");
    assert_eq!(with.asked(), alone.asked(), "listing host asks no fact");
}

/// The run of the later candidate `at` of `run`, built for the command `command` as the first pick was.
fn later_of(run: &RemoteRun, at: usize, command: &[String], facts: &FakeFacts) -> Result<RemoteRun, Option<String>> {
    later_run(&heavy(command), facts, &run.later[at])
}

#[test]
fn a_windows_candidate_after_a_macos_pick_runs_the_path_word_with_backslashes() {
    let command = words(&["dotnet", "build", r"X:\dev\yaat.wt\a\src\Duel.sln"]);
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD));
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder, mac_builder());
    assert_eq!(run.words, stamped(&["dotnet", "build", "src/Duel.sln"]));
    let host = later_of(&run, 0, &command, &facts).expect("host can be tried");
    assert_eq!(host.builder, host_builder("builder-1", 1));
    assert_eq!(host.words, stamped(&["dotnet", "build", r"src\Duel.sln"]));
    assert_eq!(host.outputs, ["**/bin/**", "**/.godot/mono/temp/bin/**"], "host's own outputs");
    assert_eq!(host.key, run.key, "the work folder is the worktree's");
}

#[test]
fn each_later_candidate_runs_under_its_own_pattern() {
    let config = with_builders(
        r#"{"yaat": ["dotnet *"]}"#,
        r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet build*"]}},
            {"host": "win-2", "os": "windows", "slots": 3, "repos": {"yaat": ["cargo *", "dotnet build*"]}}]"#,
    );
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(config);
    let run = routed(decide(&heavy(&command), &facts));
    let second = later_of(&run, 0, &command, &facts).expect("win-2 can be tried");
    assert_eq!((second.builder.alias.as_str(), second.pattern.as_str()), ("win-2", "dotnet build*"));
    let third = later_of(&run, 1, &command, &facts).expect("host can be tried");
    assert_eq!((third.builder.alias.as_str(), third.pattern.as_str()), ("builder-1", "dotnet *"));
}

#[test]
fn a_later_candidate_is_built_from_the_routing_file_as_it_reads_at_the_fallback() {
    let builders = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}},
        {"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet *"]}}]"#;
    let command = words(&["dotnet", "build"]);
    let at_route = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, builders));
    let run = routed(decide(&heavy(&command), &at_route));
    assert_eq!(later_aliases(&run), ["win-2", "builder-1"]);

    let edited = r#"{"host": "builder-1", "slots": 4, "repos": {"yaat": ["dotnet build*"]},
        "builders": [{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}}]}"#;
    let at_fallback = FakeFacts::with_config(Config::parse(edited).expect("usable"));
    assert!(
        matches!(later_of(&run, 0, &command, &at_fallback), Err(Some(_))),
        "win-2 is no longer listed, which is a reason, not a steer-off"
    );
    let host = later_of(&run, 1, &command, &at_fallback).expect("host can be tried");
    assert_eq!(host.builder, host_builder("builder-1", 4), "host as the routing file now reads");
    assert_eq!(host.pattern, "dotnet build*", "the pattern matched again from its fresh entry");
}

#[test]
fn a_later_candidate_the_local_file_now_keeps_here_is_skipped_with_its_reason() {
    let command = words(&["dotnet", "build"]);
    let config = with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(config.clone())));
    assert_eq!(later_aliases(&run), ["builder-1"]);
    let now = FakeFacts {
        local: Ok(patterns(&["dotnet build*"])),
        ..FakeFacts::with_config(config)
    };
    let skipped = later_of(&run, 0, &command, &now);
    assert!(
        matches!(&skipped, Err(Some(why)) if why.starts_with(".gate/local.toml keeps it here")),
        "{skipped:?}"
    );
}

/// A heavy gate running `command`, pinned to the builder `host` with `--builder`.
fn pinned_to<'a>(command: &'a [String], host: &'a str) -> Inputs<'a> {
    Inputs {
        pin: Some(host),
        ..heavy(command)
    }
}

/// The refusal of a pinned decision, which must not route or run here.
fn refused(route: Route) -> PinRefusal {
    assert!(matches!(route, Route::Refused(_)), "expected a refused pin, got {route:?}");
    match route {
        Route::Refused(refusal) => refusal,
        Route::Local { .. } | Route::Routed(_) => unreachable!("asserted to be refused"),
    }
}

/// `mac-1` and `win-2`, both routed, under a `host` running `yaat`'s `dotnet` commands.
const MAC_AND_WIN: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet build*"]}},
    {"host": "win-2", "os": "windows", "slots": 3, "repos": {"yaat": ["cargo *", "dotnet build*"]}}]"#;

#[test]
fn a_pin_routes_to_the_named_builder_alone() {
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_AND_WIN));
    let run = routed(decide(&pinned_to(&command, "WIN-2"), &facts));
    assert_eq!(
        run.builder.alias, "win-2",
        "matched in any ASCII letter case, named as the file spells it"
    );
    assert!(run.later.is_empty(), "a pin has no next builder: {:?}", later_aliases(&run));
    assert_eq!(run.pattern, "dotnet build*", "the first pattern of the repo's routes that matches");
    let host = routed(decide(&pinned_to(&command, "builder-1"), &facts));
    assert_eq!(
        host.builder,
        host_builder("builder-1", 1),
        "host may be pinned too, ahead of the entries the pick prefers"
    );
    assert_eq!(host.later, []);
}

#[test]
fn a_pin_names_a_not_routed_builder() {
    let config = Config::parse(
        r#"{"host": "builder-1", "hosts": ["win-7"], "repos": {"yaat": ["dotnet *"]},
            "builders": [{"host": "mac-9", "os": "macos", "root": "/Users/t/gate", "routed": false}]}"#,
    )
    .expect("usable");
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(config.clone());
    assert_eq!(
        routed(decide(&heavy(&command), &facts)).builder.alias,
        "builder-1",
        "the pick never chooses a not-routed builder"
    );
    let run = routed(decide(&pinned_to(&command, "mac-9"), &FakeFacts::with_config(config.clone())));
    assert_eq!(
        (run.builder.alias.as_str(), run.builder.os, run.builder.root.as_deref()),
        ("mac-9", BuilderOs::Macos, Some("/Users/t/gate"))
    );
    assert_eq!(run.later, []);
    assert_eq!(run.pattern, "dotnet *");
    let hosts = routed(decide(&pinned_to(&command, "win-7"), &FakeFacts::with_config(config)));
    assert_eq!(
        hosts.builder,
        Builder {
            alias: "win-7".to_owned(),
            os: BuilderOs::Windows,
            root: None,
            slots: 1,
            id: Some("c1aaea5e".to_owned()),
        },
        "a hosts entry is a Windows builder with the default root and an id of its own, so it shares no pool with host"
    );
}

#[test]
fn a_pin_to_an_unlisted_host_is_refused() {
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_AND_WIN));
    let refusal = refused(decide(&pinned_to(&command, "nobody"), &facts));
    assert_eq!(refusal.line(), "gate: --builder nobody is not a builder remote.json lists");
    assert_eq!(facts.asked(), ["config"], "refused before git is asked");

    let none = FakeFacts {
        config: Ok(None),
        ..FakeFacts::at_root()
    };
    assert_eq!(
        refused(decide(&pinned_to(&command, "mac-1"), &none)).line(),
        "gate: --builder needs a usable remote.json: there is none"
    );
    let unusable = FakeFacts {
        config: Err(ConfigProblem {
            path: PathBuf::from(r"C:\l\gate\remote.json"),
            reason: "slots must be a whole number 1 to 16".to_owned(),
        }),
        ..FakeFacts::at_root()
    };
    assert_eq!(
        refused(decide(&pinned_to(&command, "mac-1"), &unusable)).line(),
        r"gate: --builder needs a usable remote.json: C:\l\gate\remote.json: slots must be a whole number 1 to 16"
    );
}

#[test]
fn a_pin_whose_command_the_repo_does_not_route_is_refused() {
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_AND_WIN));
    let npm = words(&["npm", "test"]);
    assert_eq!(
        refused(decide(&pinned_to(&npm, "win-2"), &facts)).line(),
        "gate: --builder win-2: this command is not one the repo routes (none of its patterns matches the command)"
    );
    let other = FakeFacts::with_config(with_builders(r#"{"other": ["*"]}"#, "[]"));
    assert_eq!(
        refused(decide(&pinned_to(&words(&["dotnet", "build"]), "builder-1"), &other)).line(),
        "gate: --builder builder-1: this command is not one the repo routes (neither builder-1 nor a routed builder lists repo yaat)"
    );
}

#[test]
fn a_pin_under_a_covering_parent_is_refused() {
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_AND_WIN));
    let covered = Inputs {
        covering_parent: true,
        ..pinned_to(&command, "win-2")
    };
    let refusal = refused(decide(&covered, &facts));
    assert_eq!(refusal, PinRefusal::covered("win-2"));
    assert_eq!(
        refusal.line(),
        "gate: --builder win-2 cannot route this run: a parent gate's slot covers this gate"
    );
    assert!(facts.asked().is_empty(), "asked {:?}", facts.asked());
}

#[test]
fn a_pin_of_a_backslash_command_to_a_mac_is_refused() {
    let facts = FakeFacts {
        config: Ok(Some(with_builders(r#"{"yaat": ["*"]}"#, TWO_MACS))),
        ..FakeFacts::at("")
    };
    let command = words(&["dotnet", "test", "--filter", r"Name~Foo\.Bar"]);
    assert_eq!(
        refused(decide(&pinned_to(&command, "mac-1"), &facts)).line(),
        r"gate: --builder mac-1 cannot route this run: a word still holds a \, which a macOS builder reads another way"
    );
    let desktop = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD, &["dotnet build*"], &[]);
    assert_eq!(
        refused(decide(&pinned_to(&words(&["dotnet", "build"]), "mac-1"), &desktop)).line(),
        "gate: --builder mac-1 cannot route this run: it asks for a desktop, which a macOS builder does not have"
    );
}

#[test]
fn a_pin_of_git_is_refused() {
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["*"]}"#, MAC_AND_WIN));
    for command in [words(&["git", "commit"]), words(&[r"C:\Program Files\Git\cmd\prek.exe", "run"])] {
        assert_eq!(
            refused(decide(&pinned_to(&command, "builder-1"), &facts)).line(),
            "gate: --builder builder-1 cannot route this run: git, prek and an empty command never route",
            "{command:?}"
        );
    }
}

#[test]
fn without_a_pin_the_choice_is_unchanged() {
    let command = words(&["dotnet", "build"]);
    let facts = FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_AND_WIN));
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder, mac_builder());
    assert_eq!(later_aliases(&run), ["win-2", "builder-1"]);
    assert_eq!(run.pattern, "dotnet build*");
}

/// A not-routed `mac-9` whose own entry runs `yaat`'s `dotnet build` commands, under a `host` whose `repos` is
/// `host_repos`.
fn trial_mac(host_repos: &str) -> Config {
    with_builders(
        host_repos,
        r#"[{"host": "mac-9", "os": "macos", "root": "/Users/t/gate", "routed": false, "repos": {"yaat": ["dotnet build*"]}}]"#,
    )
}

#[test]
fn a_pin_to_a_not_routed_builder_uses_its_own_repos_patterns() {
    let build = words(&["dotnet", "build"]);
    let test = words(&["dotnet", "test"]);
    let alone = trial_mac(r#"{"other": ["*"]}"#);
    let run = routed(decide(&pinned_to(&build, "mac-9"), &FakeFacts::with_config(alone.clone())));
    assert_eq!(
        (run.builder.alias.as_str(), run.pattern.as_str()),
        ("mac-9", "dotnet build*"),
        "a repo only the trial builder's own entry lists"
    );
    assert_eq!(run.later, []);
    assert_eq!(
        refused(decide(&pinned_to(&test, "mac-9"), &FakeFacts::with_config(alone.clone()))).line(),
        "gate: --builder mac-9: this command is not one the repo routes (none of its patterns matches the command)"
    );
    assert!(
        matches!(decide(&heavy(&build), &FakeFacts::with_config(alone)), Route::Local { reason: None }),
        "the pick never sends a run to it"
    );

    let beside_host = trial_mac(r#"{"yaat": ["dotnet *"]}"#);
    assert_eq!(
        routed(decide(&pinned_to(&build, "mac-9"), &FakeFacts::with_config(beside_host.clone()))).pattern,
        "dotnet build*",
        "its own pattern, not host's first match"
    );
    assert_eq!(
        refused(decide(&pinned_to(&test, "mac-9"), &FakeFacts::with_config(beside_host))).line(),
        "gate: --builder mac-9: this command is not one the repo routes (none of its patterns matches the command)",
        "host's pattern does not stand in for the trial builder's own"
    );
}

#[test]
fn a_pin_to_a_builder_whose_entry_lacks_the_repo_uses_the_first_routed_match() {
    let build = words(&["dotnet", "build"]);
    let config = with_builders(
        r#"{"yaat": ["dotnet *"]}"#,
        r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me/gate", "repos": {"yaat": ["dotnet build*"]}},
            {"host": "win-9", "os": "windows", "routed": false, "repos": {"other": ["*"]}}]"#,
    );
    let run = routed(decide(&pinned_to(&build, "win-9"), &FakeFacts::with_config(config)));
    assert_eq!(
        (run.builder.alias.as_str(), run.pattern.as_str()),
        ("win-9", "dotnet build*"),
        "the first match across the routes, mac-1's"
    );
    assert_eq!(run.later, []);
    let unlisted = with_builders(
        r#"{"other": ["*"]}"#,
        r#"[{"host": "win-9", "os": "windows", "routed": false, "repos": {"other": ["*"]}}]"#,
    );
    assert_eq!(
        refused(decide(&pinned_to(&build, "win-9"), &FakeFacts::with_config(unlisted))).line(),
        "gate: --builder win-9: this command is not one the repo routes (neither win-9 nor a routed builder lists repo yaat)"
    );
}

/// Two `builders` entries, `mac-1` and `mac-2`, both macOS, running every `yaat` command, under a `host` that does too.
const TWO_MACS: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["*"]}},
    {"host": "mac-2", "os": "macos", "root": "/n", "repos": {"yaat": ["*"]}}]"#;

#[test]
fn a_second_mac_keeps_the_slash_words_and_is_left_out_after_a_backslash_steer_off() {
    let facts = FakeFacts {
        config: Ok(Some(with_builders(r#"{"yaat": ["*"]}"#, TWO_MACS))),
        snapshot: words(&["src/Duel.sln"]),
        ..FakeFacts::at("")
    };
    let command = words(&["dotnet", "build", r"src\Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(later_aliases(&run), ["mac-2", "builder-1"]);
    let second = later_of(&run, 0, &command, &facts).expect("mac-2 can be tried");
    assert_eq!(second.builder.alias, "mac-2");
    assert_eq!(second.words, run.words, "the slash words are the same on every Mac");

    let command = words(&["dotnet", "test", "--filter", r"Name~Foo\.Bar"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the backslash word steers the pick off both Macs");
    assert_eq!(later_aliases(&run), ["mac-1", "mac-2"]);
    assert_eq!(
        later_of(&run, 0, &command, &facts),
        Err(None),
        "mac-1 is steered off again, saying nothing"
    );
    assert_eq!(later_of(&run, 1, &command, &facts), Err(None), "and so is mac-2");
}

#[test]
fn a_mac_candidate_is_left_out_of_a_desktop_command() {
    let command = words(&["dotnet", "build"]);
    let mac_last = r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet *"]}},
        {"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}}]"#;
    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, mac_last, &["dotnet build*"], &[]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "win-2");
    assert_eq!(later_aliases(&run), ["mac-1", "builder-1"]);
    assert_eq!(later_of(&run, 0, &command, &facts), Err(None), "a Mac has no desktop, which says nothing");
    let host = later_of(&run, 1, &command, &facts).expect("host can be tried");
    assert!(host.desktop);
}

#[test]
fn a_macos_pick_s_path_map_ends_its_box_tree_in_one_slash_while_host_s_keeps_the_backslash() {
    let key = work_folder_key(ROOT);
    let mac_tree = format!("/Users/b/gate-remote/work/{key}/");
    let trees = |tree: &str| FakeFacts {
        given_trees: Some(vec![(tree.to_owned(), r"X:\dev\yaat.wt\a\".to_owned())]),
        ..FakeFacts::with_config(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD))
    };
    let mac = routed(decide(&heavy(&words(&["dotnet", "build"])), &trees(&mac_tree)));
    assert_eq!(mac.builder, mac_builder());
    assert_eq!(
        mac.words.last().map(String::as_str),
        Some(format!(r"-p:PathMap=/Users/b/gate-remote/work/{key}/=X:\dev\yaat.wt\a").as_str())
    );

    let host = routed(decide(&heavy(&words(&["dotnet", "test"])), &trees(&box_tree(None))));
    assert_eq!(host.builder, host_builder("builder-1", 1));
    assert_eq!(host.words.last(), Some(&path_map()), "host's word is unchanged");
}

#[test]
fn a_macos_pick_writes_a_worktree_path_with_slashes_while_host_keeps_the_backslashes() {
    let command = words(&["dotnet", "build", r"X:\dev\yaat.wt\a\src\Duel.sln"]);
    let mac = with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(mac)));
    assert_eq!(run.words, stamped(&["dotnet", "build", "src/Duel.sln"]));
    assert_eq!(run.fallback, command, "the fallback runs here as given");

    let elsewhere = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["cargo *"]}}]"#;
    let host = with_builders(r#"{"yaat": ["dotnet *"]}"#, elsewhere);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(host)));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", r"src\Duel.sln"]));
}

#[test]
fn a_macos_pick_below_the_root_climbs_with_slashes() {
    let command = words(&["dotnet", "build", r"X:\dev\yaat.wt\a\lib\x.csproj", r"X:\dev\yaat.wt\a\src\"]);
    let facts = FakeFacts {
        config: Ok(Some(with_builders(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD))),
        ..FakeFacts::at("src/")
    };
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.words, stamped(&["dotnet", "build", "../lib/x.csproj", "."]));
    assert_eq!(run.subfolder, "src");
}

#[test]
fn a_builder_entry_s_outputs_are_its_own_and_a_macos_entry_without_them_brings_none_back() {
    let command = words(&["dotnet", "build"]);
    let top = r#"{"yaat": ["*"]}, "outputs": {"yaat": ["top/**"]}"#;
    let mac = with_builders(top, MAC_BUILD);
    assert_eq!(
        routed(decide(&heavy(&command), &FakeFacts::with_config(mac))).outputs,
        Vec::<String>::new()
    );

    let own = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["*"]}, "outputs": {"Yaat": ["bin/**"]}}]"#;
    let mac = with_builders(top, own);
    assert_eq!(routed(decide(&heavy(&command), &FakeFacts::with_config(mac))).outputs, ["bin/**"]);

    let windows = with_builders(top, r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["*"]}}]"#);
    let run = routed(decide(&heavy(&command), &FakeFacts::with_config(windows.clone())));
    assert_eq!(
        run.outputs,
        ["**/bin/**", "**/.godot/mono/temp/bin/**"],
        "the top-level outputs are host's alone"
    );
    let run = routed(decide(&heavy(&words(&["cargo", "build"])), &FakeFacts::with_config(windows)));
    assert_eq!(run.outputs, ["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"]);
}

#[test]
fn a_send_build_repo_s_no_build_test_picked_by_a_macos_builder_has_no_send_plan_and_drops_no_build() {
    let file = |mac_patterns: &str| {
        let text = format!(
            r#"{{"host": "builder-1", "repos": {{"yaat": ["dotnet *"]}}, "sendBuild": ["yaat"],
                "builders": [{{"host": "mac-1", "os": "macos", "root": "/m", "repos": {{"yaat": {mac_patterns}}}}}]}}"#
        );
        FakeFacts::with_config(Config::parse(&text).expect("usable"))
    };
    let command = words(&["dotnet", "test", "App.csproj", "-c", "Release", "--no-build"]);
    let run = routed(decide(&heavy(&command), &file(r#"["dotnet test*"]"#)));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(run.send, None, "the Mac builds the project itself");
    assert!(run.dropped_no_build);
    assert_eq!(run.words, ["dotnet", "test", "App.csproj", "-c", "Release", stamp().as_str()]);

    let run = routed(decide(&heavy(&command), &file(r#"["cargo *"]"#)));
    assert_eq!(run.builder.alias, "builder-1");
    assert!(run.send.is_some(), "host still gets the send plan");
}

/// A gate at the root whose `.gate/desktop.toml` declares `declared` and whose `.gate/local.toml` declares `local`,
/// under a routing file whose `host` lists `yaat` as `host_repos` says, allowing the desktop pattern `dotnet build*` and
/// `dotnet build src\Duel.sln`, with the `builders` entries `builders`.
fn builder_desktop_facts(host_repos: &str, builders: &str, declared: &[&str], local: &[&str]) -> FakeFacts {
    let text = format!(
        r#"{{"host": "builder-1", "repos": {host_repos}, "builders": {builders},
            "desktop": {{"yaat": ["dotnet build*", "dotnet build src\\Duel.sln"]}}}}"#
    );
    FakeFacts {
        desktop: Ok(patterns(declared)),
        local: Ok(patterns(local)),
        ..FakeFacts::with_config(Config::parse(&text).expect("usable"))
    }
}

#[test]
fn a_command_matching_a_desktop_pattern_is_never_picked_by_a_macos_builder() {
    let command = words(&["dotnet", "build"]);
    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD, &["dotnet build*"], &[]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the pick goes on to host");
    assert!(run.desktop);
    assert_eq!(facts.desktop_read.get(), 1, "the desktop file is read once");

    let mac_then_windows = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}},
        {"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet build*"]}}]"#;
    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, mac_then_windows, &["dotnet build*"], &[]);
    assert_eq!(
        routed(decide(&heavy(&command), &facts)).builder.alias,
        "win-2",
        "the next entry that is no macOS one"
    );

    let facts = builder_desktop_facts(r#"{"other": ["*"]}"#, MAC_BUILD, &["dotnet build*"], &[]);
    assert_eq!(decide(&heavy(&command), &facts), SILENT, "no other builder runs it");

    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD, &["dotnet test*"], &[]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1", "a command no desktop pattern matches stays on the Mac");
    assert!(!run.desktop);
    assert_eq!(facts.desktop_read.get(), 1, "the desktop file is read once");
}

#[test]
fn on_a_macos_pick_a_local_pattern_written_with_backslashes_still_matches() {
    let command = words(&["dotnet", "build", r"X:\dev\yaat.wt\a\src\Duel.sln"]);
    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD, &[], &[r"dotnet build src\Duel.sln"]);
    let route = decide(&heavy(&command), &facts);
    assert!(
        matches!(&route, Route::Local { reason: Some(LocalReason::DeclaredLocal { pattern, .. }) } if pattern == r"dotnet build src\Duel.sln"),
        "{:?}",
        local_line(&route)
    );
}

#[test]
fn on_a_macos_pick_a_desktop_pattern_written_with_backslashes_still_matches() {
    let command = words(&["dotnet", "build", r"X:\dev\yaat.wt\a\src\Duel.sln"]);
    let facts = builder_desktop_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_BUILD, &[r"dotnet build src\Duel.sln"], &[]);
    let run = routed(decide(&heavy(&command), &facts));
    assert!(run.desktop);
    assert_eq!(run.builder.alias, "builder-1", "the desktop command leaves the Mac");
    assert_eq!(run.words, stamped(&["dotnet", "build", r"src\Duel.sln"]));
}

/// A macOS builder `mac-1`, then a Windows builder `win-2`, each running `yaat`'s `dotnet test` commands.
const MAC_THEN_WINDOWS_TEST: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet test*"]}},
    {"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet test*"]}}]"#;

/// A gate at the root whose `dotnet test` test projects are Windows-only when `windows_only`, under a routing file whose
/// `host` lists `yaat` as `host_repos` says, with the `builders` entries `builders`.
fn windows_only_facts(host_repos: &str, builders: &str, windows_only: bool) -> FakeFacts {
    FakeFacts {
        windows_only,
        ..FakeFacts::with_config(with_builders(host_repos, builders))
    }
}

#[test]
fn a_macos_pick_of_a_windows_only_test_goes_on_to_the_next_builder_that_is_no_macos_one() {
    for verb in ["test", "TEST"] {
        let command = words(&["dotnet", verb, "tests/X"]);
        let facts = windows_only_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_TEST, true);
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.builder.alias, "win-2", "the Windows-only test leaves the Mac");
        assert_eq!(facts.windows_only_asked.get(), 1, "the projects are read once");
    }
}

#[test]
fn a_macos_pick_of_a_windows_only_test_no_other_builder_runs_stays_here() {
    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet test*"]}}]"#;
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, mac, true);
    assert_eq!(decide(&heavy(&words(&["dotnet", "test", "tests/X"])), &facts), SILENT);
}

#[test]
fn a_macos_pick_of_a_test_not_windows_only_stays_on_the_mac_unless_its_words_cannot_be_read() {
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_TEST, false);
    let run = routed(decide(&heavy(&words(&["dotnet", "test", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(facts.windows_only_asked.get(), 1);

    let facts = windows_only_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_TEST, false);
    let run = routed(decide(&heavy(&words(&["dotnet", "test", "@args.rsp"])), &facts));
    assert_eq!(
        run.builder.alias, "win-2",
        "a response file's words cannot be read, so the test leaves the Mac"
    );
    assert_eq!(facts.windows_only_asked.get(), 0, "nothing is read for it");
}

#[test]
fn a_build_or_a_pick_of_no_macos_builder_never_asks_whether_a_test_is_windows_only() {
    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}}]"#;
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, mac, true);
    let run = routed(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "mac-1", "a build is no test");
    assert_eq!(facts.windows_only_asked.get(), 0);

    let windows = r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet test*"]}}]"#;
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, windows, true);
    let run = routed(decide(&heavy(&words(&["dotnet", "test", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "win-2");
    assert_eq!(facts.windows_only_asked.get(), 0, "a Windows builder runs it without reading");
}

#[test]
fn a_macos_later_candidate_of_a_windows_only_test_is_skipped_silently() {
    let command = words(&["dotnet", "test", "tests/X"]);
    let mac = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet test*"]}}]"#;
    let facts = windows_only_facts(r#"{"yaat": ["dotnet test*"]}"#, mac, true);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the pick leaves the Mac");
    assert_eq!(later_aliases(&run), ["mac-1"]);
    assert!(
        matches!(later_of(&run, 0, &command, &facts), Err(None)),
        "host turned the run away, and the Mac is still no run for it"
    );

    let windows_then_mac = r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet test*"]}},
        {"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet test*"]}}]"#;
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, windows_then_mac, true);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "win-2");
    assert!(matches!(later_of(&run, 0, &command, &facts), Err(None)), "the Mac is never tried for it");
}

#[test]
fn a_pin_to_a_macos_builder_of_a_windows_only_test_is_refused() {
    let facts = windows_only_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_TEST, true);
    assert_eq!(
        refused(decide(&pinned_to(&words(&["dotnet", "test", "tests/X"]), "mac-1"), &facts)).line(),
        "gate: --builder mac-1 cannot route this run: it may test a Windows-only project, which a macOS builder cannot run"
    );
}

/// A macOS builder `mac-1`, then a Windows builder `win-2`, each running `yaat`'s `dotnet` commands.
const MAC_THEN_WINDOWS_DOTNET: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}},
    {"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet *"]}}]"#;

/// A macOS builder `mac-1` alone, running `yaat`'s `dotnet` commands.
const MAC_DOTNET: &str = r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"yaat": ["dotnet *"]}}]"#;

/// The change touching `tools/Hook/a.cs`, one of `.gate/windows.toml`'s paths.
fn touched() -> WindowsChange {
    WindowsChange::Touched {
        path: "tools/Hook/a.cs".to_owned(),
    }
}

/// A change whose touch of `.gate/windows.toml`'s paths cannot be told.
fn unknown() -> WindowsChange {
    WindowsChange::Unknown {
        why: "no refs/remotes/origin/HEAD or refs/remotes/origin/main".to_owned(),
    }
}

#[test]
fn a_sibling_that_is_a_junction_or_not_allowed_steers_nothing_and_reads_no_change() {
    let command = words(&["dotnet", "build", "tests/X"]);
    let config = |allowed: &str| {
        let text = format!(
            r#"{{"host": "builder-1", "repos": {{"other": ["*"]}}, "builders": {MAC_THEN_WINDOWS_DOTNET}, "siblings": {{"yaat": [{allowed}]}}}}"#
        );
        Config::parse(&text).expect("usable")
    };
    for (allowed, folder, reason) in [
        (
            r#""b""#,
            SiblingFolder::Refused("is a link or a junction".to_owned()),
            "sibling b is a link or a junction",
        ),
        (
            r#""c""#,
            SiblingFolder::Present(fake_id(&sibling_root("b"))),
            "sibling b is not allowed in remote.json",
        ),
    ] {
        let facts = FakeFacts {
            siblings: Ok(words(&["b"])),
            folders: vec![(sibling_root("b"), Ok(folder))],
            windows_change: touched(),
            ..FakeFacts::with_config(config(allowed))
        };
        let line = refused(decide(&pinned_to(&command, "mac-1"), &facts)).line();
        assert_eq!(
            line,
            format!("gate: --builder mac-1 cannot route this run: {reason}"),
            "the sibling's own reason, not a change the gate cannot tell"
        );
        assert_eq!(facts.windows_change_asked.get(), 0, "nothing is read in the sibling: {line}");
    }
}

#[test]
fn a_mac_only_builder_with_a_refused_sibling_and_no_windows_file_says_the_sibling() {
    let config = Config::parse(&format!(
        r#"{{"host": "builder-1", "repos": {{"other": ["*"]}}, "builders": {MAC_DOTNET}, "siblings": {{"yaat": ["b"]}}}}"#
    ))
    .expect("usable");
    let facts = FakeFacts {
        siblings: Ok(words(&["b"])),
        folders: vec![(sibling_root("b"), Ok(SiblingFolder::Refused("is a link or a junction".to_owned())))],
        ..FakeFacts::with_config(config)
    };
    let route = decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts);
    assert_eq!(
        local_line(&route).as_deref(),
        Some("gate: not routed: sibling b is a link or a junction"),
        "no other builder to steer to, and the sibling's own reason keeps it here"
    );
    assert_eq!(facts.windows_change_asked.get(), 0, "no .gate/windows.toml is read for it");
}

/// A gate at the root whose change is `change` to `.gate/windows.toml`'s paths, under a routing file whose `host`
/// lists `yaat` as `host_repos` says, with the `builders` entries `builders`.
fn windows_change_facts(host_repos: &str, builders: &str, change: WindowsChange) -> FakeFacts {
    FakeFacts {
        windows_change: change,
        ..FakeFacts::with_config(with_builders(host_repos, builders))
    }
}

#[test]
fn a_macos_pick_of_a_change_touching_the_windows_paths_or_not_known_to_goes_on_to_the_next_builder_that_is_no_macos_one() {
    for change in [touched(), unknown()] {
        for command in [words(&["dotnet", "build", "tests/X"]), words(&["dotnet", "test", "tests/X"])] {
            let facts = windows_change_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_DOTNET, change.clone());
            let run = routed(decide(&heavy(&command), &facts));
            assert_eq!(run.builder.alias, "win-2", "{change:?} {command:?} leaves the Mac");
            assert_eq!(facts.windows_change_asked.get(), 1, "the change is read once");
        }
    }
}

#[test]
fn a_macos_pick_of_a_change_touching_the_windows_paths_goes_to_host_else_stays_here() {
    let facts = windows_change_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_DOTNET, touched());
    let run = routed(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "builder-1", "host runs it when no other builder does");

    let facts = windows_change_facts(r#"{"other": ["*"]}"#, MAC_DOTNET, unknown());
    assert_eq!(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts), SILENT);
}

#[test]
fn a_macos_pick_of_a_change_untouched_stays_on_the_mac() {
    let facts = windows_change_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_DOTNET, WindowsChange::Untouched);
    let run = routed(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(facts.windows_change_asked.get(), 1);
}

#[test]
fn a_pick_of_no_macos_builder_never_asks_what_the_change_touches() {
    let windows = r#"[{"host": "win-2", "os": "windows", "repos": {"yaat": ["dotnet *"]}}]"#;
    let facts = windows_change_facts(r#"{"other": ["*"]}"#, windows, touched());
    let run = routed(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "win-2");
    assert_eq!(facts.windows_change_asked.get(), 0, "a Windows builder runs it without reading");

    let facts = windows_change_facts(r#"{"yaat": ["dotnet *"]}"#, "[]", touched());
    let run = routed(decide(&heavy(&words(&["dotnet", "build", "tests/X"])), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(facts.windows_change_asked.get(), 0, "host runs it without reading");
}

#[test]
fn a_macos_later_candidate_of_a_change_touching_the_windows_paths_is_skipped_silently() {
    let command = words(&["dotnet", "build", "tests/X"]);
    let facts = windows_change_facts(r#"{"yaat": ["dotnet *"]}"#, MAC_DOTNET, touched());
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the pick leaves the Mac");
    assert_eq!(later_aliases(&run), ["mac-1"]);
    assert!(
        matches!(later_of(&run, 0, &command, &facts), Err(None)),
        "host turned the run away, and the Mac is still no run for it"
    );
}

#[test]
fn a_pin_to_a_macos_builder_of_a_change_touching_the_windows_paths_is_refused_and_to_a_windows_one_is_not() {
    let command = words(&["dotnet", "build", "tests/X"]);
    for (change, why) in [
        (touched(), "the change touches tools/Hook/a.cs (.gate/windows.toml)"),
        (
            unknown(),
            "cannot tell whether the change touches .gate/windows.toml's paths: no refs/remotes/origin/HEAD or refs/remotes/origin/main",
        ),
    ] {
        let facts = windows_change_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_DOTNET, change);
        assert_eq!(
            refused(decide(&pinned_to(&command, "mac-1"), &facts)).line(),
            format!("gate: --builder mac-1 cannot route this run: {why}")
        );
    }

    let facts = windows_change_facts(r#"{"other": ["*"]}"#, MAC_THEN_WINDOWS_DOTNET, touched());
    let run = routed(decide(&pinned_to(&command, "win-2"), &facts));
    assert_eq!(run.builder.alias, "win-2");
    assert_eq!(facts.windows_change_asked.get(), 0);
}

/// A gate at `prefix` below the root whose snapshot carries `snapshot`, under a routing file whose `host` lists `yaat`
/// as `host_repos` says and whose one `builders` entry, `mac-1`, runs `yaat`'s commands matching `mac_patterns`.
fn mac_facts(prefix: &str, host_repos: &str, mac_patterns: &str, snapshot: &[&str]) -> FakeFacts {
    let mac = format!(r#"[{{"host": "mac-1", "os": "macos", "root": "/m", "repos": {{"yaat": {mac_patterns}}}}}]"#);
    FakeFacts {
        config: Ok(Some(with_builders(host_repos, &mac))),
        snapshot: words(snapshot),
        ..FakeFacts::at(prefix)
    }
}

/// The snapshot questions `facts` was asked, in order.
fn snapshot_asks(facts: &FakeFacts) -> Vec<String> {
    facts
        .asked()
        .into_iter()
        .filter(|question| question.starts_with("snapshot_carries"))
        .collect()
}

#[test]
fn a_godot_console_command_spelled_by_its_windows_path_picks_a_macos_builder_as_godot_console() {
    let command = words(&[
        r"F:\Godot\Godot_console.exe",
        "--headless",
        "--path",
        "src/X",
        "--build-solutions",
        "--quit",
    ]);
    let facts = mac_facts("", r#"{"yaat": ["*Godot_console*"]}"#, r#"["*Godot_console*"]"#, &[]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(
        run.words[..6],
        words(&["Godot_console", "--headless", "--path", "src/X", "--build-solutions", "--quit"]),
        "{:?}",
        run.words
    );
    assert!(run.words.iter().all(|word| !word.contains('\\')), "no backslash is left: {:?}", run.words);
    assert_eq!(run.fallback, command, "the fallback runs here as given");
}

#[test]
fn a_macos_pick_writes_a_relative_backslash_path_the_snapshot_carries_with_slashes() {
    let command = words(&["dotnet", "build", r"src\Duel.sln"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", "src/Duel.sln"]));
    assert_eq!(run.fallback, command, "the fallback runs here as given");
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries src/Duel.sln"], "asked once");
}

#[test]
fn a_relative_backslash_path_below_the_root_is_judged_from_the_gate_s_folder() {
    let command = words(&["dotnet", "test", r"tests\Foo.Tests"]);
    let facts = mac_facts(
        "src/",
        r#"{"yaat": ["dotnet *"]}"#,
        r#"["dotnet *"]"#,
        &["src/tests/Foo.Tests/Foo.Tests.csproj"],
    );
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(&run.words[..3], ["dotnet", "test", "tests/Foo.Tests"]);
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries src/tests/Foo.Tests"]);
}

#[test]
fn a_switch_s_value_after_its_equals_sign_is_written_with_slashes() {
    let command = words(&["dotnet", "build", r"--project=tests\Foo.Tests\Foo.Tests.csproj", r"-p:Thing=src\a.props"]);
    let snapshot = ["tests/Foo.Tests/Foo.Tests.csproj", "src/a.props"];
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &snapshot);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(
        run.words,
        stamped(&["dotnet", "build", "--project=tests/Foo.Tests/Foo.Tests.csproj", "-p:Thing=src/a.props"])
    );
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries tests/Foo.Tests/Foo.Tests.csproj src/a.props"]);
}

#[test]
fn a_backslash_glob_is_judged_by_the_folder_before_its_first_wildcard() {
    let command = words(&["dotnet", "build", r"tests\*\*.csproj"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["tests/a/a.csproj"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", "tests/*/*.csproj"]));
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries tests"]);
}

#[test]
fn a_backslash_word_naming_nothing_the_snapshot_carries_steers_the_command_off_the_mac() {
    let command = words(&["dotnet", "test", "--filter", r"Name~Foo\.Bar"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the pick goes on to host");
    assert!(run.words.starts_with(&command), "host runs the words as written: {:?}", run.words);
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries Name~Foo/.Bar"]);

    let facts = mac_facts("", r#"{"other": ["*"]}"#, r#"["dotnet *"]"#, &[]);
    assert_eq!(decide(&heavy(&command), &facts), SILENT, "no other builder runs it");
}

#[test]
fn an_output_path_the_snapshot_does_not_carry_steers_the_command_off_the_mac() {
    let command = words(&["dotnet", "build", "-o", r"bin\out"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", "-o", r"bin\out"]));
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries bin/out"]);
}

#[test]
fn a_host_pick_keeps_a_relative_backslash_path_and_asks_no_snapshot() {
    let command = words(&["dotnet", "build", r"src\Duel.sln"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["cargo *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", r"src\Duel.sln"]));
    assert_eq!(snapshot_asks(&facts), Vec::<String>::new());
}

#[test]
fn script_text_holding_a_backslash_steers_the_command_off_the_mac_and_asks_no_snapshot() {
    let command = words(&["pwsh", "-Command", r"dotnet build src\Duel.sln"]);
    let facts = mac_facts("", r#"{"yaat": ["pwsh *"]}"#, r#"["pwsh *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the Mac's pwsh would pass the backslash on as written");
    assert!(run.words.starts_with(&command), "{:?}", run.words);
    assert_eq!(snapshot_asks(&facts), Vec::<String>::new());
}

#[test]
fn a_script_file_s_own_dash_c_is_no_script_text_and_its_paths_are_written_with_slashes() {
    let command = words(&[
        "pwsh",
        "-NoProfile",
        "-File",
        r"tools\run.ps1",
        "--",
        "dotnet",
        "test",
        "-c",
        "Release",
        "--project",
        r"tests\Foo.Tests\Foo.Tests.csproj",
    ]);
    let snapshot = ["tools/run.ps1", "tests/Foo.Tests/Foo.Tests.csproj"];
    let facts = mac_facts("", r#"{"yaat": ["pwsh *"]}"#, r#"["pwsh *"]"#, &snapshot);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(run.words[3], "tools/run.ps1");
    assert_eq!(run.words[10], "tests/Foo.Tests/Foo.Tests.csproj");
    assert!(run.words.iter().all(|word| !word.contains('\\')), "{:?}", run.words);
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries tools/run.ps1 tests/Foo.Tests/Foo.Tests.csproj"]);
}

#[test]
fn a_list_word_holding_a_backslash_steers_the_command_off_the_mac() {
    let command = words(&["dotnet", "test", "--logger", r"trx;LogFileName=TestResults\run.trx"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["TestResults/run.trx"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert!(run.words.starts_with(&command), "host runs the words as written: {:?}", run.words);
    assert_eq!(snapshot_asks(&facts), Vec::<String>::new());
}

#[test]
fn an_msbuild_slash_switch_s_value_is_written_with_slashes() {
    let command = words(&["dotnet", "build", r"src\Duel.sln", r"/p:Props=src\a.props"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/Duel.sln", "src/a.props"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    assert_eq!(run.words, stamped(&["dotnet", "build", "src/Duel.sln", "/p:Props=src/a.props"]));
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries src/Duel.sln src/a.props"]);
}

#[test]
fn a_value_starting_with_a_slash_is_never_asked_of_the_snapshot() {
    let command = words(&["dotnet", "build", r"src\Duel.sln", r"/x\y"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/Duel.sln"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(snapshot_asks(&facts), ["snapshot_carries src/Duel.sln"]);
}

#[test]
fn a_unix_shell_s_script_text_starts_after_its_own_dash_c_alone() {
    let shells = r#"{"yaat": ["sh *", "bash *"]}"#;
    let mac = r#"["sh *", "bash *"]"#;

    let command = words(&["sh", r"tools\run.sh", "-c", r"out\x"]);
    let facts = mac_facts("", shells, mac, &["tools/run.sh", "out/x/a.txt"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1", "the script's own -c is no script text");
    assert_eq!(run.words, words(&["sh", "tools/run.sh", "-c", "out/x"]));

    let command = words(&["bash", "-C", r"tools\run.sh"]);
    let facts = mac_facts("", shells, mac, &["tools/run.sh"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1", "-C is no -c");
    assert_eq!(run.words, words(&["bash", "-C", "tools/run.sh"]));

    let command = words(&["bash", "-o", "pipefail", "-c", r"cat src\a.txt"]);
    let facts = mac_facts("", shells, mac, &["src/a.txt"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1", "the script's backslash steers");
    assert_eq!(
        snapshot_asks(&facts),
        Vec::<String>::new(),
        "the value of -o is skipped, so the script is never asked"
    );
}

#[test]
fn a_response_file_word_with_a_backslash_is_no_candidate_and_steers_off_the_mac() {
    let command = words(&["dotnet", "build", r"@src\args.rsp"]);
    let facts = mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &["src/args.rsp"]);
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "builder-1");
    assert_eq!(snapshot_asks(&facts), Vec::<String>::new());
}

#[test]
fn the_gate_s_own_path_map_word_never_steers_a_macos_build() {
    let command = words(&["dotnet", "build"]);
    let facts = mapped(mac_facts("", r#"{"yaat": ["dotnet *"]}"#, r#"["dotnet *"]"#, &[]));
    let run = routed(decide(&heavy(&command), &facts));
    assert_eq!(run.builder.alias, "mac-1");
    let last = run.words.last().map_or("", String::as_str);
    assert!(
        last.starts_with("-p:PathMap=") && last.contains('\\'),
        "the word holds a backslash: {last}"
    );
    assert_eq!(snapshot_asks(&facts), Vec::<String>::new());
}

#[test]
fn a_file_without_builders_gives_the_target_it_always_gave() {
    let facts = FakeFacts::at_root();
    let place = facts.place().expect("the place reads").expect("there is a worktree");
    assert_eq!(
        target(&crate::support::config(), place, &facts),
        Ok(Target {
            builder: host_builder("builder-1", 2),
            place: place_at(""),
            repo: "yaat".to_owned(),
            leaf: "a".to_owned(),
            root_id: fake_id(ROOT),
            siblings: Vec::new(),
            key: work_folder_key(ROOT),
            inputs: Vec::new(),
        })
    );
}

/// A gate at `prefix` below the root, under a routing file routing every command of `yaat`, whose facts report the
/// paths `ignored` as gitignored project inputs, each carried by one file of its own name holding one byte.
fn ignoring(prefix: &str, ignored: &[&str]) -> FakeFacts {
    FakeFacts {
        config: Ok(Some(routing_all())),
        gitignored: ignored
            .iter()
            .map(|path| GitignoredInput {
                path: (*path).to_owned(),
                files: Ok(vec![((*path).to_owned(), 1)]),
            })
            .collect(),
        ..FakeFacts::at(prefix)
    }
}

/// Each of `paths` as a routed run's input globs carry it: after [`CARRIED_INPUT`].
fn marked(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| format!("{CARRIED_INPUT}{path}")).collect()
}

#[test]
fn a_word_naming_a_gitignored_project_input_is_carried_in_the_run_s_inputs() {
    let cases: [(&[&str], &str, &str); 11] = [
        (
            &[
                "dotnet",
                "build",
                "-p:Configuration=Release;CustomAfterMicrosoftCommonTargets=.tmp/probe/probe.targets",
            ],
            "",
            ".tmp/probe/probe.targets",
        ),
        (
            &["dotnet", "build", "-p:CustomAfterMicrosoftCommonTargets=.tmp/a.targets;.tmp/b.targets"],
            "",
            ".tmp/b.targets",
        ),
        (&["dotnet", "test", ".tmp/probe/Probe.csproj"], "", ".tmp/probe/Probe.csproj"),
        (&["dotnet", "test", r".tmp\probe"], "", ".tmp/probe"),
        (
            &[
                "dotnet",
                "test",
                "tests/A.Tests",
                "-p:CustomAfterMicrosoftCommonTargets=.tmp/probe/probe.targets",
            ],
            "",
            ".tmp/probe/probe.targets",
        ),
        (&["dotnet", "build", r"/p:Extra=.tmp\x.props"], "", ".tmp/x.props"),
        (&["dotnet", "build", r"-p:.tmp\x.props"], "", ".tmp/x.props"),
        (&["dotnet", "test", "--settings", ".tmp/x.runsettings"], "", ".tmp/x.runsettings"),
        (&["dotnet", "build", "@.tmp/args.rsp"], "", ".tmp/args.rsp"),
        (&["dotnet", "test", r"X:\dev\yaat.wt\a\.tmp\probe\P.csproj"], "", ".tmp/probe/P.csproj"),
        (&["dotnet", "test", "./.tmp/P.CSPROJ"], "src/", "src/.tmp/P.CSPROJ"),
    ];
    for (command, prefix, ignored) in cases {
        let command = words(command);
        let facts = ignoring(prefix, &[ignored]);
        let run = routed(decide(&heavy(&command), &facts));
        assert_eq!(run.inputs, marked(&[ignored]), "{command:?}");
        let asked = facts.gitignored_asked.borrow();
        assert_eq!(asked.len(), 1, "asked once: {asked:?}");
        assert!(asked[0].split(' ').any(|path| path == ignored), "{asked:?}");
    }
}

#[test]
fn a_word_the_facts_do_not_call_gitignored_routes_as_before() {
    let cases: [(&[&str], &str); 5] = [
        (&["dotnet", "build", "-p:LogFile=.tmp/build.log"], ".tmp/build.log"),
        (&["dotnet", "test", ".tmp/probe/Probe.csproj"], ".tmp/probe/Probe.csproj"),
        (&["dotnet", "test", "src/A.csproj"], "src/A.csproj"),
        (&["dotnet", "test", "-c", "Release"], "Release"),
        (&["dotnet", "test", r"src\A\..\..\B.csproj"], "B.csproj"),
    ];
    for (command, judged) in cases {
        let command = words(command);
        let facts = ignoring("", &[]);
        routed(decide(&heavy(&command), &facts));
        let asked = facts.gitignored_asked.borrow();
        assert_eq!(asked.len(), 1, "asked once: {asked:?}");
        assert!(asked[0].split(' ').any(|path| path == judged), "the facts judge {judged}: {asked:?}");
    }
}

#[test]
fn a_gitignored_input_s_question_carries_the_repo_s_input_globs() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"yaat": ["*"]}, "inputs": {"yaat": [".tmp/**"]}}"#).expect("usable");
    let command = words(&["dotnet", "test", ".tmp/x.csproj"]);
    let facts = FakeFacts {
        config: Ok(Some(config)),
        ..FakeFacts::at_root()
    };
    routed(decide(&heavy(&command), &facts));
    assert_eq!(*facts.gitignored_asked.borrow(), [".tmp/x.csproj | .tmp/**"]);
}

#[test]
fn script_text_and_lone_switches_are_never_asked_about() {
    let command = words(&["pwsh", "-Command", "dotnet test .tmp/p.csproj"]);
    let facts = ignoring("", &[".tmp/p.csproj", "dotnet test .tmp/p.csproj"]);
    routed(decide(&heavy(&command), &facts));
    assert!(
        facts.gitignored_asked.borrow().is_empty(),
        "the script's text is not read: {:?}",
        facts.gitignored_asked
    );

    let command = words(&["dotnet", "test", "-c", "Release"]);
    let facts = ignoring("", &[]);
    routed(decide(&heavy(&command), &facts));
    assert_eq!(*facts.gitignored_asked.borrow(), ["Release | "], "a switch alone names no path");
}

#[test]
fn a_dotnet_output_option_s_value_is_never_a_candidate() {
    for command in [
        &["dotnet", "test", "App.Tests.csproj", "--results-directory", ".tmp"][..],
        &["dotnet", "test", "tests/A.Tests", "-o", ".tmp"],
        &["dotnet", "test", "--OUTPUT=.tmp"],
        &["dotnet", "build", "-bl:.tmp/x.props"],
        &["dotnet", "build", "-flp:LogFile=.tmp"],
        &["dotnet", "test", "--diag", ".tmp/x.props"],
    ] {
        let command = words(command);
        let facts = ignoring("", &[".tmp", ".tmp/x.props"]);
        routed(decide(&heavy(&command), &facts));
        let asked = facts.gitignored_asked.borrow();
        assert!(
            asked.iter().all(|question| !question.contains(".tmp")),
            "{command:?}: an output's value is not asked about: {asked:?}"
        );
    }
}

#[test]
fn no_word_after_a_separator_is_a_candidate() {
    for command in [
        &["dotnet", "run", "--project", "tools/gen", "--", ".tmp/All.sln"][..],
        &["cargo", "test", "--", ".tmp/All.sln"],
    ] {
        let command = words(command);
        let facts = ignoring("", &[".tmp/All.sln"]);
        routed(decide(&heavy(&command), &facts));
        let asked = facts.gitignored_asked.borrow();
        assert!(asked.iter().all(|question| !question.contains(".tmp")), "{command:?}: {asked:?}");
    }
}

#[test]
fn a_folder_is_a_candidate_only_after_a_dotnet_command_s_verb() {
    for command in [
        &["dotnet", "build"][..],
        &[r"C:\Program Files\dotnet\DOTNET.EXE", "build"],
        &["cargo", "build"],
        &["cargo", "build", "--target-dir", "build"],
        &["tool", "build", "x"],
    ] {
        let command = words(command);
        let facts = ignoring("", &["build", "dotnet", "x"]);
        let route = decide(&heavy(&command), &facts);
        assert!(
            !matches!(
                route,
                Route::Local {
                    reason: Some(LocalReason::GitignoredPastCap { .. } | LocalReason::GitignoredPair { .. })
                }
            ),
            "{command:?}: {route:?}"
        );
        let asked = facts.gitignored_asked.borrow();
        assert!(
            asked
                .iter()
                .all(|question| !question.split(' ').any(|path| path == "build" || path == "x")),
            "{command:?}: {asked:?}"
        );
    }
    let command = words(&["cargo", "build", "--manifest-path", ".tmp/p.props"]);
    let facts = ignoring("", &[".tmp/p.props"]);
    assert_eq!(
        routed(decide(&heavy(&command), &facts)).inputs,
        marked(&[".tmp/p.props"]),
        "a file candidate is taken from any command"
    );
}

/// A gate at the root under a routing file routing every command of `yaat` to `builder-1` with the input globs
/// `inputs`, whose facts report each `(path, files)` of `ignored` as a gitignored project input carried by `files`.
fn carrying(inputs: &[&str], ignored: &[(&str, &[&str])]) -> FakeFacts {
    let globs = inputs.iter().map(|glob| format!("{glob:?}")).collect::<Vec<_>>().join(", ");
    let text = format!(r#"{{"host": "builder-1", "repos": {{"yaat": ["*"]}}, "inputs": {{"yaat": [{globs}]}}}}"#);
    FakeFacts {
        config: Ok(Some(Config::parse(&text).expect("usable"))),
        gitignored: ignored
            .iter()
            .map(|(path, files)| GitignoredInput {
                path: (*path).to_owned(),
                files: Ok(files.iter().map(|file| ((*file).to_owned(), 1)).collect()),
            })
            .collect(),
        ..FakeFacts::at_root()
    }
}

/// The files of the probe project `.tmp/probe`.
const PROBE: [&str; 2] = [".tmp/probe/Probe.csproj", ".tmp/probe/Program.cs"];

#[test]
fn a_gitignored_project_a_command_names_is_carried_in_the_run_s_inputs() {
    for (command, path) in [
        (&["dotnet", "test", ".tmp/probe/Probe.csproj"][..], ".tmp/probe/Probe.csproj"),
        (&["dotnet", "test", r".tmp\probe"], ".tmp/probe"),
    ] {
        let command = words(command);
        let facts = carrying(&["keys.json"], &[(path, &PROBE)]);
        let run = routed(decide(&heavy(&command), &facts));
        let mut expected = vec!["keys.json".to_owned()];
        expected.extend(marked(&PROBE));
        assert_eq!(run.inputs, expected, "{command:?}");
    }
}

#[test]
fn a_gitignored_input_an_inputs_glob_covers_is_not_carried_twice() {
    let command = words(&["dotnet", "build", ".tmp/probe/Probe.csproj", "-p:Extra=.tmp/probe/p.props"]);
    let facts = carrying(
        &[".tmp/probe/Program.cs"],
        &[
            (".tmp/probe/Probe.csproj", &PROBE),
            (".tmp/probe/p.props", &[".tmp/probe/p.props", PROBE[1]]),
        ],
    );
    let run = routed(decide(&heavy(&command), &facts));
    let mut expected = vec![PROBE[1].to_owned()];
    expected.extend(marked(&[PROBE[0], ".tmp/probe/p.props"]));
    assert_eq!(run.inputs, expected, "each path once");
}

/// The bytes of a mebibyte.
const MIB: u64 = 1024 * 1024;

/// A gate at the root under a routing file routing every command of `yaat`, whose facts report `.tmp/probe` as a
/// gitignored project input carried by `files`, each a path and its size.
fn probe_of(files: Vec<(String, u64)>) -> FakeFacts {
    FakeFacts {
        gitignored: vec![GitignoredInput {
            path: ".tmp/probe".to_owned(),
            files: Ok(files),
        }],
        ..ignoring("", &[])
    }
}

/// `count` files of `size` bytes each in `.tmp/probe`.
fn probe_files(count: usize, size: u64) -> Vec<(String, u64)> {
    (0..count).map(|index| (format!(".tmp/probe/{index}.cs"), size)).collect()
}

/// The files of a probe past the byte cap: 64 MiB and one byte, in two files.
fn past_bytes() -> Vec<(String, u64)> {
    vec![(".tmp/probe/Probe.csproj".to_owned(), 64 * MIB), (".tmp/probe/big.dmp".to_owned(), 1)]
}

/// The reason a run whose probe's files hold `held` is kept here for.
fn past_cap(held: &str) -> String {
    format!(
        r"{} is gitignored and a command's inputs carry at most 64 MiB in 2000 files; its files hold {held}",
        r".tmp\probe"
    )
}

#[test]
fn a_gitignored_input_past_the_size_cap_keeps_the_command_here_saying_so() {
    for (files, held) in [(past_bytes(), "65 MiB in 2 files"), (probe_files(2001, 1), "1 MiB in 2001 files")] {
        let command = words(&["dotnet", "test", r".tmp\probe", "-p:Extra=.tmp/x.props"]);
        let facts = probe_of(files);
        assert_eq!(
            local_line(&decide(&heavy(&command), &facts)),
            Some(format!("gate: not routed: {}", past_cap(held))),
            "{held}"
        );
        assert_eq!(head_asks(&facts), 0, "a command kept here asks no HEAD");
    }
}

#[test]
fn a_gitignored_input_at_the_cap_is_carried() {
    let at_bytes = vec![(".tmp/probe/Probe.csproj".to_owned(), 64 * MIB - 1), (".tmp/probe/big.dmp".to_owned(), 1)];
    for files in [at_bytes, probe_files(2000, 1)] {
        let count = files.len();
        let command = words(&["dotnet", "test", r".tmp\probe"]);
        let run = routed(decide(&heavy(&command), &probe_of(files)));
        assert_eq!(run.inputs.len(), count, "every file is carried");
    }
}

#[test]
fn a_candidate_that_cannot_be_sent_keeps_the_command_here_saying_why() {
    let command = words(&["dotnet", "test", ".tmp/a.props", r".tmp\probe"]);
    let facts = FakeFacts {
        gitignored: vec![
            GitignoredInput {
                path: ".tmp/a.props".to_owned(),
                files: Ok(vec![(".tmp/a.props".to_owned(), 1)]),
            },
            GitignoredInput {
                path: ".tmp/probe".to_owned(),
                files: Err("it is inside a nested repository".to_owned()),
            },
        ],
        ..ignoring("", &[])
    };
    assert_eq!(
        local_line(&decide(&heavy(&command), &facts)).as_deref(),
        Some(r"gate: not routed: .tmp\probe is gitignored and its files cannot be sent: it is inside a nested repository")
    );
    assert_eq!(head_asks(&facts), 0, "a command kept here asks no HEAD");
}

/// A gate at the root of a worktree with the sibling `b`, whose facts report `.tmp/probe` as a gitignored project
/// input carried by its project file.
fn pair_probe() -> FakeFacts {
    FakeFacts {
        gitignored: vec![GitignoredInput {
            path: ".tmp/probe".to_owned(),
            files: Ok(vec![(".tmp/probe/Probe.csproj".to_owned(), 1)]),
        }],
        ..FakeFacts::with_siblings("", &["b"], &["b"])
    }
}

/// The reason a sibling pair's run naming the probe is kept here for.
const PAIR_PROBE: &str = r".tmp\probe is gitignored, and a sibling pair does not send inputs yet";

#[test]
fn a_sibling_pair_s_gitignored_input_keeps_the_command_here_saying_so() {
    let command = words(&["dotnet", "test", r".tmp\probe"]);
    let facts = pair_probe();
    assert_eq!(
        local_line(&decide(&heavy(&command), &facts)),
        Some(format!("gate: not routed: {PAIR_PROBE}"))
    );
    assert_eq!(head_asks(&facts), 0, "a command kept here asks no HEAD");
}

#[test]
fn a_pinned_run_carries_a_gitignored_input() {
    let command = words(&["dotnet", "test", ".tmp/probe/Probe.csproj"]);
    let facts = carrying(&[], &[(".tmp/probe/Probe.csproj", &PROBE)]);
    let run = routed(decide(&pinned_to(&command, "builder-1"), &facts));
    assert_eq!(run.inputs, marked(&PROBE));
}

#[test]
fn a_pinned_run_past_the_size_cap_cannot_route_saying_why() {
    let command = words(&["dotnet", "test", r".tmp\probe"]);
    let facts = probe_of(past_bytes());
    assert_eq!(
        local_line(&decide(&pinned_to(&command, "builder-1"), &facts)),
        Some(format!(
            "gate: --builder builder-1 cannot route this run: {}",
            past_cap("65 MiB in 2 files")
        ))
    );
}

#[test]
fn a_pinned_pair_run_naming_a_gitignored_input_cannot_route_saying_why() {
    let command = words(&["dotnet", "test", r".tmp\probe"]);
    let facts = pair_probe();
    assert_eq!(
        local_line(&decide(&pinned_to(&command, "builder-1"), &facts)),
        Some(format!("gate: --builder builder-1 cannot route this run: {PAIR_PROBE}"))
    );
}

#[test]
fn a_word_outside_the_worktree_wins_over_a_gitignored_input() {
    let command = words(&["dotnet", "test", ".tmp/p.csproj", r"..\other\x.csproj"]);
    let facts = ignoring("", &[".tmp/p.csproj"]);
    assert_eq!(
        local_line(&decide(&heavy(&command), &facts)).as_deref(),
        Some(r"gate: not routed: ..\other\x.csproj is outside the worktree")
    );
    assert!(facts.gitignored_asked.borrow().is_empty(), "{:?}", facts.gitignored_asked);
}
