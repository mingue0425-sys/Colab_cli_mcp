"""Tests for src/job_runner.py, the job supervisor. Run: python3 tests/job_runner_test.py
Memory figures are checked only where /proc exists (Linux, as on Colab)."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

RUNNER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "job_runner.py")
LINUX = os.path.isdir("/proc/self")


def run_job(source, kind="python", after=None):
    job = tempfile.mkdtemp()
    with open(os.path.join(job, "main.py" if kind == "python" else "main.sh"), "w") as f:
        f.write(source)
    env = dict(os.environ, COLAB_CLI_JOB_EVERY="0.2")
    subprocess.run([sys.executable, RUNNER, job, kind] + ([after] if after else []), env=env, check=True, timeout=60)
    read = lambda name: open(os.path.join(job, name)).read()
    return read("exit_code"), json.loads(read("stats.json")), read("log"), read("memory.jsonl")


class Supervisor(unittest.TestCase):
    def test_success_logs_and_memory(self):
        code, stats, log, samples = run_job(
            "import time\nblock = bytearray(200 * 1024 * 1024)\nprint('hello')\ntime.sleep(1)\n"
        )
        self.assertEqual((code, stats["exit_code"], stats["signal"]), ("0", 0, None))
        self.assertEqual(log, "hello\n")
        self.assertGreater(len(samples.splitlines()), 2)
        self.assertGreater(stats["updated_at"], stats["started_at"])
        if LINUX:
            self.assertGreater(stats["peak_rss"], 190 * 1024 * 1024)
            self.assertGreater(stats["max_process_rss"], 190 * 1024 * 1024)
            self.assertGreater(stats["total"], 0)
            self.assertLessEqual(stats["recent_min_available"], stats["total"])

    def test_failure_keeps_exit_code(self):
        code, stats, log, _ = run_job("raise SystemExit(3)")
        self.assertEqual((code, stats["exit_code"]), ("3", 3))

    def test_sigkill_is_recorded_like_the_shell(self):
        code, stats, _, _ = run_job("import os, signal\nos.kill(os.getpid(), signal.SIGKILL)\n")
        self.assertEqual((code, stats["exit_code"], stats["signal"]), ("137", 137, 9))

    def test_shell_job_whose_child_is_killed(self):
        code, stats, log, _ = run_job(f"echo start\n{sys.executable} -c 'import os; os.kill(os.getpid(), 9)'\n", "shell")
        self.assertEqual((code, stats["exit_code"]), ("137", 137))
        self.assertTrue(log.startswith("start\n") and "Killed" in log, log)


    def test_after_waits_for_the_other_job(self):
        first = tempfile.mkdtemp()
        with open(os.path.join(first, "main.py"), "w") as f:
            f.write("import time\ntime.sleep(1)\nraise SystemExit(5)\n")
        env = dict(os.environ, COLAB_CLI_JOB_EVERY="0.2")
        runner = subprocess.Popen([sys.executable, RUNNER, first, "python"], env=env)
        with open(os.path.join(first, "pid"), "w") as f:
            f.write(str(runner.pid))
        source = f"import os\nprint(open(os.path.join({first!r}, 'exit_code')).read())\n"
        code, stats, log, _ = run_job(source, after=first)
        runner.wait()
        # It ran only after the first job ended, although that one failed.
        self.assertEqual((code, log), ("0", "5\n"))
        self.assertNotIn("waiting_for", stats)

    def test_after_a_vanished_job_starts_anyway(self):
        gone = tempfile.mkdtemp()
        with open(os.path.join(gone, "pid"), "w") as f:
            f.write("999999")
        code, _, log, _ = run_job("print('ran')", after=gone)
        self.assertEqual((code, log), ("0", "ran\n"))


if __name__ == "__main__":
    unittest.main()
