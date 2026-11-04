"""Wake a gate session when a new kind of heavy gate ran on this PC.

Reads the gate journal's release lines, groups the heavy runs that stayed here for
at least a minimum time into normalized command shapes, and prints for each pending
shape whether a pattern matched its latest run, as the gate journalled it, or matches
its words now, as ``gate remote match`` says, how many of the repo's command family
routed instead, the nearest pattern of that family and how it differs, so the session
can decide whether to route it. The gate is the only matcher.

A shape is pending until it is marked ``local`` or ``booked``, and an unmarked shape
stays in every report with the lines it last had; a shape marked ``routed`` that ran
here again after the fix is pending, flagged as still local after it, and stays in
every report until it is marked again. One decision may cover several shapes. The
decisions and the cursor live in a state file under ``.tmp/``, whose loss only means a
shape is looked at again.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import zlib
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "local-run-census"))
# The census is the skill beside this one, so its folder is put on the path first.
import census

SELFTEST_MARKER = "gate-selftest"
KEPT_HERE = frozenset({"git", "prek"})
# A `.tmp` path after the start of a word, up to whitespace or a quote, so a quoted
# path keeps its closing quote.
TMP_PATTERN = re.compile(r"(?<![\w./\\:])\.tmp/[^\s\"']+")
# A number that stands as a word or follows `=`: a digit, then digits and `.,:-`, then
# whitespace or the end. A digit inside a name (`net8.0`) or a path (`5.2/b.exe`) stays.
NUMBER_TOKEN = re.compile(r"(?<=[\s=])\d[\d.,:-]*(?=\s|$)")
CURSOR_HOURS = 24
ARCHIVE_DAYS = 7
DEFAULT_MIN_SECONDS = 60.0
DEFAULT_SETTLE = 300.0
DEFAULT_HOURS = 8.0
DEFAULT_POLL = 30.0
VERDICTS = ("routed", "local", "booked")
NO_PENDING = "local-runs: nothing pending"
STILL_LOCAL = "STILL-LOCAL-AFTER-FIX"
UNREAD = object()
# The census's name for a repo, where the key `remote.json` uses for it differs.
GATE_KEYS = {"claude-config": "claude"}
# The `notRouted` token of a command none of its repo's patterns matched.
NO_PATTERN = "no pattern"
# What asks the gate which pattern a command of a repo matches, without the two operands.
MATCH_CALL = ("pwsh", "-NoProfile", "-File", "tools/gate.ps1", "remote", "match")
# Whether a pattern of the repo matches the words now.
Ask = Callable[[str, str], bool]


class LocalRunsError(Exception):
    """A file the run cannot read or write, said as ``local-runs: <text>``."""


def gate_match(repo: str, text: str) -> bool:
    """Whether a pattern of ``repo`` matches ``text`` now, as ``gate remote match`` says.

    Status 0 is a match and 1 none; any other status raises with the command and its
    output.
    """
    command = [*MATCH_CALL, repo, text]
    shown = " ".join(command)
    try:
        done = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as error:
        raise LocalRunsError(f"cannot run {shown}: {error}") from error
    if done.returncode not in (0, 1):
        output = (done.stdout + done.stderr).strip()
        raise LocalRunsError(f"{shown} ended with status {done.returncode}: {output}")
    return done.returncode == 0


@dataclass
class Options:
    """Where a run reads and writes, and the times and counts it judges with."""

    journal: Path
    remote: Path
    state: Path
    min_seconds: float = DEFAULT_MIN_SECONDS
    settle: float = DEFAULT_SETTLE
    poll: float = DEFAULT_POLL
    hours: float = DEFAULT_HOURS
    ask: Ask = gate_match


# ---------------------------------------------------------------- paths


def local_app_data() -> Path:
    """The PC's local application data folder."""
    found = os.environ.get("LOCALAPPDATA")
    return Path(found) if found else Path.home() / "AppData" / "Local"


def default_journal() -> Path:
    """The gate journal's folder."""
    return local_app_data() / "gate" / "v1"


def default_remote() -> Path:
    """The PC's ``remote.json``."""
    return local_app_data() / "gate" / "remote.json"


