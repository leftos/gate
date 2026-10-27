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
