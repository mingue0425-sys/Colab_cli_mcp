"""colab-cli job supervisor, run detached on the runtime.

Runs the job (main.py or main.sh in the job directory) in this process group,
records memory every few seconds, and records how the job ended:
  stats.json  latest sample, peaks and the outcome (rewritten atomically;
              its updated_at doubles as a heartbeat)
  memory.jsonl  one sample per line, for the whole run
  exit_code   written last, so its presence means "ended"
Exit codes follow the shell convention: 128 + N when killed by signal N.
"""

import json
import os
import resource
import subprocess
import sys
import time

EVERY = float(os.environ.get("COLAB_CLI_JOB_EVERY", 5))  # seconds between samples
RECENT = 60  # seconds of samples kept for "memory just before the end"


def set_oom_score(value):
    # Lower for the supervisor, so the out-of-memory killer picks the job
    # instead and the outcome still gets recorded; reset for the job.
    try:
        with open("/proc/self/oom_score_adj", "w") as f:
            f.write(str(value))
    except OSError:
        pass


def read_int(path):
    try:
        with open(path) as f:
            text = f.read().strip()
        return None if text == "max" else int(text)
    except (OSError, ValueError):
        return None


def memory():
    """Total and available bytes, honoring a container memory limit."""
    info = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                key, value = line.split(":", 1)
                if key in ("MemTotal", "MemAvailable"):
                    info[key] = int(value.split()[0]) * 1024
    except (OSError, ValueError):
        pass
    total, available = info.get("MemTotal", 0), info.get("MemAvailable", 0)
    for limit_file, usage_file in (
        ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.current"),
        ("/sys/fs/cgroup/memory/memory.limit_in_bytes", "/sys/fs/cgroup/memory/memory.usage_in_bytes"),
    ):
        limit, usage = read_int(limit_file), read_int(usage_file)
        if limit and usage is not None and (not total or limit < total):
            total, available = limit, min(available or limit, max(limit - usage, 0))
            break
    return total, available


def group_rss(group, page=os.sysconf("SC_PAGE_SIZE")):
    """Resident bytes of every process in the job's process group but this one."""
    total = 0
    for pid in os.listdir("/proc") if os.path.isdir("/proc") else ():
        if not pid.isdigit() or int(pid) == group:
            continue
        try:
            with open(f"/proc/{pid}/stat") as f:
                stat = f.read()
            # Fields after "(comm)": state, ppid, pgrp, ...
            if int(stat[stat.rindex(")") + 2 :].split()[2]) != group:
                continue
            with open(f"/proc/{pid}/statm") as f:
                total += int(f.read().split()[1]) * page
        except (OSError, ValueError, IndexError):
            pass
    return total


def write_json(path, value):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(value, f)
    os.replace(tmp, path)


def main(job_dir, kind):
    me = os.getpid()
    set_oom_score(-1000)
    script = os.path.join(job_dir, "main.py" if kind == "python" else "main.sh")
    argv = [sys.executable, "-u", script] if kind == "python" else ["bash", script]
    stats_path = os.path.join(job_dir, "stats.json")
    stats = {"started_at": time.time(), "peak_rss": 0, "min_available": None}
    recent = []
    with open(os.path.join(job_dir, "log"), "ab", buffering=0) as log, open(
        os.path.join(job_dir, "memory.jsonl"), "a"
    ) as samples:
        child = subprocess.Popen(
            argv,
            cwd="/content" if os.path.isdir("/content") else None,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            preexec_fn=lambda: set_oom_score(0),
        )
        while True:
            total, available = memory()
            rss = group_rss(me)
            now = time.time()
            recent = [(t, a) for t, a in recent if now - t <= RECENT] + [(now, available)]
            stats.update(
                updated_at=now,
                total=total,
                available=available,
                rss=rss,
                peak_rss=max(stats["peak_rss"], rss),
                min_available=available
                if stats["min_available"] is None
                else min(stats["min_available"], available),
                recent_min_available=min(a for _, a in recent),
            )
            write_json(stats_path, stats)
            samples.write(json.dumps({"t": round(now, 1), "rss": rss, "available": available}) + "\n")
            samples.flush()
            try:
                code = child.wait(timeout=EVERY)
                break
            except subprocess.TimeoutExpired:
                pass
    signal = -code if code < 0 else None
    stats.update(
        ended_at=time.time(),
        exit_code=128 + signal if signal else code,
        signal=signal,
        max_process_rss=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss * 1024,
    )
    write_json(stats_path, stats)
    with open(os.path.join(job_dir, "exit_code"), "w") as f:
        f.write(str(stats["exit_code"]))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
