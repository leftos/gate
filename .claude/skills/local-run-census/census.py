"""Census of the gate journal's heavy runs: which ran here, which routed, and why.

Reads the gate journal (``events.jsonl``, ``events.<n>.jsonl`` and the gzip
archives), groups its lines into runs by ``(pid, processStart)``, keeps the runs
that ended (or queued) in a window, and prints a Markdown report per
(repo, family) plus the ``--local`` runs. The report names repos and command
families only, never machine paths.
"""

from __future__ import annotations

import argparse
import gzip
import io
import json
import os
import re
import shlex
import statistics
import subprocess
import sys
import zlib
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path, PurePosixPath, PureWindowsPath
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator

SHELLS = frozenset({"pwsh", "powershell", "bash", "sh", "python", "py", "node", "cmd"})
PYTHONS = frozenset({"python", "py"})
SCRIPT_SUFFIXES = (".ps1", ".sh", ".py", ".js", ".mjs", ".cmd", ".bat")
INLINE_FLAGS = {"-c": "-c", "-command": "-Command", "/c": "/c"}
UV_VALUE_FLAGS = frozenset(
    {
        "--project",
        "--with",
        "--python",
        "--package",
        "--directory",
        "--extra",
        "--group",
        "--env-file",
        "--with-requirements",
        "--index",
        "-p",
    }
)
CARGO_VALUE_FLAGS = frozenset(
    {"-C", "--manifest-path", "--config", "-Z", "--color", "--target-dir"}
)
PROGRAM_SUFFIXES = (".exe", ".cmd", ".bat")
SEPARATORS = ("/", "\\")
# A path root at the start of a word: a drive, a UNC share, a `/`-rooted path of two or more
# segments (so `/c` and `/p:x` stay), a `~` home, or a `..` parent step.
PATH_ROOT = re.compile(
    r"(?<![\w./\\])(?:[A-Za-z]:[\\/]|\\\\|/(?=[^\s/\"']+/)|~[\\/]|\.\.[\\/])"
)
SELFTEST_MARKER = "gate-selftest"
NO_REPO = "(no repo)"
GONE = "(gone)"
CLAUDE_CONFIG = "claude-config"
NO_HEAVY_LOCAL = "No heavy local runs in the window."


# ---------------------------------------------------------------- paths


def scrub(text: str) -> str:
    """Replaces every machine path in ``text`` with ``<path>``.

    A path starts at a root (drive, UNC share, ``/``-rooted, ``~`` or ``..``) and runs
    to the next quote, through each following space-separated word that holds a path
    separator and is no flag, so a folder name with a space does not leak its tail.
    """
    pieces: list[str] = []
    position = 0
    while (match := PATH_ROOT.search(text, position)) is not None:
        pieces += [text[position : match.start()], "<path>"]
        position = _path_end(text, match.start())
    pieces.append(text[position:])
    return "".join(pieces)


def _path_end(text: str, start: int) -> int:
    quotes = [
        index for index in (text.find(quote, start) for quote in "\"'") if index != -1
    ]
    stop = min(quotes, default=len(text))
    end = start
    for number, token in enumerate(re.finditer(r"\S+", text[start:stop])):
        word = token.group()
        if number > 0 and (
            word.startswith("-") or not any(sep in word for sep in SEPARATORS)
        ):
            break
        end = start + token.end()
    return end


def _plain_relative(word: str) -> bool:
    if PATH_ROOT.match(word) or ":" in word:
        return False
    return ".." not in re.split(r"[\\/]", word)


def _script_word(word: str) -> str:
    normalized = word.replace("\\", "/")
    if _plain_relative(word):
        return normalized
    return ".../" + PurePosixPath(normalized).name


# ---------------------------------------------------------------- families


def split_command(command: str) -> list[str]:
    """Splits a command line as Windows quotes it, with each word's surrounding quotes removed."""
    try:
        words = shlex.split(command, posix=False)
    except ValueError:
        words = command.split()
    return [_unquote(word) for word in words]


