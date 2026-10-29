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
from bisect import bisect_left, bisect_right
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
COMMIT_FAMILY = "git commit"
UNGATED_FAMILY = "git commit (ungated hooks)"
UNGATED_BLOCKER = "ungated hook commands in the repo's git hooks"
BLOCKED_ON_WORK = "blocked on work"
LINKED_BY_PARENT_PID = "parentPid"
LINKED_BY_CONTAINMENT = "containment"
CONTAINMENT_SLACK = timedelta(seconds=1)


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
    first: dict[str, Any] | None = None
    pid: int | None = None
    process_start: datetime | None = None
    parent_pid: int | None = None
    parent: Run | None = None
    parent_method: str = ""
    commit: bool = False
    commit_cache: bool | None = None
    taken: bool = False
    split: bool = False
    nested: list[Run] = field(default_factory=list)

    @property
    def head(self) -> dict[str, Any]:
        """The line the run's kind, ``cwd`` and ``command`` are read from: its ``queue``, else its ``release``, else its first.

        The archive keeps only ``release`` lines, every line carries the common fields, and a
        release carries the run's routing facts, so a run read from its release alone still
        says where it ran and why it stayed here.
        """
        head = self.queue or self.release or self.first
        assert head is not None
        return head

    @property
    def moment(self) -> datetime | None:
        """When the run counts: its ``release`` time, else its ``queue`` time, else its last line's."""
        for line in (self.release, self.queue):
            if line is not None:
                return parse_time(line.get("time"))
        return self.last


def _mark_nesting(run: Run, kind: object, event: dict[str, Any]) -> None:
    """Keeps what nesting reads from a ``take``, ``covered`` or ``parent`` line: its flags and parent pid."""
    if kind == "take":
        run.taken = True
    if kind in {"take", "covered", "parent"} and isinstance(
        event.get("parentPid"), int
    ):
        run.parent_pid = event["parentPid"]


def _keep_last(run: Run, event: dict[str, Any]) -> None:
    """Keeps the latest line time of the run."""
    moment = parse_time(event.get("time"))
    if moment is not None and (run.last is None or moment > run.last):
        run.last = moment


def _release_facts(run: Run) -> None:
    """Keeps the routing facts off a run's ``release`` line once the other lines of the run may be gone.

    The gate copies the run's ``notRouted`` and ``reason`` from its ``queue`` line, why it gave up
    each remote attempt on ``fellBack``, and its ``take`` line's ``parentPid`` onto its release, the
    one line the archive keeps. ``fellBack`` is the run's whole list of fallbacks, so it is read
    whatever lines survive: a run whose ``queue`` and ``fallback`` lines rolled away while its
    ``take`` and release stayed keeps its fallback reasons. ``notRouted``, ``reason`` and
    ``parentPid`` are read from the release only when no ``queue``, ``fallback``, ``take`` or lent
    line of the run is left, so a live day's figures are unchanged.
    """
    release = run.release
    if release is None:
        return
    had_lines = run.queue is not None or bool(run.fallbacks) or run.taken or run.lent
    fell_back = release.get("fellBack")
    if isinstance(fell_back, list):
        run.fallbacks = [str(reason) for reason in fell_back]
    if had_lines:
        return
    if run.parent_pid is None and isinstance(release.get("parentPid"), int):
        run.parent_pid = release["parentPid"]


def _add_line(run: Run, event: dict[str, Any]) -> None:
    """Adds one journal line to its run, keeping what nesting reads: the parent pid and the ``commit`` flag."""
    kind = event.get("event")
    if run.first is None:
        run.first = event
    if kind == "queue":
        run.queue = event
        run.commit = run.commit or event.get("commit") is True
    elif kind == "release":
        run.release = event
        run.commit = run.commit or event.get("commit") is True
    elif kind == "fallback":
        run.fallbacks.append(str(event.get("reason")))
    elif kind in {"covered", "parent"}:
        run.lent = True
    _mark_nesting(run, kind, event)
    _keep_last(run, event)


def is_commit(run: Run) -> bool:
    """Whether a run is a ``git commit``: its head says ``commit``, or, older than that flag, its command's first words do."""
    if run.commit_cache is None:
        run.commit_cache = (
            run.commit or family_of(str(run.head.get("command", ""))) == COMMIT_FAMILY
        )
    return run.commit_cache


