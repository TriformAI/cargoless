#!/usr/bin/env python3
# bench/m3-roundtrip.py — M3 overlay-push round-trip latency, split
# transport-vs-RA, sub-ms client-monotonic timing.
#
# WHAT THIS MEASURES
#   For each rep against a live `cargoless serve --repo --bind <addr>`:
#
#     t0  = monotonic ns just before POST /overlay
#     t1  = monotonic ns when the 200 ack is received
#     t2  = monotonic ns when the *next* SSE /events frame for this
#           worktree arrives (i.e. the verdict the push produced)
#
#     transport_ms = (t1 - t0) / 1e6   ← HTTP round-trip + overlay store
#     ra_ms        = (t2 - t1) / 1e6   ← serve-loop drain + RA flycheck
#     total_ms     = (t2 - t0) / 1e6
#
# SSE was chosen over /status polling because WorktreeStatus.published_at
# is unix-seconds (1 s granularity — too coarse for sub-second RA work),
# while client-side monotonic_ns() stamping at the moment of SSE frame
# receipt gives sub-ms precision against the same wall clock as t0/t1.
#
# HONEST METHODOLOGY CAVEATS (the report MUST carry these verbatim):
#   1. Per-push serial — pushes are sequential across reps. The fleet
#      effect at N=20 is the *daemon's per-WT state cardinality*, NOT
#      concurrent-pusher contention. A real agent-fleet with 20 pushers
#      hitting in parallel would expose serialization differently.
#   2. SSE frame receipt ≠ "verdict computed-and-persisted" — it is the
#      moment the daemon EMITS the frame, which is the publish_verdict
#      seam (Judgment B). Close enough for round-trip; not pure-unit.
#   3. Synthetic worktrees on bench/fixture — Leptos honest-size, same
#      substrate AC7 §8.5 / #196 / #259 measured. Larger workspaces
#      shift absolutes; the *split* between transport and RA is the
#      diagnostic, not the absolute numbers.
#
# INVOCATION (called by bench/m3-fleet.sh from the cargoless-builder pod):
#   m3-roundtrip.py --base-url http://127.0.0.1:PORT --worktrees WT1,WT2,...
#                   --fixture-path /tmp/m3fleet/wt1
#                   --reps 50 [--token TOK] [--inter-rep-gap 1.0]

import argparse
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from queue import Empty, Queue
from pathlib import Path


# ── HTTP primitives (std-only; matches the daemon's "no heavy deps" ethos) ──
def _request(url: str, token: str | None, *, data=None, method="GET", timeout=30.0):
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    return urllib.request.urlopen(req, timeout=timeout)


def post_overlay(base_url, token, worktree, base_ref, files):
    """POST /overlay; return (ack_dict, t0_ns, t1_ns).
    t0 = monotonic just before send; t1 = monotonic at 200 receipt.

    Wire shape is the daemon's Request::from_json contract
    (crates/cargoless-core/src/transport/mod.rs §363+): the discriminant
    key is `op` (NOT `type`), and files are objects with `path` + `content`
    fields (NOT tuple-pairs). Mis-shaped requests answer HTTP 400 silently
    — first M3 launch caught this loudly via the warm-up FAIL."""
    files_wire = [{"path": p, "content": c} for (p, c) in files]
    body = json.dumps(
        {"op": "push_overlay", "worktree": worktree, "base_ref": base_ref, "files": files_wire}
    ).encode()
    t0 = time.monotonic_ns()
    with _request(f"{base_url}/overlay", token, data=body, method="POST") as resp:
        ack = json.loads(resp.read())
    t1 = time.monotonic_ns()
    return ack, t0, t1


def get_status(base_url, token, worktree):
    url = f"{base_url}/status?worktree={urllib.parse.quote(worktree, safe='')}"
    try:
        with _request(url, token, timeout=5.0) as resp:
            return json.loads(resp.read())
    except (urllib.error.HTTPError, urllib.error.URLError, json.JSONDecodeError):
        return None


# ── SSE listener thread — sub-ms client-monotonic stamps on each frame ──
class SseListener(threading.Thread):
    """Reads GET /events as a chunked text/event-stream, stamps each
    `data: <json>\\n\\n` frame with time.monotonic_ns() at receipt, and
    pushes (recv_ns, event_dict) onto a queue per worktree.

    The daemon's HTTP server writes one `data:` line per transition + a
    blank line terminator. We track partial lines defensively but
    tolerate the simple line-at-a-time shape the daemon produces."""

    def __init__(self, base_url, token):
        super().__init__(daemon=True)
        self.base_url = base_url
        self.token = token
        self.queues: dict[str, Queue] = {}
        self.queues_lock = threading.Lock()
        self.started = threading.Event()
        self.stop_flag = threading.Event()
        self._resp = None

    def subscribe(self, worktree):
        with self.queues_lock:
            q = self.queues.get(worktree)
            if q is None:
                q = Queue()
                self.queues[worktree] = q
            return q

    def stop(self):
        self.stop_flag.set()
        try:
            if self._resp is not None:
                self._resp.close()
        except Exception:
            pass

    def run(self):
        url = f"{self.base_url}/events"
        try:
            self._resp = _request(url, self.token, timeout=None)
        except Exception as e:
            print(f"[m3-sse] FAIL: could not open /events: {e}", file=sys.stderr)
            self.started.set()
            return
        self.started.set()
        try:
            while not self.stop_flag.is_set():
                raw = self._resp.readline()
                if not raw:
                    break
                recv_ns = time.monotonic_ns()
                line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                if not line.startswith("data:"):
                    continue
                payload = line[len("data:") :].lstrip()
                try:
                    ev = json.loads(payload)
                except json.JSONDecodeError:
                    continue
                wt = ev.get("worktree")
                if not wt:
                    continue
                with self.queues_lock:
                    q = self.queues.get(wt)
                if q is not None:
                    q.put((recv_ns, ev))
        except Exception as e:
            if not self.stop_flag.is_set():
                print(f"[m3-sse] read loop ended: {e}", file=sys.stderr)


