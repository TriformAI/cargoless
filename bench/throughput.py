#!/usr/bin/env python3
"""
cargoless throughput-axis bench (AC#7 reframed: CPU/RAM efficiency, not latency).

Operator pivot 2026-05-17: "Speed is good, but throughput is better. I want to
know how we can make it spend less CPU/RAM. This is the major point."

For each tool (cargoless, trunk, bacon):
  1. Spawn it in the bench fixture, capturing its pid + stdout/stderr.
  2. Wait for the "warm" signal (first compile-complete banner per tool).
  3. Sample baseline: timestamp, RSS, utime+stime from /proc/<pid>/stat.
  4. Loop `reps` times: flip the comment-toggle edit (AST-identical;
     keeps tree GREEN so each tool actually does its happy-path work),
     sleep `inter_edit_sec`, sample stats.
  5. Compute and emit:
       * peak RSS (kb)         — worst-case memory footprint
       * RSS growth (kb)       — memory leak indicator over session
       * mean CPU%             — steady-state CPU work
       * CPU-seconds per edit  — total CPU work amortized per save
       * wall-clock per edit   — sanity check
  6. Reap the tool + restore the fixture.

Why direct write, not atomic-rename: cargoless's notify-rs watcher
treats the temp+rename FS-event pair (MOVED_FROM/MOVED_TO) differently
from a direct MODIFY/CLOSE_WRITE. Empirically verified in the 5th
comparative-latency iteration — direct write is the editor-save shape
every watcher handles cleanly. See bench/harness/src/fsutil.rs
docstring for the full explanation.

Std-library-only Python on purpose: same dep-free ethos as the Rust
harness. No pandas, no psutil — `/proc` does everything we need.

Output: one human-readable block per tool + one JSON-ish line per tool
of the shape:
    TPUT_TOOL: name=<n> reps=<r> warm_secs=<s> wall_per_edit_s=<x>
               peak_rss_kb=<R> rss_growth_kb=<G> mean_cpu_pct=<C>
               cpu_seconds_per_edit=<E> samples_rss=[...] samples_cpu_pct=[...]

And one top-line verdict:
    TPUT_VERDICT: cargoless cpu/edit=<X>s rss-peak=<Y>MB ; trunk … ; bacon …
                  ; cargoless-vs-trunk: <WIN|TIE|LOSE>/<cpu_per_edit>,
                    <WIN|TIE|LOSE>/<rss_peak>
                  ; cargoless-vs-bacon: similar
"""

from __future__ import annotations

import argparse
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

CLK_TCK = os.sysconf("SC_CLK_TCK")  # usually 100 on Linux: 1 jiffy = 1/100 s
PAGE_SIZE_KB = os.sysconf("SC_PAGE_SIZE") // 1024  # usually 4

# ---------------------------------------------------------------------
# tool definitions
# ---------------------------------------------------------------------

@dataclass
class Tool:
    name: str
    argv: list[str]
    ready_patterns: list[str]
    # If True, exclude descendant processes from CPU/RSS accounting
    # (we measure only the top-level process). Most one-process tools
    # are fine; cargoless spawns rust-analyzer + proc-macro-srv which
    # are descendants. For a fair "how heavy is this tool" comparison
    # we include the whole process tree — see sample_proc_tree().
    sum_tree: bool = True


def build_tools(cargoless_bin: str) -> list[Tool]:
    return [
        Tool(
            name="cargoless",
            argv=[cargoless_bin, "watch"],
            # Strict ready signals — post-compile banner only (verified
            # via the 4th/5th comparative-latency iteration debugging).
            ready_patterns=[
                "GREEN — tree compiles",
                "GREEN — building",
            ],
            sum_tree=True,
        ),
        Tool(
            name="trunk",
            # trunk watch (NOT serve — we want compile-loop measurement,
            # not HTTP server CPU which would be ambient noise).
            argv=["trunk", "watch"],
            ready_patterns=[
                "success",
                "applying new distribution",
            ],
            sum_tree=True,
        ),
        Tool(
            name="bacon",
            argv=["bacon", "--headless", "--job", "check"],
            ready_patterns=[
                "Success!",
                "Warnings.",  # added after 5th iteration: fixture has 5 warnings
                "Errors found",
            ],
            sum_tree=True,
        ),
    ]