def _splits(run: Run) -> bool:
    """Whether the census splits a commit run: a finished, heavy commit, no one's child, with gates nested in it."""
    return (
        run.parent is None
        and run.release is not None
        and run.head.get("kind") == "heavy"
        and is_commit(run)
        and bool(run.nested)
    )


def _runs_by_pid(
    runs: list[Run],
) -> tuple[dict[int, list[Run]], dict[int, list[datetime]]]:
    """Every run with a pid and a start, grouped by pid, sorted by start, with their starts beside them."""
    by_pid: dict[int, list[Run]] = defaultdict(list)
    for run in runs:
        if run.pid is not None and run.process_start is not None:
            by_pid[run.pid].append(run)
    starts: dict[int, list[datetime]] = {}
    for pid, same in by_pid.items():
        same.sort(key=lambda run: run.process_start)
        starts[pid] = [run.process_start for run in same]
    return by_pid, starts


def _parent_by_pid(
    run: Run,
    by_pid: dict[int, list[Run]],
    starts: dict[int, list[datetime]],
) -> Run | None:
    """The commit run the child names by pid, when the child started before that commit released."""
    same = by_pid.get(run.parent_pid)
    if not same:
        return None
    index = bisect_left(starts[run.parent_pid], run.process_start)
    if index == 0:
        return None
    parent = same[index - 1]
    if not is_commit(parent):
        return None
    if parent.release is not None:
        released = parse_time(parent.release.get("time"))
        if released is not None and run.process_start >= released:
            return None
    return parent


def _link_by_parent_pid(runs: list[Run]) -> None:
    """Links a run whose ``take``, ``covered``, ``parent`` or ``release`` line names a pid to that pid's run.

    The candidate is the latest run with that pid that started before the child, so a
    reused pid names the process that held it then. The link is made only when that run
    is a commit run and the child started before its release, or when it has no release:
    a commit that had already released cannot have begun a child.
    """
    by_pid, starts = _runs_by_pid(runs)
    for run in runs:
        if (
            run.parent is not None
            or run.parent_pid is None
            or run.process_start is None
        ):
            continue
        parent = _parent_by_pid(run, by_pid, starts)
        if parent is not None:
            run.parent = parent
            run.parent_method = LINKED_BY_PARENT_PID


@dataclass
class _Folder:
    """A folder's finished commit runs, sorted by start, and how long its longest one ran."""

    starts: list[datetime]
    entries: list[tuple[datetime, datetime, Run]]
    longest: timedelta


def _norm_cwd(cwd: object) -> str:
    """A folder as containment compares it: lower case, backslashes, no trailing separator."""
    return str(cwd).lower().replace("/", "\\").rstrip("\\")


def _folders_of(cwd: object) -> Iterator[str]:
    """A folder and each folder above it, normalized, deepest first."""
    folder = _norm_cwd(cwd)
    while folder:
        yield folder
        cut = folder.rfind("\\")
        if cut <= 0:
            return
        folder = folder[:cut]


def _commit_folders(runs: list[Run]) -> dict[str, _Folder]:
    """The finished commit runs per normalized folder, sorted by start, with the folder's longest wall time."""
    found: dict[str, list[tuple[datetime, datetime, Run]]] = defaultdict(list)
    for run in runs:
        if run.parent is not None or run.release is None or run.process_start is None:
            continue
        released = parse_time(run.release.get("time"))
        if released is None or not is_commit(run):
            continue
        found[_norm_cwd(run.head.get("cwd", ""))].append(
            (run.process_start, released, run)
        )
    folders: dict[str, _Folder] = {}
    for cwd, entries in found.items():
        entries.sort(key=lambda entry: entry[0])
        folders[cwd] = _Folder(
            starts=[entry[0] for entry in entries],
            entries=entries,
            longest=max(entry[1] - entry[0] for entry in entries),
        )
    return folders


def _containable(run: Run) -> bool:
    """Whether containment may read a run: a bare archive release that names no parent of its own."""
    return (
        run.parent is None
        and run.parent_pid is None
        and run.release is not None
        and run.queue is None
        and not run.taken
        and not run.lent
    )


