#!/usr/bin/env python3
"""Cold start of the kwikvid app against the Roadmap I target "cold start
< 1.5 s" (docs/roadmap.md, issue #148).

Usage: cold_start.py <kwikvid binary> [runs]

Launches `<binary> --smoke` `runs` times (default 5) and records the wall
time from spawn to exit of each run. `--smoke` exits once the first
`kv://state` event has been sent, which happens when the page has loaded:
the closest existing signal to "usable window". The first run is the first
launch after the build (cold disk caches); the median is over all runs.

Prints, on stdout:

- one line per run;
- `TARGET cold-start-first <ms> 1500` and `TARGET cold-start-median <ms> 1500`
  (the format of AGENTS.md, "Full regression suite");
- the `startup: <phase> done at <ms> ms` lines the app logs (kv-app
  `startup_phase`) for the first run and the median run, so a slow phase is
  visible without a profiler.

Exits non-zero when a run fails (exit code other than 0 or a timeout).
"""

import os
import re
import subprocess
import sys
import time

TARGET_MS = 1500
TIMEOUT_S = 120
PHASE = re.compile(r"startup: .*")


def run_once(binary):
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "info")
    started = time.perf_counter()
    proc = subprocess.run(
        [binary, "--smoke"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
        timeout=TIMEOUT_S,
        env=env,
    )
    took_ms = (time.perf_counter() - started) * 1e3
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-4000:])
        raise SystemExit(f"{binary} --smoke exited with code {proc.returncode}")
    # Strip ANSI colours from the tracing output.
    log = re.sub(r"\x1b\[[0-9;]*m", "", proc.stderr)
    phases = [m.group(0) for m in PHASE.finditer(log)]
    return took_ms, phases


def main():
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    binary = sys.argv[1]
    runs = int(sys.argv[2]) if len(sys.argv) == 3 else 5
    results = []
    for i in range(runs):
        took_ms, phases = run_once(binary)
        print(f"cold-start run {i + 1}: {took_ms:.1f} ms", flush=True)
        results.append((took_ms, phases))
    times = sorted(r[0] for r in results)
    first = results[0][0]
    median = times[len(times) // 2]
    print(
        f"cold-start over {runs} runs ({os.path.basename(binary)} --smoke)\n"
        f"                        time:   [{times[0]:.1f} ms {median:.1f} ms {times[-1]:.1f} ms]"
        f"  first {first:.1f} ms, target {TARGET_MS} ms"
    )
    print(f"TARGET cold-start-first {first:.1f} {TARGET_MS}")
    print(f"TARGET cold-start-median {median:.1f} {TARGET_MS}")
    median_run = next(i for i, r in enumerate(results) if r[0] == median)
    for label, index in (("first run", 0), (f"median run ({median_run + 1})", median_run)):
        print(f"startup phases, {label}:")
        for line in results[index][1] or ["(no startup: lines logged)"]:
            print(f"  {line}")


if __name__ == "__main__":
    main()