# ---------------------------------------------------------------------
# /proc sampling
# ---------------------------------------------------------------------

@dataclass
class ProcSnap:
    ts_ms: int
    rss_kb: int          # current RSS for the process (or process tree)
    cpu_jiffies: int     # cumulative utime + stime jiffies
    alive: bool


def read_proc_stat(pid: int) -> tuple[int, int] | None:
    """Return (utime_jiffies, stime_jiffies). None if pid is dead.

    /proc/<pid>/stat is space-separated. comm field (position 2) can
    contain spaces inside parens, so we split on ') ' to slice past it.
    """
    try:
        with open(f"/proc/{pid}/stat", "r") as f:
            raw = f.read().strip()
    except FileNotFoundError:
        return None
    except PermissionError:
        return None
    # Find the closing paren of `comm`
    rp = raw.rfind(")")
    if rp < 0:
        return None
    rest = raw[rp + 2 :].split()
    # rest[0] is state; field 14 in 1-based = utime, 15 = stime
    # In our split, that's indices 11 (utime) and 12 (stime).
    try:
        utime = int(rest[11])
        stime = int(rest[12])
    except (IndexError, ValueError):
        return None
    return utime, stime


def read_proc_rss_kb(pid: int) -> int | None:
    """RSS in KB for one pid via /proc/<pid>/statm (page count × page size)."""
    try:
        with open(f"/proc/{pid}/statm", "r") as f:
            parts = f.read().split()
        return int(parts[1]) * PAGE_SIZE_KB
    except (FileNotFoundError, PermissionError, IndexError, ValueError):
        return None


def descendant_pids(root_pid: int) -> list[int]:
    """All descendants of root_pid (BFS via /proc/*/stat ppid)."""
    result = [root_pid]
    children_of: dict[int, list[int]] = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open(f"/proc/{pid}/stat", "r") as f:
                raw = f.read().strip()
        except (FileNotFoundError, PermissionError):
            continue
        rp = raw.rfind(")")
        if rp < 0:
            continue
        rest = raw[rp + 2 :].split()
        try:
            ppid = int(rest[1])
        except (IndexError, ValueError):
            continue
        children_of.setdefault(ppid, []).append(pid)
    frontier = [root_pid]
    while frontier:
        next_frontier: list[int] = []
        for p in frontier:
            kids = children_of.get(p, [])
            result.extend(kids)
            next_frontier.extend(kids)
        frontier = next_frontier
    return result


def sample_proc_tree(root_pid: int) -> ProcSnap:
    """Sample RSS + cumulative CPU jiffies summed across the full tree."""
    ts_ms = time.time_ns() // 1_000_000
    pids = descendant_pids(root_pid)
    total_rss = 0
    total_cpu = 0
    alive = False
    for pid in pids:
        rss = read_proc_rss_kb(pid)
        stat = read_proc_stat(pid)
        if rss is not None:
            total_rss += rss
            alive = True
        if stat is not None:
            total_cpu += stat[0] + stat[1]
    return ProcSnap(ts_ms=ts_ms, rss_kb=total_rss, cpu_jiffies=total_cpu, alive=alive)


# ---------------------------------------------------------------------
# fixture edit driver — direct write to match editor-save FS-event shape
# ---------------------------------------------------------------------

FIXTURE_FILE_REL = "src/domain/model.rs"
ANCHOR = "self.entries.len() /* BENCH_TRAIT_ANCHOR */"
FLIP_A = "self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* tput:a */"
FLIP_B = "self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* tput:b */"


class FixtureEditor:
    """Direct-write edit driver; restore-on-exit so a failed run doesn't
    leave the fixture dirty for the next CI rerun."""

    def __init__(self, fixture_dir: Path):
        self.target = fixture_dir / FIXTURE_FILE_REL
        self.clean = self.target.read_text()
        if ANCHOR not in self.clean:
            raise RuntimeError(f"anchor missing from {self.target} — fixture drifted")

    def flip(self, rep: int) -> None:
        body = self.clean.replace(ANCHOR, FLIP_A if rep % 2 == 0 else FLIP_B, 1)
        # Direct write (open + truncate + write + fsync + close) — see
        # fsutil.rs docstring for why this matches editor save events.
        with open(self.target, "w") as f:
            f.write(body)
            f.flush()
            os.fsync(f.fileno())

    def restore(self) -> None:
        try:
            with open(self.target, "w") as f:
                f.write(self.clean)
                f.flush()
                os.fsync(f.fileno())
        except OSError:
            pass


