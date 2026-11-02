"""Tests of the local-run watcher: shapes, what counts, the report, backups, mark and watch."""

import gzip
import io
import itertools
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest import mock

import watch_local

WT = r"X:\dev\rustling-tulip.wt\rt-763"
OTHER_WT = r"X:\dev\rustling-tulip.wt\rt-764"
OTHER_REPO = r"X:\dev\other-repo.wt\ot-1"
START = datetime(2026, 11, 2, 12, tzinfo=UTC)
UNSET = object()
SEQUENCE = itertools.count()


def stamp(moment: datetime) -> str:
    return moment.strftime("%Y-%m-%dT%H:%M:%S.0000000Z")


def release(
    pid: int,
    at: datetime,
    command: str = "cargo build --workspace",
    cwd: str = WT,
    wall: float = 120.0,
    **fields: object,
) -> dict[str, object]:
    event: dict[str, object] = {
        "v": 1,
        "time": stamp(at),
        "event": "release",
        "pid": pid,
        # A run identity of its own, so two events in one test are never the census's duplicate.
        "processStart": stamp(
            at - timedelta(minutes=2) - timedelta(seconds=next(SEQUENCE))
        ),
        "kind": "heavy",
        "cwd": cwd,
        "command": command,
        "wallSeconds": wall,
    }
    event.update(fields)
    return event


class Clock:
    """A clock whose sleep advances its own time."""

    def __init__(self, start: datetime) -> None:
        self.moment = start

    def __call__(self) -> datetime:
        return self.moment

    def sleep(self, seconds: float) -> None:
        self.moment += timedelta(seconds=seconds)


class Broken:
    """A stream whose every write fails."""

    def write(self, text: str) -> int:
        raise OSError("broken stdout")

    def flush(self) -> None:
        pass


class Case(unittest.TestCase):
    """A journal folder, a ``remote.json`` and a state file under one temporary folder."""

    def setUp(self) -> None:
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        root = Path(self.folder.name)
        self.journal = root / "journal"
        self.journal.mkdir()
        self.remote = root / "remote.json"
        self.state = root / "state.json"
        self.write_remote({})

    def options(self, **changes: object) -> watch_local.Options:
        base: dict[str, object] = {
            "journal": self.journal,
            "remote": self.remote,
            "state": self.state,
        }
        base.update(changes)
        return watch_local.Options(**base)

    def write_events(
        self, *events: dict[str, object], name: str = "events.jsonl"
    ) -> None:
        text = "".join(json.dumps(event) + "\n" for event in events)
        (self.journal / name).write_text(text, encoding="utf-8", newline="\n")

    def write_remote(self, repos: dict[str, list[str]]) -> None:
        self.write_remote_data({"repos": repos})

    def write_remote_data(self, data: dict[str, object]) -> None:
        self.remote.write_text(json.dumps(data), encoding="utf-8", newline="\n")

    def write_state(self, state: dict[str, object]) -> None:
        self.state.write_text(json.dumps(state), encoding="utf-8", newline="\n")

    def scan(
        self,
        since: object = UNSET,
        now: datetime = START + timedelta(hours=1),
        **changes: object,
    ) -> tuple[int, str]:
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.scan(
                self.options(**changes),
                None if since is UNSET else since,
                now,
            )
        return status, out.getvalue()

    def watch(
        self,
        clock: Clock,
        sleep: object,
        since: datetime | None = None,
        **changes: object,
    ) -> tuple[int, str]:
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.watch(
                self.options(**changes), since, sleep=sleep, now=clock
            )
        return status, out.getvalue()