def _contained_in(run: Run, released: datetime, folder: _Folder) -> Run | None:
    """The latest commit in one folder whose span holds the run, walking back only as far as a commit can reach."""
    oldest = run.process_start - folder.longest
    index = bisect_right(folder.starts, run.process_start) - 1
    while index >= 0:
        start, commit_released, parent = folder.entries[index]
        if start < oldest:
            return None
        if parent is not run and released <= commit_released + CONTAINMENT_SLACK:
            return parent
        index -= 1
    return None


def _contained(run: Run, folders: dict[str, _Folder]) -> Run | None:
    """The commit a bare-release gate released under: the latest one in a folder that holds it, if any."""
    released = parse_time(run.release.get("time"))
    if released is None:
        return None
    best: Run | None = None
    for folder in _folders_of(run.head.get("cwd", "")):
        found = folders.get(folder)
        if found is None:
            continue
        parent = _contained_in(run, released, found)
        if parent is not None and (
            best is None or parent.process_start > best.process_start
        ):
            best = parent
    return best


def _link_by_containment(runs: list[Run]) -> None:
    """Links a gate the archive kept as a bare ``release`` to the commit it released under.

    The archive keeps only ``release`` lines, so a gate nested in a commit's hooks that names
    no parent is read from what is left: a folder that is the commit's or inside it, once both are
    normalized, a ``processStart`` at or after the commit's, and a release at most a
    second past the commit's. Only a run the archive kept as a release alone is read
    this way: a live run says for itself whether it is nested, and a run that names a
    ``parentPid`` belongs to that run, never to a commit.
    """
    folders = _commit_folders(runs)
    for run in runs:
        if not _containable(run) or run.process_start is None:
            continue
        parent = _contained(run, folders)
        if parent is not None:
            run.parent = parent
            run.parent_method = LINKED_BY_CONTAINMENT


def _link_parents(runs: list[Run]) -> None:
    """Links every gate nested in a commit run to it, by ``parentPid`` first, else by containment."""
    _link_by_parent_pid(runs)
    _link_by_containment(runs)


def _group_lines(events: Iterable[dict[str, Any]]) -> list[Run]:
    """Joins the journal lines into runs, each run with its pid and start, a self-test run left out."""
    runs: dict[tuple[Any, Any], Run] = defaultdict(Run)
    for event in events:
        if SELFTEST_MARKER in str(event.get("cwd", "")):
            continue
        _add_line(runs[(event.get("pid"), event.get("processStart"))], event)
    for (pid, started), run in runs.items():
        run.pid = pid if isinstance(pid, int) else None
        run.process_start = parse_time(started)
        _release_facts(run)
    return list(runs.values())


def group_runs(
    events: Iterable[dict[str, Any]], start: datetime, end: datetime
) -> tuple[list[Run], int]:
    """Joins lines into runs, links the gates nested in commits, and keeps the window's runs.

    A run is kept by its own moment. A gate nested in a commit run that will be split
    is counted with that commit alone: it is kept when the commit is in the window and
    dropped otherwise, whatever its own moment. Returns the kept runs with a ``queue``
    or ``release`` line or a split commit parent, and the count of the other kept runs.
    A self-test run (its ``cwd`` holds ``gate-selftest``) is left out.
    """
    every = _group_lines(events)
    _link_parents(every)
    for run in every:
        if run.parent is not None:
            run.parent.nested.append(run)
    for run in every:
        run.split = _splits(run)
    inside = {
        id(run)
        for run in every
        if run.moment is not None and start <= run.moment <= end
    }

    def kept_run(run: Run) -> bool:
        """Whether the window counts the run: with its split commit, else by its own moment."""
        parent = run.parent
        if parent is not None and parent.split:
            return id(parent) in inside
        return id(run) in inside

    kept = [run for run in every if kept_run(run)]
    headed = [
        run
        for run in kept
        if run.queue is not None
        or run.release is not None
        or (run.parent is not None and run.parent.split)
    ]
    return headed, len(kept) - len(headed)