# ---------------------------------------------------------------------
# tool runner: spawn, warm, sample, kill
# ---------------------------------------------------------------------

@dataclass
class ToolResult:
    name: str
    available: bool
    warm_secs: float = 0.0
    samples: list[ProcSnap] = field(default_factory=list)
    reps_done: int = 0
    target_reps: int = 0
    error: str | None = None

    # Computed properties (after collection)
    def peak_rss_kb(self) -> int:
        return max((s.rss_kb for s in self.samples), default=0)

    def baseline_rss_kb(self) -> int:
        return self.samples[0].rss_kb if self.samples else 0

    def final_rss_kb(self) -> int:
        return self.samples[-1].rss_kb if self.samples else 0

    def rss_growth_kb(self) -> int:
        return self.final_rss_kb() - self.baseline_rss_kb()

    def total_cpu_jiffies(self) -> int:
        if len(self.samples) < 2:
            return 0
        return self.samples[-1].cpu_jiffies - self.samples[0].cpu_jiffies

    def wall_secs(self) -> float:
        if len(self.samples) < 2:
            return 0.0
        return (self.samples[-1].ts_ms - self.samples[0].ts_ms) / 1000.0

    def cpu_seconds(self) -> float:
        return self.total_cpu_jiffies() / CLK_TCK

    def mean_cpu_pct(self) -> float:
        wall = self.wall_secs()
        if wall <= 0:
            return 0.0
        return (self.cpu_seconds() / wall) * 100.0

    def cpu_seconds_per_edit(self) -> float:
        if self.reps_done <= 0:
            return 0.0
        return self.cpu_seconds() / self.reps_done

    def wall_secs_per_edit(self) -> float:
        if self.reps_done <= 0:
            return 0.0
        return self.wall_secs() / self.reps_done


def wait_for_ready(log_path: Path, patterns: list[str], deadline_s: float) -> float:
    """Tail the log file until ANY pattern matches OR deadline. Returns
    elapsed seconds (≥ deadline_s means timeout)."""
    start = time.monotonic()
    matchers = [re.escape(p) for p in patterns]
    pattern_re = re.compile("|".join(matchers))
    last_size = 0
    while time.monotonic() - start < deadline_s:
        try:
            with open(log_path, "r", errors="replace") as f:
                f.seek(last_size)
                chunk = f.read()
                if chunk and pattern_re.search(chunk):
                    return time.monotonic() - start
                last_size = f.tell()
        except FileNotFoundError:
            pass
        time.sleep(0.5)
    return deadline_s + 0.001  # timed out