def default_state() -> Path:
    """The state file, under the current folder's ``.tmp/``."""
    return Path(".tmp") / "local-runs-state.json"


def stamp(moment: datetime) -> str:
    """An aware datetime as the journal's UTC ISO string, with seven fractional digits."""
    return moment.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%S.%f") + "0Z"


def utcnow() -> datetime:
    """The current time, aware UTC."""
    return datetime.now(UTC)


# ---------------------------------------------------------------- shapes


def command_text(event: dict[str, Any]) -> str:
    """The words a run's shape is read from: those the gate matched, else its command with its folder taken out.

    A ``--local`` run and a line written before the gate journalled its words have only
    the command, written as the words given in the run's own folder, so that folder and the
    separator after it are taken out, in either letter case and with either separator.
    """
    sent = event.get("sentWords")
    if sent:
        return str(sent)
    command = str(event.get("command") or "")
    cwd = str(event.get("cwd") or "")
    if not cwd:
        return command
    return re.sub(re.escape(cwd) + r"[\\/]", "", command, count=1, flags=re.IGNORECASE)


def shape_of(text: str) -> str:
    """A command's shape: ``\\`` becomes ``/``, ``.tmp`` paths are starred, numbers folded.

    Each ``.tmp/`` path after a word's start becomes ``.tmp/*`` (a quoted path keeps its
    closing quote), and each number that stands as a word or follows ``=`` becomes ``#``,
    so two runs differing only in a seed, a count or a rate share one shape.
    """
    return NUMBER_TOKEN.sub("#", TMP_PATTERN.sub(".tmp/*", text.replace("\\", "/")))


def shape_id(repo: str, shape: str) -> str:
    """A shape's id: the first eight hex digits of the SHA-256 of its repo and shape."""
    return hashlib.sha256(f"{repo}\n{shape}".encode()).hexdigest()[:8]


def first_word(text: str) -> str:
    """A command's first whitespace-separated word, or the empty string."""
    words = text.split()
    return words[0] if words else ""


# ---------------------------------------------------------------- journal


def read_file(path: Path) -> list[dict[str, Any]]:
    """The valid event lines of one journal file, through the census's reader.

    A file that cannot be read to its end, rotated away or a damaged archive, keeps the
    lines read before the failure and is said on stderr, as the census's own reader does.
    """
    events: list[dict[str, Any]] = []
    try:
        for text in census._lines(path):
            if not text.strip():
                continue
            event = census._event(text)
            if event is not None:
                events.append(event)
    except (OSError, EOFError, zlib.error) as error:
        print(
            f"local-runs: could not read all of {path.name}: {error}", file=sys.stderr
        )
    return events


class JournalCache:
    """Parsed journal lines per file, parsed again only when a file's size or mtime changed."""

    def __init__(self) -> None:
        self._files: dict[Path, tuple[tuple[int, int], list[dict[str, Any]]]] = {}

    def clear(self) -> None:
        """Forgets every parsed file."""
        self._files.clear()

    def events(self, path: Path) -> list[dict[str, Any]]:
        """A file's valid lines, from the cache unless the file changed."""
        try:
            stat = path.stat()
        except OSError:
            return []
        signature = (stat.st_size, stat.st_mtime_ns)
        found = self._files.get(path)
        if found is not None and found[0] == signature:
            return found[1]
        events = read_file(path)
        self._files[path] = (signature, events)
        return events


CACHE = JournalCache()
NAMER = census.RepoNamer()


def recent(path: Path, cutoff: datetime) -> bool:
    """Whether a file was last modified at or after ``cutoff``."""
    try:
        modified = path.stat().st_mtime
    except OSError:
        return False
    return datetime.fromtimestamp(modified, UTC) >= cutoff


