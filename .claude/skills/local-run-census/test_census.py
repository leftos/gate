"""Tests of the census: command families, repo names, path scrubbing, the journal reader and the figures."""

import gzip
import json
import re
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

import census

START = datetime(2026, 10, 20, tzinfo=UTC)
END = datetime(2026, 10, 27, tzinfo=UTC)
WT = r"X:\dev\demo.wt\tree"
MACHINE_PATH = re.compile(
    r"[A-Za-z]:[\\/]|\\\\|(?:^|[\s\"'=(`])/[^\s/]+/|~[\\/]|(?<!\.)\.\.[\\/]|Users"
)


def stamp(moment: datetime) -> str:
    return moment.strftime("%Y-%m-%dT%H:%M:%S.0000000Z")


def line(event: str, pid: int, at: datetime, **fields: object) -> dict[str, object]:
    base: dict[str, object] = {
        "v": 1,
        "time": stamp(at),
        "event": event,
        "pid": pid,
        "processStart": stamp(START - timedelta(days=30) + timedelta(seconds=pid)),
        "kind": "heavy",
        "cwd": WT,
        "command": "cargo test",
    }
    base.update(fields)
    return base


def journal_of(*events: dict[str, object]) -> census.Journal:
    return census.Journal(events=list(events), files=["events.jsonl"])


def write_lines(path: Path, lines: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(text + "\n" for text in lines), encoding="utf-8", newline="\n"
    )


class FamilyTests(unittest.TestCase):
    """The family rules, one case per rule, plus the commands the callers run most."""

    def test_a_shell_family_is_the_program_plus_its_script(self) -> None:
        self.assertEqual(
            census.family_of(r"pwsh -NoProfile -File tools\build.ps1 -Fast"),
            "pwsh tools/build.ps1",
        )

    def test_a_shell_without_a_script_is_the_program_plus_its_inline_flag(self) -> None:
        self.assertEqual(
            census.family_of('bash -c "cargo test --workspace"'), "bash -c"
        )
        self.assertEqual(
            census.family_of("pwsh -NoProfile -command Get-Date"), "pwsh -Command"
        )

    def test_nothing_after_an_inline_flag_is_read(self) -> None:
        self.assertEqual(
            census.family_of(
                r'pwsh -Command "& C:\Users\First Last\t.ps1" tools/after.ps1'
            ),
            "pwsh -Command",
        )
        self.assertEqual(census.family_of("cmd /c build.cmd"), "cmd /c")

    def test_a_shell_with_neither_is_the_program_alone(self) -> None:
        self.assertEqual(census.family_of("python -V"), "python")

    def test_python_m_is_python_m_plus_its_module(self) -> None:
        self.assertEqual(
            census.family_of("python -I -m pytest -q tests/x.py"), "python -m pytest"
        )
        self.assertEqual(census.family_of("py -m http.server"), "py -m http.server")

    def test_node_reads_only_up_to_its_first_non_flag(self) -> None:
        self.assertEqual(
            census.family_of(
                "node --max-old-space-size=4096 scripts/build.js cargo clippy x.js"
            ),
            "node scripts/build.js",
        )
        self.assertEqual(census.family_of("node server tools/x.js"), "node")

    def test_uv_run_is_uv_run_plus_its_target_skipping_flag_values(self) -> None:
        self.assertEqual(
            census.family_of("uv run --with ruff --python 3.12 --frozen pytest -q"),
            "uv run pytest",
        )

    def test_uv_run_skips_every_value_flag_and_flag_equals_value(self) -> None:
        command = "uv run --directory tools/linear --group dev --extra x --env-file=.env --package p -p 3.12 pytest -q"
        self.assertEqual(census.family_of(command), "uv run pytest")
        self.assertEqual(
            census.family_of(
                "uv run --with-requirements r.txt --index https://x/simple ruff"
            ),
            "uv run ruff",
        )

    def test_uv_run_python_takes_the_script_rule(self) -> None:
        self.assertEqual(
            census.family_of("uv run --no-project python -I tools/x/census.py"),
            "uv run python tools/x/census.py",
        )

    def test_cargo_skips_a_toolchain_word(self) -> None:
        self.assertEqual(
            census.family_of("cargo +nightly clippy --workspace"), "cargo clippy"
        )

    def test_cargo_skips_global_flags_and_their_values(self) -> None:
        self.assertEqual(
            census.family_of(
                r"cargo --manifest-path C:\x\Cargo.toml -Z unstable-options test"
            ),
            "cargo test",
        )
        self.assertEqual(
            census.family_of(
                "cargo -C sub --color never --target-dir t --config k=v build"
            ),
            "cargo build",
        )

    def test_another_program_is_the_program_plus_its_first_plain_word(self) -> None:
        self.assertEqual(census.family_of("just ci"), "just ci")

    def test_another_program_whose_first_plain_word_is_a_path_is_the_program_alone(
        self,
    ) -> None:
        self.assertEqual(census.family_of("npx ./node_modules/x"), "npx")
        self.assertEqual(census.family_of("godot --path project.godot"), "godot")

    def test_a_script_outside_the_repo_is_shortened(self) -> None:
        self.assertEqual(census.family_of(r"pwsh -File ..\x\t.ps1"), "pwsh .../t.ps1")
        self.assertEqual(
            census.family_of(r"pwsh -File C:\Users\Someone\t.ps1"), "pwsh .../t.ps1"
        )

    def test_cargo_nextest(self) -> None:
        self.assertEqual(
            census.family_of("cargo nextest run --workspace"), "cargo nextest"
        )

    def test_dotnet_test(self) -> None:
        self.assertEqual(
            census.family_of("dotnet test X.slnx -c Release"), "dotnet test"
        )

    def test_pwsh_test_all(self) -> None:
        self.assertEqual(
            census.family_of("pwsh -NoProfile -File tools/test-all.ps1 -Config Debug"),
            "pwsh tools/test-all.ps1",
        )

    def test_uv_run_project_python_module(self) -> None:
        self.assertEqual(
            census.family_of("uv run --project tools/x python -m pytest"),
            "uv run python -m pytest",
        )

    def test_a_quoted_program_path(self) -> None:
        self.assertEqual(
            census.family_of(r'"C:\Program Files\Godot\godot.exe" --headless --import'),
            "godot",
        )