class ShapeTests(unittest.TestCase):
    """The shape table and the one path kept whole, one case each."""

    def test_the_five_shape_rows_and_the_non_example(self) -> None:
        cases = [
            (
                "cargo build --manifest-path Cargo.toml -p daemon --release",
                "cargo build --manifest-path Cargo.toml -p daemon --release",
            ),
            (
                f"cargo nextest run --manifest-path {WT}\\Cargo.toml --workspace",
                "cargo nextest run --manifest-path Cargo.toml --workspace",
            ),
            (
                r"dotnet build .tmp\dbg1656\probe\Probe.csproj",
                "dotnet build .tmp/*",
            ),
            (
                "uv run --no-project python -I .tmp/spike/run.py --seed 4",
                "uv run --no-project python -I .tmp/* --seed #",
            ),
            (
                'dotnet build "x:/dev/rustling-tulip.wt/rt-763/src/A.csproj"',
                "dotnet build src/A.csproj",
            ),
            (
                r'"C:\Program Files\dotnet\dotnet.exe" build src/A.csproj',
                "dotnet build src/A.csproj",
            ),
            (
                'dotnet build "src/A B.csproj"',
                "dotnet build src/A B.csproj",
            ),
            (
                r"dotnet run --project X:\dev\other\A.csproj",
                "dotnet run --project X:/dev/other/A.csproj",
            ),
        ]
        for command, expected in cases:
            with self.subTest(command=command):
                self.assertEqual(watch_local.shape_of(command, WT), expected)

    def test_the_number_rows_of_the_shape_table(self) -> None:
        cases = [
            (
                "dotnet run --project A -- --seeds 1-10000 --nations 7",
                "dotnet run --project A -- --seeds # --nations #",
            ),
            ("tool --seed=4 --rate=0.5", "tool --seed=# --rate=#"),
            ("dotnet build -f net8.0", "dotnet build -f net8.0"),
            ('blender "x/Blender 5.2/b.exe"', "blender x/Blender 5.2/b.exe"),
        ]
        for command, expected in cases:
            with self.subTest(command=command):
                self.assertEqual(watch_local.shape_of(command, WT), expected)

    def test_a_cwd_below_the_root_shares_the_roots_shape(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder) / "repo"
            (root / ".git").mkdir(parents=True)
            below = root / "src"
            command = f'dotnet build "{root / "A.csproj"}"'
            self.assertEqual(
                watch_local.shape_of(command, str(root)), "dotnet build A.csproj"
            )
            self.assertEqual(
                watch_local.shape_of(command, str(below)), "dotnet build A.csproj"
            )


class LikeTests(unittest.TestCase):
    """The gate's ``-like`` match, one rule per case."""

    def test_like_reads_a_set_and_a_range(self) -> None:
        self.assertTrue(watch_local.like("[ab]c", "bc"))
        self.assertTrue(watch_local.like("[a-c]x", "bx"))
        self.assertFalse(watch_local.like("[a-c]x", "dx"))

    def test_like_takes_a_backtick_literally(self) -> None:
        self.assertTrue(watch_local.like("a`*b", "a*b"))
        self.assertFalse(watch_local.like("a`*b", "axb"))

    def test_an_unclosed_set_matches_nothing(self) -> None:
        self.assertFalse(watch_local.like("a[b", "ab"))
        self.assertFalse(watch_local.like("[c-a]x", "bx"))

    def test_matching_ignores_case(self) -> None:
        self.assertTrue(watch_local.like("Cargo Build*", "cargo build --workspace"))