def read_events(
    options: Options, cursor: datetime
) -> tuple[list[dict[str, Any]], datetime | None]:
    """Every valid journal line once, and the latest time read.

    The journal is read through the census's file list; an archive last written before a
    week ahead of the cursor is left out, and a line already seen by the census's run
    identity is skipped, so a rotation between reads cannot double a run.
    """
    events: list[dict[str, Any]] = []
    seen: set[tuple[Any, ...]] = set()
    latest: datetime | None = None
    cutoff = cursor - timedelta(days=ARCHIVE_DAYS)
    for path in journal_paths(options.journal, cutoff):
        for event in CACHE.events(path):
            identity = identity_of(event)
            if identity in seen:
                continue
            seen.add(identity)
            events.append(event)
            latest = later_of(latest, census.parse_time(event.get("time")))
    return events, latest


def journal_paths(folder: Path, cutoff: datetime) -> list[Path]:
    """The journal's files worth reading: every rotated file and the current one, and archives since ``cutoff``."""
    return [
        path
        for path in census.journal_files(folder)
        if path.parent.name != "archive" or recent(path, cutoff)
    ]


def identity_of(event: dict[str, Any]) -> tuple[Any, ...]:
    """The census's run identity of a journal line, so a rotation cannot double a run."""
    return (
        event.get("pid"),
        event.get("processStart"),
        event.get("event"),
        event.get("time"),
    )


def later_of(latest: datetime | None, moment: datetime | None) -> datetime | None:
    """The later of two times, either of which may be missing."""
    if moment is None:
        return latest
    return moment if latest is None or moment > latest else latest


def wall_seconds(event: dict[str, Any]) -> float | None:
    """A release line's wall time as a number, or None when it has none."""
    seconds = event.get("wallSeconds")
    if isinstance(seconds, bool) or not isinstance(seconds, (int, float)):
        return None
    return float(seconds)


def started_at(event: dict[str, Any]) -> datetime | None:
    """When a release started: its ``processStart``, else its ``time`` less its wall time."""
    start = census.parse_time(event.get("processStart"))
    if start is not None:
        return start
    end = census.parse_time(event.get("time"))
    seconds = wall_seconds(event)
    if end is None or seconds is None:
        return end
    return end - timedelta(seconds=seconds)


def counts(event: dict[str, Any], cursor: datetime, min_seconds: float) -> bool:
    """Whether a journal line is a heavy local run after the cursor that took long enough."""
    if event.get("event") != "release" or event.get("kind") != "heavy":
        return False
    if event.get("remote") is True:
        return False
    if not after_cursor(event, cursor):
        return False
    seconds = wall_seconds(event)
    if seconds is None or seconds < min_seconds:
        return False
    command = str(event.get("command") or "")
    if first_word(command).lower() in KEPT_HERE:
        return False
    return SELFTEST_MARKER not in str(event.get("cwd") or "").lower()


def after_cursor(event: dict[str, Any], cursor: datetime) -> bool:
    """Whether a journal line's time is after the cursor."""
    moment = census.parse_time(event.get("time"))
    return moment is not None and moment > cursor


@dataclass
class Run:
    """One counted release line, with the repo, the shape and when it started."""

    repo: str
    shape: str
    at: datetime
    started: datetime
    seconds: float
    event: dict[str, Any] = field(repr=False)


@dataclass
class Group:
    """The counted runs of one repo's shape."""

    id: str
    repo: str
    shape: str
    runs: list[Run]

    @property
    def seconds(self) -> int:
        """The group's summed wall time, rounded to whole seconds."""
        return round(sum(run.seconds for run in self.runs))

    @property
    def latest(self) -> Run:
        """The group's most recent run."""
        return max(self.runs, key=lambda run: run.at)


def counted_runs(
    options: Options, events: list[dict[str, Any]], cursor: datetime
) -> list[Run]:
    """The counted runs of the events read."""
    runs: list[Run] = []
    for event in events:
        if not counts(event, cursor, options.min_seconds):
            continue
        moment = census.parse_time(event.get("time"))
        start = started_at(event)
        seconds = wall_seconds(event)
        if moment is None or start is None or seconds is None:
            continue
        runs.append(
            Run(
                repo=NAMER(str(event.get("cwd") or "")),
                shape=shape_of(command_text(event)),
                at=moment,
                started=start,
                seconds=seconds,
                event=event,
            )
        )
    return runs


