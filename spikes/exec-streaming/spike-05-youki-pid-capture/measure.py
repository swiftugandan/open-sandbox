#!/usr/bin/env python3
"""
Spike 05 — youki PID-capture race window.

Measures how long after `nsenter` is spawned the in-namespace child
PID becomes readable via /proc/<nsenter_pid>/task/*/children. This
is the polling strategy 12.2's youki backend uses to record the
ExecRegistry entry; if the race window is > 50ms the plan's
"5×10ms backoff" is too tight and needs adjustment.

Runs N trials, reports min / p50 / p95 / p99 / max in microseconds.

Two polling strategies measured:
  - tight: busy-poll /proc with no sleep (lower bound)
  - poll10ms: sleep 10ms between checks (matches the plan)
"""
import glob
import os
import statistics
import subprocess
import sys
import time

TRIALS = 100


def read_children(ns_pid):
    """Return list of child PIDs under /proc/<ns_pid>/task/*/children."""
    pids = []
    for tdir in glob.glob(f"/proc/{ns_pid}/task/*/children"):
        try:
            with open(tdir) as f:
                content = f.read().strip()
                if content:
                    pids.extend(content.split())
        except (FileNotFoundError, PermissionError):
            pass
    return pids


def measure_one(target_pid, strategy):
    proc = subprocess.Popen(
        [
            "nsenter",
            "-t", str(target_pid),
            "--mount", "--uts", "--ipc", "--net", "--pid",
            "--", "sleep", "30",
        ]
    )
    start = time.perf_counter_ns()
    attempts = 0
    while True:
        attempts += 1
        children = read_children(proc.pid)
        if children:
            elapsed_us = (time.perf_counter_ns() - start) / 1000
            proc.kill()
            proc.wait()
            return elapsed_us, attempts, children
        if (time.perf_counter_ns() - start) > 2_000_000_000:  # 2s timeout safeguard
            proc.kill()
            proc.wait()
            return None, attempts, []
        if strategy == "poll10ms":
            time.sleep(0.010)


def run(strategy, target_pid):
    print(f"\n=== Strategy: {strategy} ===", flush=True)
    samples = []
    attempts_list = []
    fails = 0
    for i in range(TRIALS):
        us, attempts, children = measure_one(target_pid, strategy)
        if us is None:
            fails += 1
            print(f"trial={i} TIMEOUT after {attempts} attempts", flush=True)
        else:
            samples.append(us)
            attempts_list.append(attempts)
            if i < 5 or i % 20 == 0:
                print(
                    f"trial={i} child_pid={children[0] if children else '-'} "
                    f"attempts={attempts} elapsed_us={us:.1f}",
                    flush=True,
                )
        time.sleep(0.02)

    if not samples:
        print(f"FAILED: all trials timed out")
        return

    samples.sort()
    n = len(samples)
    print(f"\n--- {strategy} stats over {n}/{TRIALS} successful trials ---")
    print(f"  min     = {samples[0]:.1f} us")
    print(f"  p50     = {samples[n // 2]:.1f} us")
    print(f"  p95     = {samples[int(n * 0.95)]:.1f} us")
    print(f"  p99     = {samples[min(int(n * 0.99), n - 1)]:.1f} us")
    print(f"  max     = {samples[-1]:.1f} us")
    print(f"  mean    = {statistics.mean(samples):.1f} us")
    print(
        f"  attempts: min={min(attempts_list)} "
        f"p50={sorted(attempts_list)[n // 2]} "
        f"max={max(attempts_list)}"
    )
    if fails:
        print(f"  failures: {fails}/{TRIALS}")


def main():
    target_pid = int(sys.argv[1])
    run("tight", target_pid)
    run("poll10ms", target_pid)


if __name__ == "__main__":
    main()