@dataclass
class Family:
    """The figures of one (repo, family) of heavy runs."""

    local_seconds: list[float] = field(default_factory=list)
    routed_seconds: list[float] = field(default_factory=list)
    local: int = 0
    routed: int = 0
    unfinished: int = 0
    covered: int = 0
    verdicts: Counter[str] = field(default_factory=Counter)
    not_routed: Counter[str] = field(default_factory=Counter)
    fallbacks: Counter[str] = field(default_factory=Counter)

    def add(self, run: Run, covered: bool) -> None:
        """Counts one heavy run into the family; a covered run counts as local with no seconds."""
        if covered:
            self.local += 1
            self.covered += 1
            return
        not_routed = (run.queue or run.release or {}).get("notRouted")
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

    def add_share(self, seconds: float) -> None:
        """Counts one commit's uncovered share: a local run with its seconds and no verdict."""
        self.local += 1
        self.local_seconds.append(seconds)


@dataclass
class CommitSplit:
    """What the census read of commit runs and the gates nested in them, per repo."""

    commits: Counter[str] = field(default_factory=Counter)
    with_nested: Counter[str] = field(default_factory=Counter)
    uncovered: dict[str, float] = field(default_factory=lambda: defaultdict(float))
    unsplit: Counter[str] = field(default_factory=Counter)
    by_parent_pid: Counter[str] = field(default_factory=Counter)
    by_containment: Counter[str] = field(default_factory=Counter)

    def as_data(self) -> dict[str, dict[str, Any]]:
        """The split as the census's JSON key: one entry per repo with a commit run."""
        repos = sorted(set(self.commits) | set(self.uncovered))
        return {
            repo: {
                "commits": self.commits[repo],
                "withNested": self.with_nested[repo],
                "uncoveredSeconds": round(self.uncovered.get(repo, 0.0), 1),
                "unsplitCommits": self.unsplit[repo],
                "linkedByParentPid": self.by_parent_pid[repo],
                "linkedByContainment": self.by_containment[repo],
            }
            for repo in repos
        }


@dataclass
class Census:
    """The census of one window."""

    families: dict[tuple[str, str], Family]
    local_runs: Counter[tuple[str, str, str]]
    lent: int
    split: CommitSplit = field(default_factory=CommitSplit)


def _wall_seconds(line: dict[str, Any] | None) -> float | None:
    """A release line's ``wallSeconds``, or None when the run measured none."""
    if line is None:
        return None
    seconds = line.get("wallSeconds")
    return float(seconds) if isinstance(seconds, int | float) else None


def _count_nested(
    child: Run,
    repo: str,
    families: dict[tuple[str, str], Family],
    split: CommitSplit,
    name_repo: RepoNamer,
) -> float | None:
    """Counts one nested gate in its own family and its link method, and gives its seconds, or None if it had none."""
    head = child.head
    key = (
        name_repo(str(head.get("cwd", ""))),
        family_of(str(head.get("command", ""))),
    )
    if head.get("kind") == "heavy":
        families[key].add(child, covered=child.release is None and child.lent)
    if child.parent_method == LINKED_BY_PARENT_PID:
        split.by_parent_pid[repo] += 1
    elif child.parent_method == LINKED_BY_CONTAINMENT:
        split.by_containment[repo] += 1
    return _wall_seconds(child.release)


def _add_share(
    run: Run,
    nested_seconds: float,
    repo: str,
    families: dict[tuple[str, str], Family],
    split: CommitSplit,
) -> None:
    """Adds the commit's uncovered share as its own row, unless it measured no wall time or no share."""
    wall = _wall_seconds(run.release)
    if wall is None:
        return
    share = max(0.0, wall - nested_seconds)
    if share <= 0:
        return
    families[(repo, UNGATED_FAMILY)].add_share(share)
    split.uncovered[repo] += share


def _split_commit(
    run: Run,
    repo: str,
    families: dict[tuple[str, str], Family],
    split: CommitSplit,
    name_repo: RepoNamer,
) -> None:
    """Counts a commit's nested gates in their own families and, when their time is known, its uncovered share.

    A gate with no ``release`` has no time to take out of the commit's wall seconds, so
    a commit with one of those is not split at all: its whole wall time counts in
    ``git commit``, truly local, and the footer counts it.
    """
    split.commits[repo] += 1
    split.with_nested[repo] += 1
    nested_seconds = 0.0
    unknown = False
    for child in run.nested:
        seconds = _count_nested(child, repo, families, split, name_repo)
        if seconds is None:
            unknown = True
        else:
            nested_seconds += seconds
    if unknown:
        split.unsplit[repo] += 1
        families[(repo, COMMIT_FAMILY)].add(run, covered=False)
        return
    _add_share(run, nested_seconds, repo, families, split)