def group_runs(runs: list[Run]) -> list[Group]:
    """The runs grouped by shape id, the largest summed time first."""
    groups: dict[str, Group] = {}
    for run in runs:
        key = shape_id(run.repo, run.shape)
        group = groups.get(key)
        if group is None:
            group = Group(id=key, repo=run.repo, shape=run.shape, runs=[])
            groups[key] = group
        group.runs.append(run)
    return sorted(groups.values(), key=lambda group: -group.seconds)


# ---------------------------------------------------------------- state


def load_state(path: Path) -> dict[str, Any]:
    """The state file, or an empty state when it is missing or not readable as JSON."""
    empty: dict[str, Any] = {"v": 1, "cursor": None, "shapes": {}}
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return empty
    except OSError as error:
        raise LocalRunsError(f"cannot read {path}: {error}") from error
    try:
        state = json.loads(text)
    except json.JSONDecodeError:
        return empty
    if not isinstance(state, dict):
        return empty
    if not isinstance(state.get("shapes"), dict):
        state["shapes"] = {}
    state.setdefault("v", 1)
    return state


def save_state(path: Path, state: dict[str, Any]) -> None:
    """Writes the state to a file of its own beside the target and moves it over.

    No reader sees a half file, no two writers share a temporary file, and a failed
    write leaves none behind.
    """
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        handle, name = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".")
        temporary = Path(name)
        try:
            with os.fdopen(handle, "w", newline="\n", encoding="utf-8") as stream:
                json.dump(state, stream, indent=2, sort_keys=True)
                stream.write("\n")
            os.replace(temporary, path)
        except OSError:
            temporary.unlink(missing_ok=True)
            raise
    except OSError as error:
        raise LocalRunsError(f"cannot write {path}: {error}") from error


def cursor_of(state: dict[str, Any], since: datetime | None, now: datetime) -> datetime:
    """The cursor a report reads from: ``--since``, else the state's, else 24 hours before now."""
    if since is not None:
        return since
    cursor = census.parse_time(state.get("cursor"))
    return cursor if cursor is not None else now - timedelta(hours=CURSOR_HOURS)


def entry_for(state: dict[str, Any], key: str) -> dict[str, Any]:
    """A shape's state entry, empty when the state has none."""
    shapes = state.get("shapes")
    entry = shapes.get(key) if isinstance(shapes, dict) else None
    return entry if isinstance(entry, dict) else {}


def entry_pending(entry: dict[str, Any], group: Group) -> bool:
    """Whether a shape is pending: no verdict, flagged, or ``routed`` with a run started after the fix."""
    verdict = entry.get("verdict")
    if verdict is None or entry.get("flagged"):
        return True
    if verdict != "routed":
        return False
    fixed_at = census.parse_time(entry.get("at"))
    if fixed_at is None:
        return True
    return any(run.started > fixed_at for run in group.runs)


def pending_of(groups: list[Group], state: dict[str, Any]) -> list[Group]:
    """The groups still pending."""
    return [
        group for group in groups if entry_pending(entry_for(state, group.id), group)
    ]


def stale_blocks(state: dict[str, Any], groups: list[Group]) -> list[list[str]]:
    """The kept report blocks of shapes with no verdict or flagged, and no run in this report."""
    shown = {group.id for group in groups}
    shapes = state.get("shapes")
    if not isinstance(shapes, dict):
        return []
    blocks: list[list[str]] = []
    for key in sorted(shapes):
        block = kept_block(shapes[key], key in shown)
        if block:
            blocks.append(block)
    return blocks


def kept_block(entry: object, shown: bool) -> list[str]:
    """A state entry's kept report lines, when it has no verdict or is flagged, and no run in this report."""
    if shown or not isinstance(entry, dict):
        return []
    if entry.get("verdict") is not None and not entry.get("flagged"):
        return []
    lines = entry.get("lines")
    if isinstance(lines, list) and lines:
        return [str(line) for line in lines]
    return []


def has_pending(state: dict[str, Any], groups: list[Group]) -> bool:
    """Whether any shape is pending, with a run in this report or only in the state."""
    return bool(pending_of(groups, state)) or bool(stale_blocks(state, groups))