def _unquote(word: str) -> str:
    if len(word) >= 2 and word[0] == word[-1] and word[0] in "\"'":
        return word[1:-1]
    return word


def program_name(word: str) -> str:
    """The lower-cased file name of a program word, without ``.exe``, ``.cmd`` or ``.bat``."""
    name = PureWindowsPath(word).name.lower()
    for suffix in PROGRAM_SUFFIXES:
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def _shell_family(program: str, args: list[str]) -> str:
    """The first of a script word, an inline-command flag or (for Python) ``-m <module>``; nothing after it is read."""
    for index, word in enumerate(args):
        flag = INLINE_FLAGS.get(word.lower())
        if flag is not None:
            return f"{program} {flag}"
        if program in PYTHONS and word == "-m":
            module = args[index + 1] if index + 1 < len(args) else ""
            return f"{program} -m {module}".rstrip()
        if word.lower().endswith(SCRIPT_SUFFIXES):
            return f"{program} {_script_word(word)}"
    return program


def _node_family(args: list[str]) -> str:
    """Node's family reads its flags and its first non-flag word only (the script, if any)."""
    for word in args:
        if word.startswith("-"):
            flag = INLINE_FLAGS.get(word.lower())
            if flag is not None:
                return f"node {flag}"
            continue
        if word.lower().endswith(SCRIPT_SUFFIXES):
            return f"node {_script_word(word)}"
        return "node"
    return "node"


def _skip_flags(words: list[str], value_flags: frozenset[str]) -> int:
    """The index of the first word that is no flag and no flag's value, or ``len(words)``."""
    index = 0
    while index < len(words):
        word = words[index]
        if word in value_flags:
            index += 2
        elif word.startswith(("-", "+")):
            index += 1
        else:
            return index
    return index


def _uv_family(args: list[str]) -> str | None:
    if "run" not in args:
        return None
    rest = args[args.index("run") + 1 :]
    index = _skip_flags(rest, UV_VALUE_FLAGS)
    if index >= len(rest):
        return "uv run"
    word = rest[index]
    target = program_name(word)
    if target in PYTHONS:
        return "uv run " + _shell_family(target, rest[index + 1 :])
    if any(sep in word for sep in SEPARATORS):
        return f"uv run {_script_word(word)}"
    return f"uv run {word}"


def _cargo_family(args: list[str]) -> str:
    index = _skip_flags(args, CARGO_VALUE_FLAGS)
    return f"cargo {args[index]}" if index < len(args) else "cargo"


def _other_family(program: str, args: list[str]) -> str:
    for word in args:
        if word.startswith(("-", "/")):
            continue
        if any(sep in word for sep in SEPARATORS) or "." in word:
            return program
        return f"{program} {word}"
    return program


def _raw_family(program: str, args: list[str]) -> str:
    if program == "node":
        return _node_family(args)
    if program in SHELLS:
        return _shell_family(program, args)
    if program == "uv":
        uv_family = _uv_family(args)
        if uv_family is not None:
            return uv_family
    if program == "cargo":
        return _cargo_family(args)
    return _other_family(program, args)


def family_of(command: str) -> str:
    """The command family a journal ``command`` belongs to (the rules are in SKILL.md)."""
    words = split_command(command)
    if not words:
        return "(empty)"
    return scrub(_raw_family(program_name(words[0]), words[1:]))


# ---------------------------------------------------------------- repos


class RepoNamer:
    """Names the repo a run's ``cwd`` belongs to, caching one answer per ``cwd``."""

    def __init__(self) -> None:
        self._cache: dict[str, str] = {}

    def __call__(self, cwd: str) -> str:
        if cwd not in self._cache:
            self._cache[cwd] = repo_of(cwd)
        return self._cache[cwd]


def _segments(path: str) -> list[str]:
    return [
        part for part in re.split(r"[\\/]+", path) if part and not part.endswith(":")
    ]