class ScrubTests(unittest.TestCase):
    """Machine paths in free text become ``<path>``."""

    def test_a_drive_path(self) -> None:
        self.assertEqual(census.scrub(r"see C:\dev\x\y.txt now"), "see <path> now")

    def test_a_unc_path(self) -> None:
        self.assertEqual(census.scrub(r"copy \\box\share\f to"), "copy <path> to")

    def test_a_mnt_path(self) -> None:
        self.assertEqual(census.scrub("cd /mnt/c/work && go"), "cd <path> && go")

    def test_a_home_path(self) -> None:
        self.assertEqual(census.scrub("read ~/notes/a.md first"), "read <path> first")

    def test_a_path_with_a_space_does_not_leak_its_tail(self) -> None:
        self.assertEqual(
            census.scrub(r"C:\Users\First Last\x -c Release"), "<path> -c Release"
        )

    def test_a_path_ends_at_a_quote(self) -> None:
        self.assertEqual(census.scrub(r'"C:\Program Files\x.exe" run'), '"<path>" run')

    def test_switches_and_shortened_names_stay(self) -> None:
        self.assertEqual(census.scrub("cmd /c .../t.ps1 /p:x"), "cmd /c .../t.ps1 /p:x")


class RepoTests(unittest.TestCase):
    """The repo a working folder belongs to."""

    def test_a_worktree_names_its_repo(self) -> None:
        self.assertEqual(census.repo_of(r"X:\dev\gate.wt\gate-831"), "gate")

    def test_a_claude_worktree_names_claude_config(self) -> None:
        self.assertEqual(
            census.repo_of(r"C:\Users\someone\.claude-wt\cc-484"), "claude-config"
        )

    def test_a_missing_plain_checkout_is_gone(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(
                census.repo_of(str(Path(root) / "gone" / "my-repo")), "(gone)"
            )

    def test_a_folder_in_no_repo_is_no_repo(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(census.repo_of(root), "(no repo)")

    def test_a_common_dir_names_its_checkout_or_superproject(self) -> None:
        self.assertEqual(census.repo_of_common_dir("X:/dev/gate/.git"), "gate")
        self.assertEqual(
            census.repo_of_common_dir("X:/dev/super/.git/modules/sub"), "super"
        )


class JournalTests(unittest.TestCase):
    """The journal reader over fixture files."""

    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.folder = Path(self._dir.name)

    def tearDown(self) -> None:
        self._dir.cleanup()

    def test_the_v_rule_nul_lines_and_bad_lines_are_skipped_and_counted(self) -> None:
        good = line(
            "release", 1, START + timedelta(days=1), wallSeconds=5.0, verdict="passed"
        )
        no_v = {
            key: value for key, value in line("queue", 1, START).items() if key != "v"
        }
        write_lines(
            self.folder / "events.jsonl",
            [
                json.dumps(good),
                json.dumps(no_v),
                json.dumps({**good, "v": 2}),
                "\x00\x00\x00\x00",
                "{not json",
                "",
            ],
        )
        journal = census.read_journal(self.folder)
        self.assertEqual(
            [event["event"] for event in journal.events], ["release", "queue"]
        )
        self.assertEqual(journal.skipped, 3)

    def test_a_line_in_the_archive_and_events_1_is_read_once(self) -> None:
        release = json.dumps(
            line(
                "release",
                1,
                START + timedelta(days=1),
                wallSeconds=5.0,
                verdict="passed",
            )
        )
        archive = self.folder / "archive" / "events-20261021T000000Z.jsonl.gz"
        archive.parent.mkdir()
        archive.write_bytes(gzip.compress((release + "\n").encode()))
        write_lines(self.folder / "events.1.jsonl", [release])
        journal = census.read_journal(self.folder)
        self.assertEqual(len(journal.events), 1)
        self.assertEqual(journal.duplicates, 1)
        self.assertEqual(
            journal.files,
            ["archive/events-20261021T000000Z.jsonl.gz", "events.1.jsonl"],
        )

    def test_a_truncated_archive_is_listed_and_the_census_goes_on(self) -> None:
        lines = [
            json.dumps(line("release", pid, START + timedelta(days=1)))
            for pid in range(200)
        ]
        data = gzip.compress("\n".join(lines).encode())
        archive = self.folder / "archive" / "events-20261021T000000Z.jsonl.gz"
        archive.parent.mkdir()
        archive.write_bytes(data[: len(data) // 2])
        write_lines(
            self.folder / "events.jsonl",
            [json.dumps(line("release", 999, START + timedelta(days=2)))],
        )
        journal = census.read_journal(self.folder)
        self.assertEqual(
            journal.unreadable, ["archive/events-20261021T000000Z.jsonl.gz"]
        )
        self.assertIn(999, [event["pid"] for event in journal.events])


class ReleaseFactsTests(unittest.TestCase):
    """The routing facts a run's ``release`` line keeps once the archive rolled its other lines away."""

    @staticmethod
    def census_of(*events: dict[str, object]) -> dict[str, object]:
        return census.census_data(journal_of(*events), START, END, census.RepoNamer())

    @staticmethod
    def rows(data: dict) -> dict[tuple[str, str], dict]:
        return {(row["repo"], row["family"]): row for row in data["heavy"]}

    def test_archive_day_reads_not_routed_and_reason_from_release(self) -> None:
        at = START + timedelta(hours=2)
        data = self.census_of(
            line(
                "release",
                30,
                at,
                notRouted="--local",
                reason="built here",
                wallSeconds=10.0,
                verdict="passed",
            )
        )
        self.assertEqual(
            self.rows(data)[("demo", "cargo test")]["notRouted"], {"--local": 1}
        )
        self.assertEqual(
            data["local"],
            [{"repo": "demo", "family": "cargo test", "reason": "built here", "runs": 1}],
        )

    def test_archive_day_counts_fallbacks_from_release_fell_back(self) -> None:
        at = START + timedelta(hours=2)
        data = self.census_of(
            line(
                "release",
                31,
                at,
                fellBack=["full", "busy"],
                wallSeconds=10.0,
                verdict="passed",
            )
        )
        self.assertEqual(
            self.rows(data)[("demo", "cargo test")]["fallbacks"],
            {"full": 1, "busy": 1},
        )

    def test_archive_day_links_a_nested_gate_by_release_parent_pid(self) -> None:
        at = START + timedelta(hours=2)
        data = self.census_of(
            line(
                "release",
                40,
                at - timedelta(seconds=10),
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                41,
                at - timedelta(seconds=20),
                parentPid=40,
                wallSeconds=60.0,
                verdict="passed",
                command=BUILD_COMMAND,
            ),
        )
        self.assertEqual(
            self.rows(data)[("demo", "dotnet build")]["local"],
            1,
        )
        self.assertEqual(data["commitSplit"]["demo"]["linkedByParentPid"], 1)

    def test_straddling_run_counts_its_fallbacks_from_release(self) -> None:
        at = START + timedelta(hours=2)
        data = self.census_of(
            line("take", 32, at - timedelta(seconds=30)),
            line(
                "release",
                32,
                at,
                fellBack=["full", "busy"],
                wallSeconds=10.0,
                verdict="passed",
            ),
        )
        self.assertEqual(
            self.rows(data)[("demo", "cargo test")]["fallbacks"],
            {"full": 1, "busy": 1},
        )

    def test_live_day_with_queue_line_is_unchanged_by_release_facts(self) -> None:
        at = START + timedelta(hours=2)
        data = self.census_of(
            line(
                "queue",
                50,
                at - timedelta(seconds=30),
                notRouted="--local",
                reason="from the queue",
            ),
            line("fallback", 50, at - timedelta(seconds=20), reason="full"),
            line(
                "release",
                50,
                at,
                notRouted="from the release",
                reason="from the release",
                fellBack=["full"],
                wallSeconds=10.0,
                verdict="passed",
            ),
        )
        build = self.rows(data)[("demo", "cargo test")]
        self.assertEqual(build["notRouted"], {"--local": 1})
        self.assertEqual(build["fallbacks"], {"full": 1})
        self.assertEqual(
            data["local"],
            [{"repo": "demo", "family": "cargo test", "reason": "from the queue", "runs": 1}],
        )


class RunTests(unittest.TestCase):
    """Runs, the window and the figures."""

    def test_the_window_keeps_runs_by_their_release_else_their_queue(self) -> None:
        events = [
            line("queue", 1, START - timedelta(days=2)),
            line("release", 1, START + timedelta(hours=1), wallSeconds=1.0),
            line("queue", 2, START - timedelta(days=2)),
            line("release", 2, START - timedelta(days=1), wallSeconds=1.0),
            line("queue", 3, END - timedelta(hours=1)),
            line("covered", 4, START + timedelta(hours=2)),
        ]
        runs, headless = census.group_runs(events, START, END)
        self.assertEqual(sorted(run.head["pid"] for run in runs), [1, 3])
        self.assertEqual(headless, 1)

    def test_a_selftest_run_is_left_out(self) -> None:
        events = [
            line(
                "release",
                1,
                START + timedelta(hours=1),
                cwd=r"C:\t\gate-selftest-1\x.wt\y",
            )
        ]
        self.assertEqual(census.group_runs(events, START, END), ([], 0))

    def test_take_census_counts_routed_local_unfinished_lent_and_local_flag(
        self,
    ) -> None:
        at = START + timedelta(hours=1)
        events = [
            line("release", 1, at, wallSeconds=10.0, verdict="passed", remote=True),
            line("release", 2, at, wallSeconds=4.0, verdict="failed", remote="true"),
            line("release", 3, at, wallSeconds=None, verdict="unknown"),
            line("queue", 4, at),
            line("queue", 5, at),
            line("covered", 5, at),
            line("queue", 6, at, notRouted="--local", reason=r"needs C:\Users\x\f.txt"),
            line("release", 6, at, wallSeconds=2.0, verdict="passed"),
            line("fallback", 6, at, reason="busy"),
        ]
        runs, _ = census.group_runs(events, START, END)
        data = census.census_data(journal_of(*events), START, END, census.RepoNamer())
        self.assertEqual(len(runs), 6)
        (row,) = data["heavy"]
        self.assertEqual((row["local"], row["routed"], row["unfinished"]), (3, 1, 1))
        self.assertEqual(data["lent"], 1)
        self.assertEqual(row["localMedianSeconds"], 3.0)
        self.assertEqual(row["routedMedianSeconds"], 10.0)
        self.assertEqual(row["notRouted"], {"--local": 1})
        self.assertEqual(row["fallbacks"], {"busy": 1})
        self.assertEqual(
            data["local"],
            [
                {
                    "repo": "demo",
                    "family": "cargo test",
                    "reason": "needs <path>",
                    "runs": 1,
                }
            ],
        )

    def test_a_median_of_only_null_seconds_is_none(self) -> None:
        events = [
            line(
                "release",
                1,
                START + timedelta(hours=1),
                wallSeconds=None,
                verdict="unknown",
            )
        ]
        (row,) = census.census_data(
            journal_of(*events), START, END, census.RepoNamer()
        )["heavy"]
        self.assertIsNone(row["localMedianSeconds"])
        self.assertEqual(row["localSecondsTotal"], 0)

    def test_an_empty_window_says_so(self) -> None:
        report = census.render(
            census.census_data(journal_of(), START, END, census.RepoNamer())
        )
        self.assertIn(census.NO_HEAVY_LOCAL, report)


COMMIT_COMMAND = "git commit -m x"
BUILD_COMMAND = "dotnet build -p:TreatWarningsAsErrors=true"


class CommitSplitTests(unittest.TestCase):
    """A commit whose hooks run gates: what the census sorts inside it."""

    @staticmethod
    def census_of(*events: dict[str, object]) -> dict[str, object]:
        return census.census_data(journal_of(*events), START, END, census.RepoNamer())

    @staticmethod
    def rows(data: dict) -> dict[tuple[str, str], dict]:
        return {(row["repo"], row["family"]): row for row in data["heavy"]}

    def test_a_commit_s_nested_gate_linked_by_parent_pid_is_sorted_as_its_own_family(
        self,
    ) -> None:
        at = START + timedelta(hours=2)
        events = [
            line(
                "queue",
                10,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                11,
                at - timedelta(seconds=80),
                parentPid=10,
                command=BUILD_COMMAND,
            ),
            line("queue", 11, at - timedelta(seconds=80), command=BUILD_COMMAND),
            line(
                "release",
                11,
                at - timedelta(seconds=20),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                10,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertNotIn(("demo", "git commit"), rows)
        build = rows[("demo", "dotnet build")]
        self.assertEqual((build["routed"], build["local"]), (1, 0))
        self.assertEqual(build["routedMedianSeconds"], 60.0)
        self.assertEqual(data["commitSplit"]["demo"]["linkedByParentPid"], 1)
        self.assertIn(
            "nested gates linked by parentPid: 1; by containment: 0",
            census.render(data),
        )

    def test_a_nested_gate_on_an_archive_day_is_linked_by_containment(self) -> None:
        at = START + timedelta(hours=3)
        events = [
            line(
                "release",
                20,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                21,
                at + timedelta(seconds=1),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        self.assertEqual(
            rows[("demo", "git commit (ungated hooks)")]["localSecondsTotal"], 40.0
        )
        split = data["commitSplit"]["demo"]
        self.assertEqual(
            (split["commits"], split["withNested"], split["linkedByContainment"]),
            (1, 1, 1),
        )

    def test_a_commit_s_uncovered_share_is_its_own_blocked_row(self) -> None:
        at = START + timedelta(hours=4)
        events = [
            line(
                "queue",
                30,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                31,
                at - timedelta(seconds=80),
                parentPid=30,
                command=BUILD_COMMAND,
            ),
            line("queue", 31, at - timedelta(seconds=80), command=BUILD_COMMAND),
            line(
                "release",
                31,
                at - timedelta(seconds=50),
                wallSeconds=50.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                30,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        share = self.rows(data)[("demo", "git commit (ungated hooks)")]
        self.assertEqual((share["local"], share["localSecondsTotal"]), (1, 50.0))
        self.assertEqual(share["state"], "blocked on work")
        self.assertEqual(
            share["blocker"], "ungated hook commands in the repo's git hooks"
        )
        self.assertIn(
            "| demo | `git commit (ungated hooks)` | 1 | 0 | 0 | 0 | 50.0 | 50.0 | - | - | - | - "
            "| blocked on work: ungated hook commands in the repo's git hooks |",
            census.render(data),
        )

    def test_a_commit_without_nested_gates_stays_truly_local(self) -> None:
        at = START + timedelta(hours=5)
        events = [
            line(
                "queue",
                40,
                at - timedelta(seconds=6),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                40,
                at,
                commit=True,
                wallSeconds=6.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        (row,) = data["heavy"]
        self.assertEqual(
            (row["repo"], row["family"], row["local"], row["localSecondsTotal"]),
            ("demo", "git commit", 1, 6.0),
        )
        self.assertNotIn("state", row)
        split = data["commitSplit"]["demo"]
        self.assertEqual(
            (split["commits"], split["withNested"], split["uncoveredSeconds"]),
            (1, 0, 0.0),
        )

    def test_a_covered_nested_gate_counts_with_no_seconds(self) -> None:
        at = START + timedelta(hours=6)
        events = [
            line(
                "queue",
                50,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "queue",
                51,
                at - timedelta(seconds=80),
                command="dotnet csharpier check .",
            ),
            line(
                "covered",
                51,
                at - timedelta(seconds=10),
                parentPid=50,
                command="dotnet csharpier check .",
            ),
            line(
                "release",
                50,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        check = rows[("demo", "dotnet csharpier")]
        self.assertEqual(
            (check["local"], check["covered"], check["unfinished"]), (1, 1, 0)
        )
        self.assertEqual(check["localSecondsTotal"], 0)
        self.assertIsNone(check["localMedianSeconds"])
        self.assertNotIn(("demo", "git commit (ungated hooks)"), rows)
        self.assertEqual(rows[("demo", "git commit")]["localSecondsTotal"], 100.0)
        self.assertEqual(data["headless"], 0)

    def test_a_gate_after_the_commit_s_release_is_not_contained(self) -> None:
        at = START + timedelta(hours=7)
        events = [
            line(
                "release",
                60,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                61,
                at + timedelta(seconds=2),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        self.assertEqual(rows[("demo", "git commit")]["localSecondsTotal"], 100.0)
        split = data["commitSplit"]["demo"]
        self.assertEqual(
            (
                split["withNested"],
                split["linkedByContainment"],
                split["uncoveredSeconds"],
            ),
            (0, 0, 0.0),
        )

    def test_census_data_carries_the_commit_split(self) -> None:
        at = START + timedelta(hours=8)
        other = r"X:\dev\other.wt\tree"
        events = [
            line(
                "queue",
                70,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                71,
                at - timedelta(seconds=80),
                parentPid=70,
                command=BUILD_COMMAND,
            ),
            line("queue", 71, at - timedelta(seconds=80), command=BUILD_COMMAND),
            line(
                "release",
                71,
                at - timedelta(seconds=20),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                70,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                80,
                at,
                commit=True,
                wallSeconds=20.0,
                verdict="passed",
                command="git commit -m s",
                cwd=other,
            ),
        ]
        data = self.census_of(*events)
        self.assertEqual(
            data["commitSplit"],
            {
                "demo": {
                    "commits": 1,
                    "withNested": 1,
                    "uncoveredSeconds": 40.0,
                    "unsplitCommits": 0,
                    "linkedByParentPid": 1,
                    "linkedByContainment": 0,
                },
                "other": {
                    "commits": 1,
                    "withNested": 0,
                    "uncoveredSeconds": 0.0,
                    "unsplitCommits": 0,
                    "linkedByParentPid": 0,
                    "linkedByContainment": 0,
                },
            },
        )
        for key in (
            "window",
            "files",
            "unreadable",
            "skipped",
            "duplicates",
            "headless",
            "queuesFrom",
            "lent",
            "heavy",
            "local",
        ):
            self.assertIn(key, data)


def started(seconds: float) -> str:
    """A ``processStart`` that many seconds after the fixture's base, to reuse a pid later."""
    return stamp(START - timedelta(days=30) + timedelta(seconds=seconds))


class NestedLinkTests(unittest.TestCase):
    """How a gate is linked to the commit it runs under, and what that costs the commit."""

    @staticmethod
    def census_of(*events: dict[str, object]) -> dict[str, object]:
        return census.census_data(journal_of(*events), START, END, census.RepoNamer())

    @staticmethod
    def rows(data: dict) -> dict[tuple[str, str], dict]:
        return {(row["repo"], row["family"]): row for row in data["heavy"]}

    def test_a_reused_pid_of_a_non_commit_gate_does_not_link_to_an_old_commit(
        self,
    ) -> None:
        at = START + timedelta(hours=2)
        events = [
            line(
                "queue",
                100,
                at - timedelta(seconds=1000),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                100,
                at - timedelta(seconds=900),
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "queue",
                100,
                at - timedelta(seconds=500),
                command="cargo test",
                processStart=started(500),
            ),
            line(
                "release",
                100,
                at - timedelta(seconds=400),
                wallSeconds=50.0,
                verdict="passed",
                command="cargo test",
                processStart=started(500),
            ),
            line(
                "take",
                101,
                at - timedelta(seconds=300),
                parentPid=100,
                command=BUILD_COMMAND,
                processStart=started(600),
            ),
            line(
                "queue",
                101,
                at - timedelta(seconds=300),
                command=BUILD_COMMAND,
                processStart=started(600),
            ),
            line(
                "release",
                101,
                at - timedelta(seconds=200),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
                processStart=started(600),
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(rows[("demo", "git commit")]["localSecondsTotal"], 100.0)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        split = data["commitSplit"]["demo"]
        self.assertEqual((split["withNested"], split["linkedByParentPid"]), (0, 0))

    def test_a_gate_inside_a_nested_gate_is_not_counted_against_the_commit(
        self,
    ) -> None:
        at = START + timedelta(hours=3)
        events = [
            line(
                "queue",
                200,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                201,
                at - timedelta(seconds=80),
                parentPid=200,
                command=BUILD_COMMAND,
            ),
            line("queue", 201, at - timedelta(seconds=80), command=BUILD_COMMAND),
            line(
                "release",
                201,
                at - timedelta(seconds=20),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "take",
                202,
                at - timedelta(seconds=15),
                parentPid=201,
                command="dotnet csharpier check .",
            ),
            line(
                "queue",
                202,
                at - timedelta(seconds=15),
                command="dotnet csharpier check .",
            ),
            line(
                "release",
                202,
                at - timedelta(seconds=10),
                wallSeconds=20.0,
                verdict="passed",
                command="dotnet csharpier check .",
            ),
            line(
                "release",
                200,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(
            rows[("demo", "git commit (ungated hooks)")]["localSecondsTotal"], 40.0
        )
        self.assertEqual(rows[("demo", "dotnet csharpier")]["localSecondsTotal"], 20.0)
        split = data["commitSplit"]["demo"]
        self.assertEqual((split["withNested"], split["linkedByContainment"]), (1, 0))

    def test_a_live_run_without_parent_pid_in_the_commit_s_folder_is_not_contained(
        self,
    ) -> None:
        at = START + timedelta(hours=4)
        events = [
            line(
                "release",
                300,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line("take", 301, at - timedelta(seconds=30), command=BUILD_COMMAND),
            line("queue", 301, at - timedelta(seconds=30), command=BUILD_COMMAND),
            line(
                "release",
                301,
                at + timedelta(seconds=1),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(rows[("demo", "git commit")]["localSecondsTotal"], 100.0)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        split = data["commitSplit"]["demo"]
        self.assertEqual((split["withNested"], split["linkedByContainment"]), (0, 0))

    def test_an_unfinished_commit_is_not_split(self) -> None:
        at = START + timedelta(hours=5)
        events = [
            line(
                "queue",
                400,
                at - timedelta(seconds=50),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                401,
                at - timedelta(seconds=40),
                parentPid=400,
                command=BUILD_COMMAND,
            ),
            line("queue", 401, at - timedelta(seconds=40), command=BUILD_COMMAND),
            line(
                "release",
                401,
                at - timedelta(seconds=20),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(rows[("demo", "git commit")]["unfinished"], 1)
        self.assertNotIn(("demo", "git commit (ungated hooks)"), rows)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        split = data["commitSplit"]["demo"]
        self.assertEqual((split["commits"], split["withNested"]), (1, 0))

    def test_a_child_of_a_light_commit_is_kept_by_its_own_moment(self) -> None:
        at = START + timedelta(hours=6)
        events = [
            line(
                "queue",
                500,
                at - timedelta(seconds=60),
                commit=True,
                kind="light",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                500,
                at,
                commit=True,
                kind="light",
                wallSeconds=60.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                501,
                START - timedelta(hours=2),
                parentPid=500,
                command="dotnet csharpier check .",
            ),
            line(
                "queue",
                501,
                START - timedelta(hours=2),
                command="dotnet csharpier check .",
            ),
            line(
                "release",
                501,
                START - timedelta(hours=1),
                wallSeconds=20.0,
                verdict="passed",
                command="dotnet csharpier check .",
            ),
        ]
        data = self.census_of(*events)
        self.assertEqual(data["headless"], 0)
        self.assertNotIn(("demo", "dotnet csharpier"), self.rows(data))
        self.assertEqual(data["commitSplit"], {})

    def test_a_covered_child_of_a_light_commit_still_counts_as_headless(self) -> None:
        at = START + timedelta(hours=7)
        events = [
            line(
                "queue",
                600,
                at - timedelta(seconds=60),
                commit=True,
                kind="light",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                600,
                at,
                commit=True,
                kind="light",
                wallSeconds=60.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "covered",
                601,
                at - timedelta(seconds=30),
                parentPid=600,
                kind="light",
                command="dotnet csharpier check .",
            ),
        ]
        data = self.census_of(*events)
        self.assertEqual(data["headless"], 1)
        self.assertEqual(data["heavy"], [])


class SplitRuleTests(unittest.TestCase):
    """What the census does with a split commit's gates, its share and its window."""

    @staticmethod
    def census_of(*events: dict[str, object]) -> dict[str, object]:
        return census.census_data(journal_of(*events), START, END, census.RepoNamer())

    @staticmethod
    def rows(data: dict) -> dict[tuple[str, str], dict]:
        return {(row["repo"], row["family"]): row for row in data["heavy"]}

    def test_a_local_nested_gate_still_shows_in_the_local_table(self) -> None:
        at = START + timedelta(hours=2)
        events = [
            line(
                "queue",
                700,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                701,
                at - timedelta(seconds=80),
                parentPid=700,
                command=BUILD_COMMAND,
            ),
            line(
                "queue",
                701,
                at - timedelta(seconds=80),
                notRouted="--local",
                reason=r"needs C:\Users\x\f.txt",
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                701,
                at - timedelta(seconds=20),
                wallSeconds=60.0,
                verdict="passed",
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                700,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        self.assertEqual(
            data["local"],
            [
                {
                    "repo": "demo",
                    "family": "dotnet build",
                    "reason": "needs <path>",
                    "runs": 1,
                }
            ],
        )

    def test_a_commit_with_a_covered_hook_gate_gets_no_ungated_share(self) -> None:
        at = START + timedelta(hours=3)
        events = [
            line(
                "queue",
                710,
                at - timedelta(seconds=100),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                711,
                at - timedelta(seconds=80),
                parentPid=710,
                command=BUILD_COMMAND,
            ),
            line("queue", 711, at - timedelta(seconds=80), command=BUILD_COMMAND),
            line(
                "release",
                711,
                at - timedelta(seconds=40),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "queue",
                712,
                at - timedelta(seconds=30),
                command="dotnet csharpier check .",
            ),
            line(
                "covered",
                712,
                at - timedelta(seconds=20),
                parentPid=710,
                command="dotnet csharpier check .",
            ),
            line(
                "release",
                710,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertNotIn(("demo", "git commit (ungated hooks)"), rows)
        commit = rows[("demo", "git commit")]
        self.assertEqual((commit["local"], commit["localSecondsTotal"]), (1, 100.0))
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        self.assertEqual(rows[("demo", "dotnet csharpier")]["covered"], 1)
        split = data["commitSplit"]["demo"]
        self.assertEqual(
            (split["withNested"], split["uncoveredSeconds"], split["unsplitCommits"]),
            (1, 0.0, 1),
        )
        self.assertIn(
            "commits with covered hook gates, share not split: 1", census.render(data)
        )

    def test_a_commit_fully_covered_by_its_nested_gates_adds_no_share_row(self) -> None:
        at = START + timedelta(hours=4)
        events = [
            line(
                "queue",
                720,
                at - timedelta(seconds=60),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                721,
                at - timedelta(seconds=50),
                parentPid=720,
                command=BUILD_COMMAND,
            ),
            line("queue", 721, at - timedelta(seconds=50), command=BUILD_COMMAND),
            line(
                "release",
                721,
                at,
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
            line(
                "release",
                720,
                at,
                commit=True,
                wallSeconds=60.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertNotIn(("demo", "git commit (ungated hooks)"), rows)
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        self.assertEqual(data["commitSplit"]["demo"]["uncoveredSeconds"], 0.0)

    def test_a_nested_gate_counts_only_in_its_commit_s_window(self) -> None:
        events = [
            line(
                "queue",
                730,
                START - timedelta(hours=2),
                commit=True,
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                730,
                START - timedelta(hours=1),
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "take",
                731,
                START + timedelta(minutes=30),
                parentPid=730,
                command=BUILD_COMMAND,
            ),
            line("queue", 731, START + timedelta(minutes=30), command=BUILD_COMMAND),
            line(
                "release",
                731,
                START + timedelta(hours=1),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
            ),
        ]
        data = self.census_of(*events)
        self.assertEqual(data["heavy"], [])
        self.assertEqual(data["commitSplit"], {})

    def test_containment_matches_a_folder_spelled_differently_or_below(self) -> None:
        at = START + timedelta(hours=5)
        below = "x:/dev/demo.wt/tree/sub/"
        events = [
            line(
                "release",
                740,
                at,
                commit=True,
                wallSeconds=100.0,
                verdict="passed",
                command=COMMIT_COMMAND,
            ),
            line(
                "release",
                741,
                at + timedelta(seconds=1),
                wallSeconds=60.0,
                verdict="passed",
                remote=True,
                command=BUILD_COMMAND,
                cwd=below,
            ),
        ]
        data = self.census_of(*events)
        rows = self.rows(data)
        self.assertEqual(
            rows[("demo", "git commit (ungated hooks)")]["localSecondsTotal"], 40.0
        )
        self.assertEqual(rows[("demo", "dotnet build")]["routed"], 1)
        split = data["commitSplit"]["demo"]
        self.assertEqual((split["withNested"], split["linkedByContainment"]), (1, 1))


class NoMachinePathTests(unittest.TestCase):
    """No machine path reaches the Markdown or the JSON."""

    COMMANDS = (
        r'"C:\Program Files\PowerShell\7\pwsh.exe" -NoProfile -File C:\Users\First Last\tools\t.ps1 -X',
        r"pwsh -NoProfile -File ..\other\t.ps1",
        r"dotnet build C:\Users\First Last\src\a.csproj -c Release",
        r"cargo --manifest-path \\box\share\Cargo.toml test",
        "bash /mnt/c/work/run.sh",
        "bash ~/scripts/go.sh",
        r"uv run --directory C:\x\tools python -I C:\x\tools\a.py",
        r"node C:\x\index.js build C:\y",
        r"C:\Users\First Last\bin\tool.exe C:\data",
        r"git -C ..\demo.wt\other commit -F C:\m.txt",
    )

    def test_a_census_of_path_laden_commands_prints_no_path(self) -> None:
        at = START + timedelta(hours=1)
        events = []
        for pid, command in enumerate(self.COMMANDS):
            events += [
                line(
                    "queue",
                    pid,
                    at,
                    command=command,
                    notRouted="--local",
                    reason=r"see C:\Users\First Last\f and ~/x and /mnt/c/y",
                ),
                line(
                    "release",
                    pid,
                    at,
                    command=command,
                    wallSeconds=1.0,
                    verdict="passed",
                ),
            ]
        data = census.census_data(journal_of(*events), START, END, census.RepoNamer())
        for text in (census.render(data), json.dumps(data)):
            self.assertIsNone(
                MACHINE_PATH.search(text), MACHINE_PATH.search(text) and text
            )


if __name__ == "__main__":
    unittest.main()