def remember(
    state: dict[str, Any], groups: list[Group], blocks: dict[str, list[str]]
) -> dict[str, Any]:
    """The state with each pending shape's entry, its report lines kept for a later report.

    A pending shape marked ``routed`` is a failed fix, flagged so that it stays pending
    once its runs are behind the cursor.
    """
    shapes = dict(state.get("shapes") or {})
    for group in groups:
        entry = dict(shapes.get(group.id) or {})
        entry["repo"] = group.repo
        entry["shape"] = group.shape
        entry["lines"] = blocks[group.id]
        if entry.get("verdict") == "routed":
            entry["flagged"] = True
        shapes[group.id] = entry
    return {**state, "shapes": shapes}


# ---------------------------------------------------------------- patterns


def repo_lists(value: object) -> dict[str, list[str]]:
    """A ``repos`` table as a map of repo to its patterns."""
    if not isinstance(value, dict):
        return {}
    patterns: dict[str, list[str]] = {}
    for name, values in value.items():
        if isinstance(values, list):
            patterns[str(name)] = [item for item in values if isinstance(item, str)]
    return patterns


def patterns_of(remote: object) -> dict[str, list[str]]:
    """Every pattern a run may route with: each routed ``builders`` entry's, then the top-level ones.

    That is the order ``Config::routes`` feeds the pick's scorer, so a tie keeps the
    first pattern the gate would try.
    """
    if not isinstance(remote, dict):
        return {}
    patterns: dict[str, list[str]] = {}
    builders = remote.get("builders")
    if isinstance(builders, list):
        for entry in builders:
            if isinstance(entry, dict) and entry.get("routed", True) is not False:
                for name, values in repo_lists(entry.get("repos")).items():
                    patterns[name] = [*patterns.get(name, []), *values]
    for name, values in repo_lists(remote.get("repos")).items():
        patterns[name] = [*patterns.get(name, []), *values]
    return patterns


def load_patterns(path: Path) -> dict[str, list[str]]:
    """``remote.json``'s repo patterns as a map of repo to its patterns."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise LocalRunsError(f"cannot read {path}: {error}") from error
    try:
        remote = json.loads(text)
    except json.JSONDecodeError as error:
        raise LocalRunsError(f"cannot read {path}: {error}") from error
    return patterns_of(remote)


def gate_key(repo: str) -> str:
    """The key ``remote.json`` uses for a repo the census names."""
    return GATE_KEYS.get(repo.lower(), repo)


def patterns_for(patterns: dict[str, list[str]], repo: str) -> list[str]:
    """A repo's patterns, matched by its gate key in any letter case, none when it has no entry."""
    key = gate_key(repo).lower()
    for name, values in patterns.items():
        if name.lower() == key:
            return values
    return []


def nearest_pattern(patterns: list[str], shape: str) -> str | None:
    """The pattern of the shape's first word whose word list is closest to the shape's.

    Closeness is ``difflib``'s ratio over the two word lists compared in lower case;
    a tie keeps the first pattern in file order.
    """
    words = shape.split()
    head = words[0].lower() if words else ""
    if not head:
        return None
    lowered = [word.lower() for word in words]
    best: str | None = None
    best_ratio = -1.0
    for pattern in patterns:
        ratio = closeness(pattern, lowered, head)
        if ratio > best_ratio:
            best, best_ratio = pattern, ratio
    return best


def closeness(pattern: str, lowered: list[str], head: str) -> float:
    """How close a pattern's word list is to the shape's, or -1 when its first word differs."""
    other = pattern.split()
    if not other or other[0].lower() != head:
        return -1.0
    return difflib.SequenceMatcher(
        None, [word.lower() for word in other], lowered
    ).ratio()


def differs(nearest: str, shape: str) -> list[str]:
    """The words of the pattern the shape lacks, then the shape's words the pattern lacks."""
    near = nearest.split()
    words = shape.split()
    missing = [f"-{word}" for word in near if word not in words]
    extra = [f"+{word}" for word in words if word not in near]
    return missing + extra