def take_census(runs: Iterable[Run], name_repo: RepoNamer) -> Census:
    """Sums the heavy runs per (repo, family) and the ``--local`` runs of every kind.

    A heavy commit run with gates nested in it is split: each nested gate counts in its
    own family, and the commit's share no nested gate covers becomes its own
    ``git commit (ungated hooks)`` row. A heavy run lent or covered by its parent's slot,
    with no ``release`` of its own and no split commit parent, is counted apart and left
    out. A nested gate still counts in the ``--local`` table, as a top-level run does.
    """
    runs = list(runs)
    families: dict[tuple[str, str], Family] = defaultdict(Family)
    local_runs: Counter[tuple[str, str, str]] = Counter()
    split = CommitSplit()
    lent = 0
    for run in runs:
        head = run.head
        key = (
            name_repo(str(head.get("cwd", ""))),
            family_of(str(head.get("command", ""))),
        )
        if head.get("notRouted") == "--local":
            local_runs[(*key, str(head.get("reason", "")))] += 1
        if run.parent is not None and run.parent.split:
            continue
        if head.get("kind") != "heavy":
            continue
        if run.lent and run.release is None:
            lent += 1
            continue
        if run.split:
            _split_commit(run, key[0], families, split, name_repo)
            continue
        if is_commit(run):
            split.commits[key[0]] += 1
        families[key].add(run, covered=False)
    return Census(dict(families), local_runs, lent, split)


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
    rows = []
    for (repo, family), figures in census.families.items():
        row = {
            "repo": repo,
            "family": family,
            "local": figures.local,
            "routed": figures.routed,
            "unfinished": figures.unfinished,
            "covered": figures.covered,
            "localSecondsTotal": round(sum(figures.local_seconds), 1),
            "localMedianSeconds": _median(figures.local_seconds),
            "routedMedianSeconds": _median(figures.routed_seconds),
            "localVerdicts": dict(figures.verdicts.most_common()),
            "notRouted": dict(figures.not_routed.most_common()),
            "fallbacks": dict(figures.fallbacks.most_common()),
        }
        if family == UNGATED_FAMILY:
            row["state"] = BLOCKED_ON_WORK
            row["blocker"] = UNGATED_BLOCKER
        rows.append(row)
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
        (
            "| Repo | Family | Local | Routed | Unfinished | Covered | Local s total "
            "| Local median s | Routed median s | Local verdicts | notRouted "
            "| Fallbacks | State |"
        ),
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for row in rows:
        state = row.get("state")
        if state and row.get("blocker"):
            state = f"{state}: {row['blocker']}"
        cells = [
            row["repo"],
            f"`{row['family']}`",
            row["local"],
            row["routed"],
            row["unfinished"],
            row.get("covered", 0),
            row["localSecondsTotal"],
            row["localMedianSeconds"],
            row["routedMedianSeconds"],
            _counts(Counter(row["localVerdicts"])),
            _counts(Counter(row["notRouted"])),
            _counts(Counter(row["fallbacks"])),
            state or "-",
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
        coverage = "no live queue line is left in the journal."
    elif queues_from <= window["start"]:
        coverage = "the live queue lines cover the whole window."
    else:
        coverage = f"the live queue lines cover only the runs queued from {queues_from}."
    split = data["commitSplit"]
    by_parent_pid = sum(entry["linkedByParentPid"] for entry in split.values())
    by_containment = sum(entry["linkedByContainment"] for entry in split.values())
    unsplit = sum(entry["unsplitCommits"] for entry in split.values())
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
        (
            "- The archive keeps only release lines: a release carries notRouted, reason, fellBack "
            f"and parentPid, and one journalled before the gate wrote them keeps none; {coverage}"
        ),
        f"- nested gates linked by parentPid: {by_parent_pid}; by containment: {by_containment}.",
        f"- commits with covered hook gates, share not split: {unsplit}.",
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
        "commitSplit": census.split.as_data(),
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