def percentile(xs, p):
    """Inclusive nearest-rank percentile on a sorted-ascending list."""
    if not xs:
        return float("nan")
    n = len(xs)
    idx = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
    return sorted(xs)[idx]


def summarize(label, xs):
    if not xs:
        print(f"  {label}: n=0 (no samples)")
        return
    p50 = percentile(xs, 50)
    p95 = percentile(xs, 95)
    p99 = percentile(xs, 99)
    print(
        f"  {label}: n={len(xs)} "
        f"p50={p50:.1f}ms p95={p95:.1f}ms p99={p99:.1f}ms "
        f"min={min(xs):.1f}ms max={max(xs):.1f}ms median={statistics.median(xs):.1f}ms"
    )


# ── edit driver ────────────────────────────────────────────────────────
# Matches bench/run.sh + bench/modelr-fleet.sh + bench/m2-cpu-approx.sh.
TRAIT_REL = "src/domain/model.rs"
TRAIT_FIND = "self.entries.len() /* BENCH_TRAIT_ANCHOR */"
TRAIT_REPL = "self.entries.len_oops() /* BENCH_TRAIT_ANCHOR */"


def read_anchor_file(repo_root: Path) -> str:
    p = repo_root / TRAIT_REL
    return p.read_text()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", required=True, help="e.g. http://127.0.0.1:8080")
    ap.add_argument(
        "--worktrees",
        required=True,
        help="comma-separated absolute paths (the daemon's WT keys, one per rep)",
    )
    ap.add_argument(
        "--fixture-path",
        required=True,
        help="path to ONE worktree to read the original anchor file content from "
        "(all worktrees share the same bench/fixture content)",
    )
    ap.add_argument("--reps", type=int, default=50)
    ap.add_argument("--token", default="")
    ap.add_argument(
        "--inter-rep-gap",
        type=float,
        default=1.0,
        help="seconds between reps (let RA settle)",
    )
    ap.add_argument(
        "--verdict-timeout",
        type=float,
        default=60.0,
        help="seconds to wait for the SSE verdict frame after a push",
    )
    ap.add_argument(
        "--warmup-timeout",
        type=float,
        default=300.0,
        help="seconds to wait for initial green verdict on the first worktree",
    )
    ap.add_argument(
        "--scale-label",
        default="N=?",
        help="text label for the result block (e.g. 'N=1' or 'N=20')",
    )
    args = ap.parse_args()

    base_url = args.base_url.rstrip("/")
    token = args.token or None
    worktrees: list[str] = [w for w in args.worktrees.split(",") if w]
    if not worktrees:
        print("FAIL: --worktrees produced an empty list", file=sys.stderr)
        sys.exit(2)
    fixture_path = Path(args.fixture_path)
    if not (fixture_path / TRAIT_REL).is_file():
        print(
            f"FAIL: anchor file {fixture_path / TRAIT_REL} not found", file=sys.stderr
        )
        sys.exit(2)

    orig_content = read_anchor_file(fixture_path)
    if TRAIT_FIND not in orig_content:
        print(f"FAIL: anchor token '{TRAIT_FIND}' not in {fixture_path / TRAIT_REL}", file=sys.stderr)
        sys.exit(2)
    red_content = orig_content.replace(TRAIT_FIND, TRAIT_REPL)
    assert red_content != orig_content

    print(f"[m3] scale={args.scale_label} base={base_url} reps={args.reps} worktrees={len(worktrees)}")

    # Start SSE listener BEFORE any push so we don't race against the
    # first transition event.
    sse = SseListener(base_url, token)
    sse.start()
    if not sse.started.wait(timeout=10.0):
        print("FAIL: SSE listener did not start within 10s", file=sys.stderr)
        sys.exit(2)
    # Subscribe to all worktrees we'll touch.
    queues = {wt: sse.subscribe(wt) for wt in worktrees}

    # Warm-up: prime each worktree to a known GREEN state by pushing an
    # empty overlay (server overlay clears ⇒ tree == base ⇒ green once
    # cold cargo check completes). Wait for the green SSE event per WT.
    # The FIRST worktree on a cold Leptos cluster pays the cold-cargo-
    # check cost (minutes); subsequent worktrees in the same cluster
    # share the warm RA and are fast.
    print("[m3] warm-up: pushing empty overlay per worktree + waiting for GREEN")
    for i, wt in enumerate(worktrees):
        t_warm0 = time.monotonic()
        try:
            ack, _, _ = post_overlay(base_url, token, wt, "HEAD", [])
        except Exception as e:
            print(f"FAIL: warm-up push {wt}: {e}", file=sys.stderr)
            try: sse.stop()
            except Exception: pass
            # os._exit bypasses cleanup — the SSE listener thread's blocked
            # socket.readline() doesn't always release on sse._resp.close(),
            # which left the prior FAIL run hung. Force-exit is the right
            # discipline here: the FAIL message has been flushed already.
            os._exit(2)
        if not ack.get("accepted"):
            print(f"FAIL: warm-up push {wt} rejected: {ack}", file=sys.stderr)
            try: sse.stop()
            except Exception: pass
            os._exit(2)
        # Drain SSE for this WT until green (or timeout)
        timeout = args.warmup_timeout if i == 0 else args.verdict_timeout
        deadline = time.monotonic() + timeout
        got_green = False
        while time.monotonic() < deadline:
            try:
                recv_ns, ev = queues[wt].get(timeout=0.5)
            except Empty:
                continue
            if ev.get("verdict") == "green":
                got_green = True
                print(
                    f"  warmup {wt}: green in {time.monotonic() - t_warm0:.1f}s"
                )
                break
        if not got_green:
            print(f"FAIL: worktree {wt} did not reach GREEN within {timeout}s", file=sys.stderr)
            try: sse.stop()
            except Exception: pass
            os._exit(2)

    # Drain any residual events (the warmup pushes may have produced
    # transient `red`→`green` sequences for each WT). After this point
    # the queues are empty and we're ready to measure.
    for q in queues.values():
        while not q.empty():
            try:
                q.get_nowait()
            except Empty:
                break

    # Measurement loop. Round-robin worktrees so an N=20 scale exercises
    # all 20 WTs across the 50 reps (~2-3 hits each).
    transport_ms: list[float] = []
    ra_ms: list[float] = []
    total_ms: list[float] = []
    failures = 0
    for rep in range(1, args.reps + 1):
        wt = worktrees[(rep - 1) % len(worktrees)]
        # Push the RED overlay (modified anchor file).
        files = [(TRAIT_REL, red_content)]
        try:
            ack, t0, t1 = post_overlay(base_url, token, wt, "HEAD", files)
        except Exception as e:
            print(f"  rep {rep} {wt}: push failed: {e}")
            failures += 1
            continue
        if not ack.get("accepted"):
            print(f"  rep {rep} {wt}: ack rejected: {ack}")
            failures += 1
            continue
        # Wait for the next RED SSE event for this WT.
        deadline = time.monotonic() + args.verdict_timeout
        t2 = None
        while time.monotonic() < deadline:
            try:
                recv_ns, ev = queues[wt].get(timeout=0.25)
            except Empty:
                continue
            v = ev.get("verdict")
            if v == "red":
                t2 = recv_ns
                break
            # If a stale green slips through, ignore and keep waiting.
        if t2 is None:
            print(f"  rep {rep} {wt}: verdict-RED never observed within {args.verdict_timeout}s")
            failures += 1
        else:
            t_ms = (t1 - t0) / 1e6
            r_ms = (t2 - t1) / 1e6
            tot_ms = (t2 - t0) / 1e6
            transport_ms.append(t_ms)
            ra_ms.append(r_ms)
            total_ms.append(tot_ms)
            print(
                f"  rep {rep:3d} {wt[-32:]}: transport={t_ms:6.1f}ms ra={r_ms:7.1f}ms total={tot_ms:7.1f}ms"
            )
        # Revert: push empty overlay → daemon clears WT overlay → verdict back to green.
        try:
            post_overlay(base_url, token, wt, "HEAD", [])
        except Exception as e:
            print(f"  rep {rep} {wt}: revert-push failed: {e}")
        # Wait for green back (or timeout) to keep state clean.
        deadline = time.monotonic() + args.verdict_timeout
        while time.monotonic() < deadline:
            try:
                _, ev = queues[wt].get(timeout=0.25)
            except Empty:
                continue
            if ev.get("verdict") == "green":
                break
        time.sleep(args.inter_rep_gap)

    sse.stop()
    sse.join(timeout=5.0)

    print()
    print(f"============================================================")
    print(f"M3 ROUND-TRIP LATENCY SUMMARY  scale={args.scale_label}")
    print(f"  reps measured = {len(total_ms)} / {args.reps}   (failures = {failures})")
    summarize("transport (ack-send)", transport_ms)
    summarize("RA flycheck (verdict-ack)", ra_ms)
    summarize("total (verdict-send)", total_ms)
    print(f"============================================================")
    print(f"DONE_SENTINEL scale={args.scale_label}")
    sys.exit(0 if failures == 0 else 1)


if __name__ == "__main__":
    main()