def match_state(event: dict[str, Any], repo: str, ask: Ask) -> str:
    """What the gate says of a run's patterns: ``yes``, ``since`` or ``no``.

    A run the gate gave no words matched nothing it can tell of (``no``). One it says
    matched no pattern is ``since`` when a pattern matches its words now, else ``no``; any
    other run with words matched a pattern at the run (``yes``).
    """
    sent = event.get("sentWords")
    if not sent:
        return "no"
    if event.get("notRouted") != NO_PATTERN:
        return "yes"
    return "since" if ask(gate_key(repo), str(sent)) else "no"


# ---------------------------------------------------------------- facts


def reason_of(event: dict[str, Any]) -> str:
    """A run's ``notRouted`` token, with a ``--local`` run's own reason after it.

    A run that matched no pattern shows no reason: the shape's ``matched=`` already says so.
    """
    token = event.get("notRouted")
    if not isinstance(token, str) or not token or token == NO_PATTERN:
        return "none"
    text = event.get("reason")
    if token == "--local" and isinstance(text, str) and text:
        return f"--local: {text}"
    return token


ROUTABLE_UNDER_PARENT = "routable under parent"

Routable = dict[int, list[tuple[datetime, str]]]


def routable_index(events: list[dict[str, Any]]) -> Routable:
    """The ``covered`` lines naming a routing pattern their command matches, by parent pid.

    Each parent pid maps to the lines' times and patterns, in the order they were read.
    """
    index: Routable = {}
    for event in events:
        if event.get("event") != "covered":
            continue
        pattern = event.get("routable")
        parent = event.get("parentPid")
        moment = census.parse_time(event.get("time"))
        if not isinstance(pattern, str) or not pattern or moment is None:
            continue
        if not isinstance(parent, int) or isinstance(parent, bool):
            continue
        index.setdefault(parent, []).append((moment, pattern))
    return index


def routable_inside(event: dict[str, Any], routable: Routable) -> list[str]:
    """The distinct routing patterns a release's covered gates matched, first seen first.

    A covered line counts when it names the run's pid and falls between the run's start
    (``started_at``) and its release, which guards against pid reuse.
    """
    pid = event.get("pid")
    start = started_at(event)
    end = census.parse_time(event.get("time"))
    if not isinstance(pid, int) or isinstance(pid, bool) or start is None or end is None:
        return []
    patterns: list[str] = []
    for moment, pattern in routable.get(pid, []):
        if start <= moment <= end and pattern not in patterns:
            patterns.append(pattern)
    return patterns


def fell_back(event: dict[str, Any]) -> str:
    """A run's fallback reasons joined by ``; ``; empty when it has none."""
    reasons = event.get("fellBack")
    if not isinstance(reasons, list):
        return ""
    return "; ".join(reason for reason in reasons if isinstance(reason, str))


def routed_counter(events: list[dict[str, Any]]) -> Counter[tuple[str, str]]:
    """The releases that routed, counted per repo and shape family."""
    counter: Counter[tuple[str, str]] = Counter()
    for event in events:
        if event.get("event") != "release" or event.get("remote") is not True:
            continue
        repo = NAMER(str(event.get("cwd") or "")).lower()
        counter[(repo, census.family_of(shape_of(command_text(event))))] += 1
    return counter


def routed_count(counter: Counter[tuple[str, str]], group: Group) -> int:
    """How many of the repo's releases of this shape's family went to a builder."""
    return counter[(group.repo.lower(), census.family_of(group.shape))]


# ---------------------------------------------------------------- report


def block_of(
    group: Group,
    *,
    patterns: dict[str, list[str]],
    counter: Counter[tuple[str, str]],
    routable: Routable,
    ask: Ask,
    state: dict[str, Any],
) -> list[str]:
    """One shape's report block: its line, then its shape, nearest and differs lines.

    When the latest run's covered gates matched routing patterns, a ``routable inside``
    line follows, and a run that says no reason of its own reads ``routable under parent``.
    """
    own = patterns_for(patterns, group.repo)
    latest = group.latest
    inside = routable_inside(latest.event, routable)
    reason = reason_of(latest.event)
    if inside and reason == "none":
        reason = ROUTABLE_UNDER_PARENT
    facts = [
        f"runs={len(group.runs)}",
        f"seconds={group.seconds}",
        f"matched={match_state(latest.event, group.repo, ask)}",
        f"routed={routed_count(counter, group)}",
        f"reason={reason}",
    ]
    fallbacks = fell_back(latest.event)
    if fallbacks:
        facts.append(f"fellBack={fallbacks}")
    if entry_for(state, group.id).get("verdict") == "routed":
        facts.append(STILL_LOCAL)
    lines = [" ".join([group.id, group.repo, *facts]), f"  shape: {group.shape}"]
    nearest = nearest_pattern(own, group.shape)
    if nearest is not None:
        lines.append(f"  nearest: {nearest}")
        changes = differs(nearest, group.shape)
        if changes:
            lines.append(f"  differs: {' '.join(changes)}")
    if inside:
        lines.append(f"  routable inside: {', '.join(inside)}")
    return lines