def repo_of(cwd: str) -> str:
    """The repo of a working folder.

    A ``<repo>.wt`` segment names its repo and a ``.claude-wt`` segment names
    claude-config; otherwise git names the main checkout. A folder git names no
    repo for is ``(no repo)`` and a folder that is gone is ``(gone)``: a folder's
    own name is never printed, since it may be a machine path's part.
    """
    segments = _segments(cwd)
    for segment in segments:
        if segment.lower() == ".claude-wt":
            return CLAUDE_CONFIG
        if segment.lower().endswith(".wt") and len(segment) > 3:
            return segment[:-3]
    if not segments:
        return NO_REPO
    if not Path(cwd).is_dir():
        return GONE
    common = _git_common_dir(cwd)
    return NO_REPO if common is None else repo_of_common_dir(common)


def repo_of_common_dir(common: str) -> str:
    """The repo a git common dir belongs to: the folder holding ``.git``, the superproject's for a submodule."""
    parts = _segments(common)
    lowered = [part.lower() for part in parts]
    if ".git" in lowered and lowered.index(".git") > 0:
        name = parts[lowered.index(".git") - 1]
    elif parts:
        name = parts[-1].removesuffix(".git")
    else:
        return NO_REPO
    return CLAUDE_CONFIG if name.lower() == ".claude" else name