def run_tool(
    tool: Tool,
    fixture: Path,
    cfg: argparse.Namespace,
    log_dir: Path,
) -> ToolResult:
    res = ToolResult(name=tool.name, target_reps=cfg.reps, available=False)

    # Availability check
    try:
        v = subprocess.run([tool.argv[0], "--version"], capture_output=True, timeout=10)
        if v.returncode != 0 and not v.stdout and not v.stderr:
            res.error = f"--version exited {v.returncode}; not on PATH?"
            return res
    except (FileNotFoundError, subprocess.TimeoutExpired) as e:
        res.error = f"availability probe failed: {e}"
        return res
    res.available = True

    log_path = log_dir / f"{tool.name}.log"
    log_path.write_text("")  # truncate
    print(f"  [{tool.name}] spawn argv={tool.argv}", flush=True)
    with open(log_path, "ab") as log_f:
        # New session so we can reap the whole tree cleanly.
        proc = subprocess.Popen(
            tool.argv,
            cwd=str(fixture),
            stdin=subprocess.DEVNULL,
            stdout=log_f,
            stderr=subprocess.STDOUT,
            env={
                **os.environ,
                "NO_COLOR": "1",
                "CLICOLOR": "0",
                "CLICOLOR_FORCE": "0",
                "RUST_LOG_STYLE": "never",
            },
            start_new_session=True,
        )

    print(f"  [{tool.name}] waiting up to {cfg.warm_timeout_sec}s for ready...", flush=True)
    res.warm_secs = wait_for_ready(log_path, tool.ready_patterns, cfg.warm_timeout_sec)
    if res.warm_secs > cfg.warm_timeout_sec:
        res.error = f"NO_READY after {cfg.warm_timeout_sec}s (tail follows)"
        try:
            res.error += "\n" + "\n".join(log_path.read_text().splitlines()[-20:])
        except OSError:
            pass
        _reap(proc)
        return res
    print(f"  [{tool.name}] warm at {res.warm_secs:.1f}s", flush=True)

    # Settle a beat after warm to let any debounce / init noise quiet
    time.sleep(cfg.settle_sec)

    editor = FixtureEditor(fixture)

    # Baseline sample (rep 0, not counted as a measured edit)
    res.samples.append(sample_proc_tree(proc.pid))
    print(
        f"  [{tool.name}] baseline rss={res.samples[-1].rss_kb}kb cpu_j={res.samples[-1].cpu_jiffies}",
        flush=True,
    )

    try:
        for rep in range(1, cfg.reps + 1):
            editor.flip(rep)
            time.sleep(cfg.inter_edit_sec)
            snap = sample_proc_tree(proc.pid)
            if not snap.alive:
                res.error = f"tool process tree died at rep {rep}"
                break
            res.samples.append(snap)
            res.reps_done = rep
            if rep % 5 == 0 or rep == cfg.reps:
                print(
                    f"  [{tool.name}] rep {rep}/{cfg.reps} "
                    f"rss={snap.rss_kb}kb cpu_j={snap.cpu_jiffies}",
                    flush=True,
                )
    finally:
        editor.restore()
        _reap(proc)

    return res


def _reap(proc: subprocess.Popen) -> None:
    """Kill the entire process group of the spawned tool, wait, ignore errors."""
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass


# ---------------------------------------------------------------------
# reporting
# ---------------------------------------------------------------------

def render_block(r: ToolResult) -> str:
    if not r.available:
        return f"  [{r.name}] UNAVAILABLE  error={r.error or '-'}"
    if r.error:
        head = (
            f"  [{r.name}] PARTIAL/ERROR  error={r.error}\n"
            f"      warm_secs={r.warm_secs:.2f}  samples={len(r.samples)}/{r.target_reps}"
        )
        return head
    return (
        f"  [{r.name}] warm_secs={r.warm_secs:.2f}\n"
        f"      reps={r.reps_done}/{r.target_reps}\n"
        f"      baseline_rss_kb={r.baseline_rss_kb()}  "
        f"final_rss_kb={r.final_rss_kb()}  peak_rss_kb={r.peak_rss_kb()}  "
        f"rss_growth_kb={r.rss_growth_kb()}\n"
        f"      total_cpu_seconds={r.cpu_seconds():.2f}  "
        f"wall_secs={r.wall_secs():.2f}  mean_cpu_pct={r.mean_cpu_pct():.1f}\n"
        f"      cpu_seconds_per_edit={r.cpu_seconds_per_edit():.3f}  "
        f"wall_secs_per_edit={r.wall_secs_per_edit():.2f}"
    )


def render_tput_line(r: ToolResult) -> str:
    if not r.available or r.error:
        return (
            f"TPUT_TOOL: name={r.name} status=UNAVAILABLE error={(r.error or '-')[:80]}"
        )
    return (
        f"TPUT_TOOL: name={r.name} reps={r.reps_done} warm_secs={r.warm_secs:.2f} "
        f"wall_per_edit_s={r.wall_secs_per_edit():.2f} "
        f"peak_rss_kb={r.peak_rss_kb()} rss_growth_kb={r.rss_growth_kb()} "
        f"mean_cpu_pct={r.mean_cpu_pct():.2f} "
        f"cpu_seconds_per_edit={r.cpu_seconds_per_edit():.3f}"
    )