def report_lines(
    groups: list[Group], blocks: dict[str, list[str]], stale: list[list[str]]
) -> list[str]:
    """The report of the pending shapes: those with a run first, then those only in the state."""
    if not groups and not stale:
        return [NO_PENDING]
    lines = [f"local-runs: {len(groups) + len(stale)} pending"]
    for group in groups:
        lines.extend(blocks[group.id])
    for block in stale:
        lines.extend(block)
    return lines


def report(options: Options, view: View, cursor: datetime) -> list[str]:
    """Prints the pending report, then merges its entries and cursor into the state."""
    state = load_state(options.state)
    pending = pending_of(view.groups, state)
    patterns = load_patterns(options.remote) if pending else {}
    counter = routed_counter(view.events) if pending else Counter()
    routable = routable_index(view.events) if pending else {}
    blocks = {
        group.id: block_of(
            group,
            patterns=patterns,
            counter=counter,
            routable=routable,
            ask=options.ask,
            state=state,
        )
        for group in pending
    }
    stale = stale_blocks(state, pending)
    lines = report_lines(pending, blocks, stale)
    print("\n".join(lines))
    merged = remember(load_state(options.state), pending, blocks)
    merged["cursor"] = stamp(
        max(cursor, view.latest) if view.latest is not None else cursor
    )
    save_state(options.state, merged)
    return lines


# ---------------------------------------------------------------- journal view


@dataclass
class View:
    """What a poll last read from the journal."""

    signature: object = UNREAD
    events: list[dict[str, Any]] = field(default_factory=list)
    latest: datetime | None = None
    groups: list[Group] = field(default_factory=list)


def journal_signature(path: Path) -> object:
    """The journal's size and modification time, or None when the file is not there."""
    try:
        stat = path.stat()
    except OSError:
        return None
    return (stat.st_size, stat.st_mtime_ns)


def refresh(options: Options, cursor: datetime, view: View) -> View:
    """The journal view, read again only when ``events.jsonl`` changed."""
    signature = journal_signature(options.journal / "events.jsonl")
    if signature == view.signature:
        return view
    events, latest = read_events(options, cursor)
    runs = counted_runs(options, events, cursor)
    return View(
        signature=signature, events=events, latest=latest, groups=group_runs(runs)
    )


# ---------------------------------------------------------------- commands


def scan(
    options: Options, since: datetime | None = None, now: datetime | None = None
) -> int:
    """Prints the pending report, saves the state with the advanced cursor, returns 0."""
    cursor = cursor_of(load_state(options.state), since, now or utcnow())
    report(options, refresh(options, cursor, View()), cursor)
    return 0


def watch(
    options: Options,
    since: datetime | None,
    *,
    sleep: Any = time.sleep,
    now: Any = None,
) -> int:
    """Reports once a pending shape has settled, or gives up after ``hours`` with none.

    The journal is read only when ``events.jsonl``'s size or modification time changed;
    the state is read on each poll with no settle running, again when a settle ends, so
    a settle whose shapes were all marked meanwhile wakes nobody and the watch goes on,
    and again before the report's write, so a mark made meanwhile is kept.
    """
    clock = now or utcnow
    start = clock()
    cursor = cursor_of(load_state(options.state), since, start)
    view = View()
    deadline: datetime | None = None
    while True:
        moment = clock()
        view = refresh(options, cursor, view)
        deadline = settle_at(options, moment, deadline, view.groups)
        if deadline is not None and moment >= deadline:
            if has_pending(load_state(options.state), view.groups):
                break
            deadline = None
        if expired(start, moment, deadline, options.hours):
            print(
                f"local-runs: watch ended after {options.hours:g} h with nothing pending"
            )
            return 1
        sleep(options.poll)
    report(options, view, cursor)
    return 0