def _git_common_dir(cwd: str) -> str | None:
    try:
        result = subprocess.run(
            [
                "git",
                "-C",
                cwd,
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        print(
            f"census: git could not name the repo of a folder: {error}", file=sys.stderr
        )
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


# ---------------------------------------------------------------- journal


@dataclass
class Journal:
    """Every journal line, with what was read, skipped and could not be read."""

    events: list[dict[str, Any]] = field(default_factory=list)
    files: list[str] = field(default_factory=list)
    unreadable: list[str] = field(default_factory=list)
    skipped: int = 0
    duplicates: int = 0


def journal_files(folder: Path) -> list[Path]:
    """The journal's files: the archives oldest first, then ``events.<n>.jsonl`` newest-last, then ``events.jsonl``."""
    archives = sorted((folder / "archive").glob("events-*.jsonl.gz"))
    rotated = [
        path
        for path in folder.glob("events.*.jsonl")
        if path.name.split(".")[1].isdigit()
    ]
    rotated.sort(key=lambda path: -int(path.name.split(".")[1]))
    current = folder / "events.jsonl"
    return [*archives, *rotated, *([current] if current.is_file() else [])]


def _lines(path: Path) -> Iterator[str]:
    if path.suffix == ".gz":
        with gzip.open(path, "rt", encoding="utf-8", errors="replace") as stream:
            yield from stream
    else:
        with path.open(encoding="utf-8", errors="replace") as stream:
            yield from stream


def parse_time(text: object) -> datetime | None:
    """An ISO 8601 journal time as an aware UTC datetime, or None when it is not one."""
    if not isinstance(text, str):
        return None
    try:
        moment = datetime.fromisoformat(text)
    except ValueError:
        return None
    return moment if moment.tzinfo else moment.replace(tzinfo=UTC)


def _event(line: str) -> dict[str, Any] | None:
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        return None
    if (
        not isinstance(event, dict)
        or event.get("v", 1) != 1
        or parse_time(event.get("time")) is None
    ):
        return None
    return event


def _read_file(path: Path, journal: Journal, seen: set[tuple[Any, ...]]) -> None:
    for line in _lines(path):
        if not line.strip():
            continue
        event = _event(line)
        if event is None:
            journal.skipped += 1
            continue
        identity = (
            event.get("pid"),
            event.get("processStart"),
            event.get("event"),
            event.get("time"),
        )
        if identity in seen:
            journal.duplicates += 1
            continue
        seen.add(identity)
        journal.events.append(event)


def read_journal(folder: Path) -> Journal:
    """Reads every journal line, skipping lines that do not parse or whose ``v`` is not 1.

    The newest archive holds the release lines of ``events.1.jsonl`` too, so a line
    already read (the same run, event and time) is counted as a duplicate and left out.
    A file that cannot be read keeps the lines read before the failure and is listed
    by name; the census goes on.
    """
    journal = Journal()
    seen: set[tuple[Any, ...]] = set()
    for path in journal_files(folder):
        name = path.relative_to(folder).as_posix()
        journal.files.append(name)
        try:
            _read_file(path, journal, seen)
        except (OSError, EOFError, zlib.error) as error:
            print(f"census: could not read all of {name}: {error}", file=sys.stderr)
            journal.unreadable.append(name)
    return journal


# ---------------------------------------------------------------- runs


@dataclass
class Run:
    """One gate run's lines, keyed by ``(pid, processStart)``."""

    queue: dict[str, Any] | None = None
    release: dict[str, Any] | None = None
    fallbacks: list[str] = field(default_factory=list)
    lent: bool = False
    last: datetime | None = None

    @property
    def head(self) -> dict[str, Any]:
        """The line the run's kind, ``cwd`` and ``command`` are read from: its ``queue``, else its ``release``.

        The archive keeps only ``release`` lines, and every line carries the common fields.
        """
        head = self.queue or self.release
        assert head is not None
        return head

    @property
    def moment(self) -> datetime | None:
        """When the run counts: its ``release`` time, else its ``queue`` time, else its last line's."""
        for line in (self.release, self.queue):
            if line is not None:
                return parse_time(line.get("time"))
        return self.last


def _add_line(run: Run, event: dict[str, Any]) -> None:
    kind = event.get("event")
    if kind == "queue":
        run.queue = event
    elif kind == "release":
        run.release = event
    elif kind == "fallback":
        run.fallbacks.append(str(event.get("reason")))
    elif kind in {"covered", "parent"}:
        run.lent = True
    moment = parse_time(event.get("time"))
    if moment is not None and (run.last is None or moment > run.last):
        run.last = moment


def group_runs(
    events: Iterable[dict[str, Any]], start: datetime, end: datetime
) -> tuple[list[Run], int]:
    """Joins lines into runs and keeps those whose moment falls in ``[start, end]``.

    Returns the kept runs with a ``queue`` or ``release`` line, and the count of kept
    runs with neither. A self-test run (its ``cwd`` holds ``gate-selftest``) is left out.
    """
    runs: dict[tuple[Any, Any], Run] = defaultdict(Run)
    for event in events:
        if SELFTEST_MARKER in str(event.get("cwd", "")):
            continue
        _add_line(runs[(event.get("pid"), event.get("processStart"))], event)
    kept = [
        run
        for run in runs.values()
        if run.moment is not None and start <= run.moment <= end
    ]
    headed = [run for run in kept if run.queue is not None or run.release is not None]
    return headed, len(kept) - len(headed)


@dataclass
class Family:
    """The figures of one (repo, family) of heavy runs."""

    local_seconds: list[float] = field(default_factory=list)
    routed_seconds: list[float] = field(default_factory=list)
    local: int = 0
    routed: int = 0
    unfinished: int = 0
    verdicts: Counter[str] = field(default_factory=Counter)
    not_routed: Counter[str] = field(default_factory=Counter)
    fallbacks: Counter[str] = field(default_factory=Counter)

    def add(self, run: Run) -> None:
        """Counts one heavy run into the family."""
        not_routed = run.queue.get("notRouted") if run.queue else None
        if not_routed:
            self.not_routed[str(not_routed)] += 1
        self.fallbacks.update(run.fallbacks)
        if run.release is None:
            self.unfinished += 1
            return
        seconds = run.release.get("wallSeconds")
        routed = run.release.get("remote") is True
        if routed:
            self.routed += 1
        else:
            self.local += 1
            self.verdicts[str(run.release.get("verdict"))] += 1
        if isinstance(seconds, int | float):
            (self.routed_seconds if routed else self.local_seconds).append(
                float(seconds)
            )


@dataclass
class Census:
    """The census of one window."""

    families: dict[tuple[str, str], Family]
    local_runs: Counter[tuple[str, str, str]]
    lent: int


def take_census(runs: Iterable[Run], name_repo: RepoNamer) -> Census:
    """Sums the heavy runs per (repo, family) and the ``--local`` runs of every kind.

    A heavy run lent or covered by its parent's slot, with no ``release`` of its own,
    is counted apart and left out of the figures.
    """
    families: dict[tuple[str, str], Family] = defaultdict(Family)
    local_runs: Counter[tuple[str, str, str]] = Counter()
    lent = 0
    for run in runs:
        head = run.head
        key = (
            name_repo(str(head.get("cwd", ""))),
            family_of(str(head.get("command", ""))),
        )
        if head.get("notRouted") == "--local":
            local_runs[(*key, str(head.get("reason", "")))] += 1
        if head.get("kind") != "heavy":
            continue
        if run.lent and run.release is None:
            lent += 1
            continue
        families[key].add(run)
    return Census(dict(families), local_runs, lent)


# ---------------------------------------------------------------- output


def _median(values: list[float]) -> float | None:
    return round(statistics.median(values), 1) if values else None


def _counts(counter: Counter[str]) -> str:
    return ", ".join(f"{name} {count}" for name, count in counter.most_common()) or "-"


def _cell(value: object) -> str:
    text = "-" if value is None else str(value)
    return text.replace("|", "\\|")


def heavy_rows(census: Census) -> list[dict[str, Any]]:
    """The heavy-family rows, largest total local seconds first."""
    rows = [
        {
            "repo": repo,
            "family": family,
            "local": figures.local,
            "routed": figures.routed,
            "unfinished": figures.unfinished,
            "localSecondsTotal": round(sum(figures.local_seconds), 1),
            "localMedianSeconds": _median(figures.local_seconds),
            "routedMedianSeconds": _median(figures.routed_seconds),
            "localVerdicts": dict(figures.verdicts.most_common()),
            "notRouted": dict(figures.not_routed.most_common()),
            "fallbacks": dict(figures.fallbacks.most_common()),
        }
        for (repo, family), figures in census.families.items()
    ]
    rows.sort(key=lambda row: (-row["localSecondsTotal"], row["repo"], row["family"]))
    return rows


def local_rows(census: Census) -> list[dict[str, Any]]:
    """The ``--local`` runs per repo, family and reason, most first."""
    rows = [
        {"repo": repo, "family": family, "reason": scrub(reason), "runs": count}
        for (repo, family, reason), count in census.local_runs.items()
    ]
    rows.sort(key=lambda row: (-row["runs"], row["repo"], row["family"], row["reason"]))
    return rows


def _heavy_table(rows: list[dict[str, Any]]) -> list[str]:
    lines = [
        "## Heavy families",
        "",
        "| Repo | Family | Local | Routed | Unfinished | Local s total | Local median s | Routed median s | Local verdicts | notRouted | Fallbacks |",
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for row in rows:
        cells = [
            row["repo"],
            f"`{row['family']}`",
            row["local"],
            row["routed"],
            row["unfinished"],
            row["localSecondsTotal"],
            row["localMedianSeconds"],
            row["routedMedianSeconds"],
            _counts(Counter(row["localVerdicts"])),
            _counts(Counter(row["notRouted"])),
            _counts(Counter(row["fallbacks"])),
        ]
        lines.append("| " + " | ".join(_cell(cell) for cell in cells) + " |")
    if not any(row["local"] for row in rows):
        lines += ["", NO_HEAVY_LOCAL]
    return lines


def _local_table(rows: list[dict[str, Any]]) -> list[str]:
    lines = [
        "## `--local` runs",
        "",
        "| Repo | Family | Reason | Runs |",
        "|---|---|---|---|",
    ]
    for row in rows:
        cells = [row["repo"], f"`{row['family']}`", row["reason"] or "-", row["runs"]]
        lines.append("| " + " | ".join(_cell(cell) for cell in cells) + " |")
    return lines


def _footer(data: dict[str, Any]) -> list[str]:
    window = data["window"]
    archives = [name for name in data["files"] if name.startswith("archive/")]
    live = [name for name in data["files"] if not name.startswith("archive/")]
    queues_from = data["queuesFrom"]
    if queues_from is None:
        coverage = "cover no run: no queue line is left in the journal."
    elif queues_from <= window["start"]:
        coverage = "cover the whole window."
    else:
        coverage = f"cover only the runs queued from {queues_from}."
    return [
        f"- Window: {window['start']} to {window['end']} (UTC).",
        (
            f"- Files read: {len(archives)} archives, {', '.join(live) or 'no live file'}; "
            f"unreadable: {', '.join(data['unreadable']) or 'none'}."
        ),
        f"- Skipped lines: {data['skipped']}; duplicate lines left out: {data['duplicates']}.",
        (
            f"- Runs with neither a queue nor a release line in the window: {data['headless']}; "
            f"heavy runs lent or covered, with no release of their own, left out: {data['lent']}."
        ),
        f"- The archive keeps only release lines, so notRouted, the fallbacks and the --local table {coverage}",
    ]


def render(data: dict[str, Any]) -> str:
    """The Markdown report of a census's JSON data."""
    lines = [
        *_heavy_table(data["heavy"]),
        "",
        *_local_table(data["local"]),
        "",
        *_footer(data),
    ]
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------- entry


def _window(args: argparse.Namespace, now: datetime) -> datetime:
    if args.since is None:
        return now - timedelta(days=args.days)
    start = parse_time(args.since)
    if start is None:
        sys.exit(
            f"census: --since {args.since!r} is not an ISO 8601 time, such as 2026-10-20T00:00:00Z"
        )
    return start


def _default_journal() -> Path:
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        sys.exit(
            "census: LOCALAPPDATA is not set; pass --journal <folder holding events.jsonl>"
        )
    return Path(local) / "gate" / "v1"


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Census of the gate journal's heavy runs over a window."
    )
    when = parser.add_mutually_exclusive_group()
    when.add_argument(
        "--days",
        type=float,
        default=7.0,
        help="the window's length, ending now (default 7)",
    )
    when.add_argument(
        "--since",
        help="the window's start, an ISO 8601 time (UTC when it names no zone)",
    )
    parser.add_argument(
        "--journal",
        type=Path,
        help="the journal folder (default %%LOCALAPPDATA%%\\gate\\v1)",
    )
    parser.add_argument(
        "--json", type=Path, help="also write the census as JSON to this file"
    )
    return parser.parse_args(argv)


def _first_queue(events: Iterable[dict[str, Any]]) -> str | None:
    times = [
        moment
        for event in events
        if event.get("event") == "queue" and (moment := parse_time(event.get("time")))
    ]
    return min(times).isoformat(timespec="seconds") if times else None


def census_data(
    journal: Journal, start: datetime, end: datetime, name_repo: RepoNamer
) -> dict[str, Any]:
    """The census of a journal over ``[start, end]``, as the JSON the report renders from."""
    runs, headless = group_runs(journal.events, start, end)
    census = take_census(runs, name_repo)
    return {
        "window": {
            "start": start.isoformat(timespec="seconds"),
            "end": end.isoformat(timespec="seconds"),
        },
        "files": journal.files,
        "unreadable": journal.unreadable,
        "skipped": journal.skipped,
        "duplicates": journal.duplicates,
        "headless": headless,
        "queuesFrom": _first_queue(journal.events),
        "lent": census.lent,
        "heavy": heavy_rows(census),
        "local": local_rows(census),
    }


def main(argv: list[str] | None = None) -> int:
    """Runs the census and prints its report."""
    args = _parse_args(argv)
    now = datetime.now(UTC)
    start = _window(args, now)
    folder = args.journal or _default_journal()
    if not journal_files(folder):
        sys.exit(
            f"census: no journal files in {folder}; pass --journal <folder holding events.jsonl>"
        )
    data = census_data(read_journal(folder), start, now, RepoNamer())
    if args.json is not None:
        args.json.write_text(
            json.dumps(data, indent=2) + "\n", encoding="utf-8", newline="\n"
        )
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(encoding="utf-8")
    sys.stdout.write(render(data))
    return 0


if __name__ == "__main__":
    sys.exit(main())