def render_verdict(results: list[ToolResult]) -> str:
    def get(name: str) -> ToolResult | None:
        for r in results:
            if r.name == name:
                return r
        return None

    def fmt(r: ToolResult | None) -> str:
        if r is None or not r.available or r.error:
            return "N/A"
        return (
            f"{r.name}=cpu/e={r.cpu_seconds_per_edit():.3f}s "
            f"rss-peak={r.peak_rss_kb()/1024:.0f}MB"
        )

    cargoless = get("cargoless")
    trunk = get("trunk")
    bacon = get("bacon")

    def compare(ours: ToolResult | None, theirs: ToolResult | None, dim: str) -> str:
        if (
            ours is None
            or theirs is None
            or not ours.available
            or not theirs.available
            or ours.error
            or theirs.error
        ):
            return f"N/A/{dim}"
        a = ours.cpu_seconds_per_edit() if dim.startswith("cpu") else ours.peak_rss_kb()
        b = theirs.cpu_seconds_per_edit() if dim.startswith("cpu") else theirs.peak_rss_kb()
        if a < b:
            return f"WIN/{dim}"
        if a == b:
            return f"TIE/{dim}"
        return f"LOSE/{dim}"

    line = "TPUT_VERDICT: " + " ; ".join([fmt(cargoless), fmt(trunk), fmt(bacon)])
    if cargoless and trunk and cargoless.available and trunk.available:
        line += (
            f" ; vs-trunk: {compare(cargoless, trunk, 'cpu_per_edit')} "
            f"{compare(cargoless, trunk, 'rss_peak')}"
        )
    if cargoless and bacon and cargoless.available and bacon.available:
        line += (
            f" ; vs-bacon: {compare(cargoless, bacon, 'cpu_per_edit')} "
            f"{compare(cargoless, bacon, 'rss_peak')}"
        )
    return line


# ---------------------------------------------------------------------
# main
# ---------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="cargoless throughput-axis bench")
    parser.add_argument("--fixture", default="bench/fixture")
    parser.add_argument(
        "--cargoless-bin",
        default=os.environ.get(
            "CARGOLESS_BIN",
            str(Path("target/release/tftrunk")),
        ),
    )
    parser.add_argument("--reps", type=int, default=int(os.environ.get("REPS", 60)))
    parser.add_argument(
        "--inter-edit-sec",
        type=float,
        default=float(os.environ.get("INTER_EDIT_SEC", 10.0)),
    )
    parser.add_argument(
        "--warm-timeout-sec",
        type=float,
        default=float(os.environ.get("WARM_TIMEOUT_SEC", 600)),
    )
    parser.add_argument("--settle-sec", type=float, default=2.0)
    parser.add_argument(
        "--tool",
        choices=["cargoless", "trunk", "bacon", "all"],
        default="all",
    )
    parser.add_argument("--log-dir", default="/tmp/cargoless-throughput-logs")
    args = parser.parse_args()

    fixture = Path(args.fixture).resolve()
    if not fixture.exists():
        print(f"ERROR: fixture {fixture} not found", file=sys.stderr)
        return 2

    log_dir = Path(args.log_dir)
    log_dir.mkdir(parents=True, exist_ok=True)

    tools = build_tools(args.cargoless_bin)
    if args.tool != "all":
        tools = [t for t in tools if t.name == args.tool]

    print("=== cargoless throughput bench (AC#7 reframed: CPU/RAM) ===")
    print(f"fixture:       {fixture}")
    print(f"cargoless bin: {args.cargoless_bin}")
    print(
        f"config: reps={args.reps} inter_edit_sec={args.inter_edit_sec} "
        f"warm_timeout_sec={args.warm_timeout_sec} settle_sec={args.settle_sec}"
    )
    print(f"clock_tck={CLK_TCK} page_size_kb={PAGE_SIZE_KB}")
    print()

    results: list[ToolResult] = []
    for tool in tools:
        print(f"---- tool: {tool.name} ----")
        res = run_tool(tool, fixture, args, log_dir)
        results.append(res)
        print(render_block(res))
        print()

    print("==== throughput verdict markers ====")
    for r in results:
        print(render_tput_line(r))
    print()
    print(render_verdict(results))
    return 0


if __name__ == "__main__":
    sys.exit(main())