def settle_at(
    options: Options,
    moment: datetime,
    deadline: datetime | None,
    groups: list[Group],
) -> datetime | None:
    """The settle deadline: the one given, or a new one when a shape is pending."""
    if deadline is not None:
        return deadline
    if not has_pending(load_state(options.state), groups):
        return None
    return moment + timedelta(seconds=options.settle)


def expired(
    start: datetime, moment: datetime, deadline: datetime | None, hours: float
) -> bool:
    """Whether the watch has waited ``hours`` with no shape pending."""
    return deadline is None and moment - start >= timedelta(hours=hours)


def mark(
    keys: list[str],
    verdict: str,
    note: str,
    state: Path,
    now: datetime | None = None,
) -> int:
    """Sets one verdict and note on every id, returns 0, or 2 for an unknown id.

    Every id is checked before any is written, so one unknown id writes nothing and
    is said on its own line.
    """
    saved = load_state(state)
    shapes = saved["shapes"]
    unknown = [key for key in keys if not isinstance(shapes.get(key), dict)]
    if unknown:
        for key in unknown:
            print(f"local-runs: no shape {key}")
        return 2
    at = stamp(now or utcnow())
    for key in keys:
        entry = shapes[key]
        entry["verdict"] = verdict
        entry["note"] = note
        entry["at"] = at
        entry.pop("flagged", None)
    save_state(state, saved)
    for key in keys:
        print(f"local-runs: {key} {verdict}")
    return 0


def iso_time(text: str) -> datetime:
    """An ISO 8601 command-line time as an aware datetime."""
    moment = census.parse_time(text)
    if moment is None:
        raise argparse.ArgumentTypeError(f"not an ISO 8601 time: {text}")
    return moment


def main(argv: list[str] | None = None) -> int:
    """Runs the command line and returns its exit status."""
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(
        prog="watch_local.py",
        description="Report the heavy gate runs that stayed on this PC and need a routing decision.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    for name, help_text in (
        ("scan", "print the pending report once and advance the cursor"),
        ("watch", "poll the journal and report once a pending shape has settled"),
    ):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("--journal", type=Path, default=default_journal())
        command.add_argument("--remote", type=Path, default=default_remote())
        command.add_argument("--state", type=Path, default=default_state())
        command.add_argument("--min-seconds", type=float, default=DEFAULT_MIN_SECONDS)
        command.add_argument("--since", type=iso_time, default=None)
        command.set_defaults(
            settle=DEFAULT_SETTLE, hours=DEFAULT_HOURS, poll=DEFAULT_POLL
        )
        if name == "watch":
            command.add_argument("--settle", type=float, default=DEFAULT_SETTLE)
            command.add_argument("--hours", type=float, default=DEFAULT_HOURS)
            command.add_argument("--poll", type=float, default=DEFAULT_POLL)
    marking = commands.add_parser("mark", help="set one or more shapes' verdict")
    marking.add_argument("id", nargs="+", metavar="id")
    marking.add_argument("--verdict", required=True, choices=VERDICTS)
    marking.add_argument("--note", required=True)
    marking.add_argument("--state", type=Path, default=default_state())
    args = parser.parse_args(argv)
    try:
        if args.command == "mark":
            return mark(args.id, args.verdict, args.note, args.state)
        options = Options(
            journal=args.journal,
            remote=args.remote,
            state=args.state,
            min_seconds=args.min_seconds,
            settle=args.settle,
            poll=args.poll,
            hours=args.hours,
        )
        if args.command == "scan":
            return scan(options, args.since)
        return watch(options, args.since)
    except LocalRunsError as error:
        print(f"local-runs: {error}")
        return 2


if __name__ == "__main__":
    sys.exit(main())