class CountingTests(Case):
    """What counts as a heavy run that stayed here, one rule per case."""

    def test_a_short_run_does_not_count(self) -> None:
        self.write_events(
            release(1, START, command="cargo build -p short", wall=59.0),
            release(1, START, command="cargo build -p long", wall=60.0),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertNotIn("cargo build -p short", text)
        self.assertIn("cargo build -p long", text)

    def test_a_routed_run_does_not_count(self) -> None:
        self.write_events(release(1, START, remote=True))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_git_and_prek_do_not_count(self) -> None:
        self.write_events(
            release(1, START, command="Git commit -m x"),
            release(1, START, command="PREK run --all-files"),
            release(1, START, command="cargo test --workspace"),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertNotIn("Git commit", text)
        self.assertNotIn("PREK run", text)
        self.assertIn("cargo test --workspace", text)

    def test_a_selftest_folder_does_not_count(self) -> None:
        self.write_events(
            release(1, START, cwd=r"C:\Users\me\AppData\Local\Temp\gate-selftest\a")
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_a_light_run_does_not_count(self) -> None:
        self.write_events(release(1, START, kind="light"))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_a_run_before_the_cursor_does_not_count(self) -> None:
        self.write_events(release(1, START - timedelta(hours=2)))
        self.write_state(
            {"v": 1, "cursor": stamp(START - timedelta(hours=1)), "shapes": {}}
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")
        status, text = self.scan(since=START - timedelta(hours=3))
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", text)


class ReportTests(Case):
    """The pending shapes, their facts and the order they are printed in."""

    def test_two_worktrees_share_one_shape(self) -> None:
        self.write_events(
            release(1, START, cwd=WT, wall=120.0),
            release(1, START + timedelta(minutes=1), cwd=OTHER_WT, wall=80.0),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("runs=2 seconds=200", text)
        self.assertEqual(text.count("  shape: "), 1)

    def test_a_near_miss_names_its_nearest_pattern(self) -> None:
        shape_command = (
            "cargo build --manifest-path Cargo.toml -p daemon -p tracer --release"
        )
        pattern = (
            "cargo build --manifest-path Cargo.toml -p daemon -p tracer "
            "-p rustling-tulip-native --release"
        )
        self.write_remote({"rustling-tulip": [pattern]})
        self.write_events(release(1, START, command=shape_command))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=no", text)
        self.assertIn(f"  nearest: {pattern}", text)
        self.assertIn("  differs: -rustling-tulip-native", text)

    def test_a_matched_shape_says_so(self) -> None:
        self.write_remote({"rustling-tulip": ["cargo build*"]})
        self.write_events(release(1, START))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=yes", text)

    def test_a_repo_with_no_patterns_has_no_nearest(self) -> None:
        self.write_events(release(1, START))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=no", text)
        self.assertNotIn("nearest:", text)
        self.assertNotIn("differs:", text)

    def test_routed_counts_the_repos_family_only(self) -> None:
        self.write_events(
            release(1, START, command="cargo build --workspace", wall=90.0),
            release(1, START, cwd=OTHER_REPO, remote=True),
            release(1, START, command="cargo test --workspace", remote=True),
            release(1, START, command="cargo build --release", remote=True),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("routed=1", text)

    def test_routed_counts_a_tmp_script_by_its_shape(self) -> None:
        self.write_events(
            release(
                1,
                START,
                command="uv run --no-project python -I .tmp/b/run.py --seed 4",
                wall=90.0,
            ),
            release(
                1,
                START,
                command="uv run --no-project python -I .tmp/a/run.py --seed 4",
                remote=True,
            ),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("routed=1", text)

    def test_a_local_shape_is_not_pending(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {key: {"repo": "rustling-tulip", "verdict": "local"}},
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_a_booked_shape_is_not_pending(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {key: {"repo": "rustling-tulip", "verdict": "booked"}},
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_a_routed_shape_still_local_is_pending_and_flagged(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {
                        "repo": "rustling-tulip",
                        "verdict": "routed",
                        "at": stamp(START - timedelta(hours=1)),
                    }
                },
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("local-runs: 1 pending", text)
        self.assertIn("STILL-LOCAL-AFTER-FIX", text)

    def test_a_run_started_before_the_fix_is_not_still_local(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {
                        "repo": "rustling-tulip",
                        "verdict": "routed",
                        "at": stamp(START + timedelta(hours=1)),
                    }
                },
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_a_run_started_after_the_fix_is_still_local(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {
                        "repo": "rustling-tulip",
                        "verdict": "routed",
                        "at": stamp(START - timedelta(hours=1)),
                    }
                },
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("STILL-LOCAL-AFTER-FIX", text)

    def test_the_report_sorts_by_seconds(self) -> None:
        self.write_events(
            release(1, START, command="cargo test --workspace", wall=100.0),
            release(1, START, command="cargo build --workspace", wall=300.0),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertLess(
            text.index("cargo build --workspace"),
            text.index("cargo test --workspace"),
        )

    def test_a_local_flag_run_shows_its_reason(self) -> None:
        self.write_events(
            release(
                1,
                START,
                command="cargo build --workspace",
                notRouted="--local",
                reason="builder busy",
            ),
            release(1, START, command="dotnet build A.csproj", notRouted="faster here"),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("reason=--local: builder busy", text)
        self.assertIn("reason=faster here", text)

    def test_a_builder_entrys_pattern_counts(self) -> None:
        self.write_remote_data(
            {
                "builders": [
                    {
                        "host": "builder",
                        "os": "windows",
                        "repos": {"rustling-tulip": ["cargo build*"]},
                    }
                ]
            }
        )
        self.write_events(release(1, START))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=yes", text)

    def test_a_not_routed_builder_entrys_pattern_does_not_count(self) -> None:
        self.write_remote_data(
            {
                "builders": [
                    {
                        "host": "builder",
                        "os": "windows",
                        "routed": False,
                        "repos": {"rustling-tulip": ["cargo build*"]},
                    }
                ]
            }
        )
        self.write_events(release(1, START))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=no", text)

    def test_two_runs_differing_only_in_numbers_share_a_shape(self) -> None:
        self.write_events(
            release(
                1,
                START,
                command="dotnet run --project A -- --seeds 1-100 --nations 7",
                wall=70.0,
            ),
            release(
                1,
                START + timedelta(minutes=1),
                command="dotnet run --project A -- --seeds 2-200 --nations 9",
                wall=90.0,
            ),
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.count("  shape: "), 1)
        self.assertIn("runs=2 seconds=160", text)
        self.assertIn("--seeds # --nations #", text)

    def test_a_question_mark_pattern_matches_a_separator(self) -> None:
        command = (
            "dotnet run --project src/Intrigue.Sim -c Release --no-build -- "
            "--mode generate --seeds 1-10000 --nations 7"
        )
        pattern = "dotnet run --project src?Intrigue.Sim *"
        self.write_remote({"world-of-intrigue": [pattern]})
        self.write_events(
            release(
                1,
                START,
                cwd=r"X:\dev\world-of-intrigue.wt\wi-1",
                command=command,
            )
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("matched=yes", text)

    def test_claude_configs_patterns_are_found_under_its_gate_key(self) -> None:
        self.write_remote({"claude": ["cargo build*"]})
        self.write_events(release(1, START, cwd=r"C:\Users\me\.claude-wt\cc-1"))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn(" claude-config ", text)
        self.assertIn("matched=yes", text)


class BackupTests(Case):
    """The three-valued matched fact, read from the backups beside ``remote.json``."""

    def setUp(self) -> None:
        super().setUp()
        self.write_events(release(1, START))
        self.write_remote({"rustling-tulip": ["cargo build*"]})

    def write_backup(
        self,
        at: datetime,
        repos: dict[str, list[str]],
        name: str | None = None,
        text: str | None = None,
    ) -> Path:
        path = self.remote.parent / (
            name or f"remote.json.bak-{at.strftime('%Y%m%dT%H%M%SZ')}"
        )
        path.write_text(
            text if text is not None else json.dumps({"repos": repos}),
            encoding="utf-8",
            newline="\n",
        )
        return path

    def matched(self, text: str) -> str:
        for line in text.splitlines():
            if " matched=" in line:
                return line.split(" matched=")[1].split()[0]
        raise AssertionError(f"no matched= line in {text!r}")

    def test_a_pattern_added_after_the_run_is_since(self) -> None:
        self.write_backup(START + timedelta(hours=1), {})
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "since")

    def test_a_pattern_present_at_the_run_is_yes(self) -> None:
        self.write_backup(
            START + timedelta(hours=1), {"rustling-tulip": ["cargo build*"]}
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "yes")

    def test_with_no_later_backup_the_current_file_decides(self) -> None:
        self.write_backup(START - timedelta(hours=1), {})
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "yes")

    def test_the_earliest_later_backup_decides(self) -> None:
        self.write_backup(START + timedelta(hours=1), {})
        self.write_backup(
            START + timedelta(hours=2), {"rustling-tulip": ["cargo build*"]}
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "since")

    def test_an_unparsable_backup_is_skipped(self) -> None:
        self.write_backup(START + timedelta(hours=1), {}, text="not json")
        self.write_backup(
            START + timedelta(hours=2), {"rustling-tulip": ["cargo build*"]}
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "yes")

    def test_a_backup_with_a_bad_name_is_skipped(self) -> None:
        self.write_backup(START + timedelta(hours=1), {}, name="remote.json.bak-old")
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "yes")

    def test_a_tagged_backup_counts(self) -> None:
        self.write_backup(
            START + timedelta(hours=1),
            {},
            name=f"remote.json.bak-retry-{stamp_name(START + timedelta(hours=1))}",
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "since")

    def test_a_numbered_backup_counts(self) -> None:
        self.write_backup(
            START + timedelta(hours=1),
            {},
            name=f"remote.json.bak-{stamp_name(START + timedelta(hours=1))}-2",
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "since")

    def test_a_pattern_added_during_the_run_is_since(self) -> None:
        self.write_events(
            release(
                1,
                START + timedelta(minutes=10),
                wall=600.0,
                processStart=stamp(START),
            )
        )
        self.write_backup(START + timedelta(minutes=5), {})
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "since")

    def test_a_pattern_never_matching_is_no(self) -> None:
        self.write_remote({})
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(self.matched(text), "no")


def stamp_name(moment: datetime) -> str:
    return moment.strftime("%Y%m%dT%H%M%SZ")


class MarkTests(Case):
    """The mark command."""

    def state_with(self, *keys: str) -> dict[str, object]:
        shapes = {key: {"repo": "rustling-tulip", "shape": "a shape"} for key in keys}
        return {"v": 1, "cursor": None, "shapes": shapes}

    def test_mark_of_an_unknown_id_exits_2(self) -> None:
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.mark(["deadbeef"], "local", "a note", self.state)
        self.assertEqual(status, 2)
        self.assertEqual(out.getvalue().strip(), "local-runs: no shape deadbeef")

    def test_mark_writes_nothing_when_one_id_is_unknown(self) -> None:
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(self.state_with(key))
        before = self.state.read_text(encoding="utf-8")
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.mark([key, "deadbeef"], "local", "a note", self.state)
        self.assertEqual(status, 2)
        self.assertIn("local-runs: no shape deadbeef", out.getvalue())
        self.assertEqual(self.state.read_text(encoding="utf-8"), before)

    def test_mark_takes_several_ids(self) -> None:
        first = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        second = watch_local.shape_id("rustling-tulip", "cargo test --workspace")
        self.write_state(self.state_with(first, second))
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.mark(
                [first, second], "booked", "one decision", self.state
            )
        self.assertEqual(status, 0)
        self.assertEqual(
            out.getvalue().strip().splitlines(),
            [f"local-runs: {first} booked", f"local-runs: {second} booked"],
        )
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["shapes"][first]["note"], "one decision")
        self.assertEqual(state["shapes"][second]["note"], "one decision")


class JournalTests(Case):
    """The journal's own edges: broken lines, rotation, archives and the file cache."""

    def test_a_broken_journal_line_is_skipped(self) -> None:
        good = release(1, START)
        text = (
            "not json\n"
            + json.dumps({"v": 1, "event": "release"})
            + "\n"
            + json.dumps(good)
            + "\n"
        )
        (self.journal / "events.jsonl").write_text(text, encoding="utf-8", newline="\n")
        status, out = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", out)

    def test_a_missing_rotated_file_is_fine(self) -> None:
        self.write_events(release(1, START))
        self.assertFalse((self.journal / "events.1.jsonl").exists())
        status, out = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", out)

    def test_a_rotation_does_not_double_a_run(self) -> None:
        event = release(1, START, wall=120.0)
        self.write_events(event)
        self.write_events(event, name="events.1.jsonl")
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("runs=1 seconds=120", text)

    def test_an_archived_release_after_the_cursor_is_reported(self) -> None:
        folder = self.journal / "archive"
        folder.mkdir()
        with gzip.open(
            folder / "events-20261101.jsonl.gz", "wt", encoding="utf-8", newline="\n"
        ) as stream:
            stream.write(json.dumps(release(1, START)) + "\n")
        status, text = self.scan(since=START - timedelta(hours=1))
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", text)

    def test_an_unchanged_file_is_not_parsed_again(self) -> None:
        watch_local.CACHE.clear()
        self.write_events(release(1, START))
        calls: list[Path] = []
        original = watch_local.read_file

        def counting(path: Path) -> list[dict]:
            calls.append(path)
            return original(path)

        with mock.patch.object(watch_local, "read_file", counting):
            self.scan()
            self.scan()
        self.assertEqual(len(calls), 1)

    def test_a_line_of_another_version_is_skipped(self) -> None:
        older = release(1, START, command="cargo test --workspace")
        older["v"] = 2
        self.write_events(older, release(1, START))
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertNotIn("cargo test --workspace", text)
        self.assertIn("cargo build --workspace", text)

    def test_a_truncated_archive_keeps_its_lines(self) -> None:
        path = self.journal / "events-20261101.jsonl.gz"
        written = 2000
        with gzip.open(path, "wt", encoding="utf-8", newline="\n") as stream:
            for pid in range(written):
                stream.write(json.dumps(release(pid, START)) + "\n")
        whole = path.read_bytes()
        path.write_bytes(whole[: len(whole) // 2])
        errors = io.StringIO()
        with redirect_stderr(errors):
            events = watch_local.read_file(path)
        self.assertGreater(len(events), 0)
        self.assertLess(len(events), written)
        self.assertIn(
            f"local-runs: could not read all of {path.name}: ", errors.getvalue()
        )


class RemoteTests(Case):
    """``remote.json`` that cannot be read."""

    def test_a_bad_remote_file_exits_2(self) -> None:
        self.write_events(release(1, START))
        self.remote.write_text("{not json", encoding="utf-8", newline="\n")
        out = io.StringIO()
        with redirect_stdout(out):
            status = watch_local.main(
                [
                    "scan",
                    "--journal",
                    str(self.journal),
                    "--remote",
                    str(self.remote),
                    "--state",
                    str(self.state),
                    "--since",
                    "2026-11-02T11:00:00Z",
                ]
            )
        self.assertEqual(status, 2)
        self.assertTrue(
            out.getvalue().startswith(f"local-runs: cannot read {self.remote}: ")
        )


class CursorTests(Case):
    """The cursor a report leaves behind."""

    def test_the_cursor_advances_after_a_report(self) -> None:
        self.write_events(release(1, START))
        status, first = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", first)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        state["shapes"] = {}
        self.write_state(state)
        status, second = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(second.strip(), "local-runs: nothing pending")


class StateTests(Case):
    """The state file's writes and what a report carries forward."""

    def test_a_state_write_leaves_no_temporary_file(self) -> None:
        self.write_events(release(1, START))
        self.scan()
        names = sorted(path.name for path in self.state.parent.glob("state.json*"))
        self.assertEqual(names, ["state.json"])

    def test_a_failed_print_keeps_the_cursor(self) -> None:
        self.write_events(release(1, START))
        before = stamp(START - timedelta(days=1))
        self.write_state({"v": 1, "cursor": before, "shapes": {}})
        with redirect_stdout(Broken()), self.assertRaises(OSError):
            watch_local.scan(self.options(), None, now=START + timedelta(hours=1))
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["cursor"], before)

    def test_an_unmarked_shape_is_in_the_next_report(self) -> None:
        self.write_events(release(1, START))
        status, first = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("cargo build --workspace", first)
        status, second = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("local-runs: 1 pending", second)
        self.assertIn("cargo build --workspace", second)

    def test_a_marked_shape_leaves_the_report(self) -> None:
        self.write_events(release(1, START))
        self.scan()
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        watch_local.mark([key], "booked", "done", self.state)
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def flag_a_failed_fix(self) -> str:
        """Reports a shape marked ``routed`` that ran here after the fix, returning its id."""
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {
                        "repo": "rustling-tulip",
                        "verdict": "routed",
                        "at": stamp(START - timedelta(hours=1)),
                    }
                },
            }
        )
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("STILL-LOCAL-AFTER-FIX", text)
        return key

    def test_a_flagged_shape_stays_in_the_next_report(self) -> None:
        self.flag_a_failed_fix()
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertIn("local-runs: 1 pending", text)
        self.assertIn("STILL-LOCAL-AFTER-FIX", text)

    def test_marking_a_flagged_shape_clears_it(self) -> None:
        key = self.flag_a_failed_fix()
        flagged = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertIs(flagged["shapes"][key].get("flagged"), True)
        with redirect_stdout(io.StringIO()):
            status = watch_local.mark([key], "booked", "fix failed", self.state)
        self.assertEqual(status, 0)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertNotIn("flagged", state["shapes"][key])
        status, text = self.scan()
        self.assertEqual(status, 0)
        self.assertEqual(text.strip(), "local-runs: nothing pending")

    def test_two_saves_do_not_share_a_temporary_file(self) -> None:
        sources: list[str] = []
        original = watch_local.os.replace

        def recording(source: str, target: Path) -> None:
            sources.append(str(source))
            original(source, target)

        with mock.patch.object(watch_local.os, "replace", recording):
            watch_local.save_state(self.state, {"v": 1})
            watch_local.save_state(self.state, {"v": 1})
        self.assertEqual(len(set(sources)), 2)
        for source in sources:
            self.assertEqual(Path(source).parent, self.state.parent)
            self.assertTrue(Path(source).name.startswith("state.json."))

    def test_a_failed_replace_leaves_no_temporary_file(self) -> None:
        refused = mock.Mock(side_effect=OSError("held by a reader"))
        with (
            mock.patch.object(watch_local.os, "replace", refused),
            self.assertRaises(watch_local.LocalRunsError),
        ):
            watch_local.save_state(self.state, {"v": 1})
        refused.assert_called_once()
        self.assertEqual(list(self.state.parent.glob("state.json*")), [])


class WatchTests(Case):
    """The watch's batching, its hours, its read skipping and its state reloads."""

    def test_watch_batches_after_the_settle(self) -> None:
        self.write_events(release(1, START, wall=90.0))
        clock = Clock(START)
        sleeps: list[float] = []
        case = self

        def sleep(seconds: float) -> None:
            sleeps.append(seconds)
            clock.sleep(seconds)
            if len(sleeps) == 3:
                case.write_events(
                    release(1, START, wall=90.0),
                    release(1, START, command="dotnet build A.csproj", wall=70.0),
                )

        status, text = self.watch(
            clock,
            sleep,
            since=START - timedelta(minutes=1),
            settle=120.0,
            poll=30.0,
            hours=1.0,
        )
        self.assertEqual(status, 0)
        self.assertIn("local-runs: 2 pending", text)
        self.assertIn("cargo build --workspace", text)
        self.assertIn("dotnet build A.csproj", text)

    def test_watch_ends_after_its_hours(self) -> None:
        self.write_events()
        clock = Clock(START)
        status, text = self.watch(
            clock, clock.sleep, settle=60.0, poll=30.0, hours=0.05
        )
        self.assertEqual(status, 1)
        self.assertEqual(
            text.strip(), "local-runs: watch ended after 0.05 h with nothing pending"
        )

    def test_watch_skips_an_unchanged_journal(self) -> None:
        self.write_events(release(1, START, remote=True))
        clock = Clock(START)
        reads: list[Path] = []
        original = watch_local.read_file

        def counting(path: Path) -> list[dict]:
            reads.append(path)
            return original(path)

        with (
            mock.patch.object(watch_local, "read_file", counting),
            mock.patch.object(watch_local, "CACHE", watch_local.JournalCache()),
        ):
            status, _ = self.watch(
                clock, clock.sleep, settle=60.0, poll=30.0, hours=0.02
            )
        self.assertEqual(status, 1)
        self.assertEqual(len(reads), 1)

    def test_an_unchanged_journal_computes_nothing(self) -> None:
        self.write_events(release(1, START, remote=True))
        clock = Clock(START)
        calls: list[object] = []
        original = watch_local.group_runs

        def counting(runs: object) -> object:
            calls.append(runs)
            return original(runs)

        with mock.patch.object(watch_local, "group_runs", counting):
            status, _ = self.watch(
                clock, clock.sleep, settle=60.0, poll=30.0, hours=0.02
            )
        self.assertEqual(status, 1)
        self.assertEqual(len(calls), 1)

    def test_a_settled_report_does_not_read_the_journal_again(self) -> None:
        self.write_events(release(1, START))
        clock = Clock(START)
        calls: list[object] = []
        original = watch_local.group_runs

        def counting(runs: object) -> object:
            calls.append(runs)
            return original(runs)

        with mock.patch.object(watch_local, "group_runs", counting):
            status, text = self.watch(
                clock,
                clock.sleep,
                since=START - timedelta(minutes=1),
                settle=60.0,
                poll=30.0,
                hours=1.0,
            )
        self.assertEqual(status, 0)
        self.assertIn("local-runs: 1 pending", text)
        self.assertEqual(len(calls), 1)

    def test_marking_during_the_settle_does_not_wake(self) -> None:
        self.write_events(release(1, START))
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {"repo": "rustling-tulip", "shape": "cargo build --workspace"}
                },
            }
        )
        clock = Clock(START)
        sleeps: list[float] = []

        def sleep(seconds: float) -> None:
            sleeps.append(seconds)
            clock.sleep(seconds)
            if len(sleeps) == 1:
                with redirect_stdout(io.StringIO()):
                    watch_local.mark([key], "local", "decided", self.state)

        status, text = self.watch(
            clock,
            sleep,
            since=START - timedelta(minutes=1),
            settle=60.0,
            poll=30.0,
            hours=0.05,
        )
        self.assertEqual(status, 1)
        self.assertEqual(
            text.strip(), "local-runs: watch ended after 0.05 h with nothing pending"
        )

    def test_a_mark_made_during_a_watch_is_kept(self) -> None:
        self.write_events(
            release(1, START),
            release(1, START, command="cargo test --workspace"),
        )
        key = watch_local.shape_id("rustling-tulip", "cargo build --workspace")
        self.write_state(
            {
                "v": 1,
                "cursor": None,
                "shapes": {
                    key: {"repo": "rustling-tulip", "shape": "cargo build --workspace"}
                },
            }
        )
        clock = Clock(START)
        sleeps: list[float] = []

        def sleep(seconds: float) -> None:
            sleeps.append(seconds)
            clock.sleep(seconds)
            if len(sleeps) == 2:
                watch_local.mark([key], "local", "kept", self.state)

        status, _ = self.watch(
            clock,
            sleep,
            since=START - timedelta(minutes=1),
            settle=60.0,
            poll=30.0,
            hours=1.0,
        )
        self.assertEqual(status, 0)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["shapes"][key]["verdict"], "local")
        self.assertEqual(state["shapes"][key]["note"], "kept")
