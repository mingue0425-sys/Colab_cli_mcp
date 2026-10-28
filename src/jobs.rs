//! Background jobs on a runtime: long work (training, downloads, installs)
//! runs as a detached process writing a log, so the kernel stays free.
//! Starting and stopping use the kernel; status and logs are read from files,
//! which works even while the kernel is busy.

use crate::error::{Error, Result};
use serde_json::{Value, json};

pub const ROOT: &str = "content/.colab-cli/jobs";

pub fn new_id(name: Option<&str>) -> Result<String> {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..4];
    let id = match name {
        Some(name) => format!("{name}-{stamp}-{suffix}"),
        None => format!("job-{stamp}-{suffix}"),
    };
    check_id(&id)?;
    Ok(id)
}

pub fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::input(
            "Job names and IDs use letters, digits, '-' or '_'.",
        ));
    }
    Ok(())
}

pub fn dir(id: &str) -> String {
    format!("/{ROOT}/{id}")
}

/// The supervisor that runs a job and records its memory and outcome.
const RUNNER: &str = include_str!("job_runner.py");

/// Python run by the kernel to launch the job detached from it, under the
/// supervisor. Values are embedded as JSON string literals, which are valid
/// Python literals.
/// `after` queues the job behind another one; `autosave` (outputs, Drive
/// folder, account) is kept in the job directory for when it ends.
pub fn start_snippet(
    id: &str,
    source: &str,
    shell: bool,
    after: Option<&str>,
    autosave: Option<&Value>,
) -> Result<String> {
    let (file, kind) = if shell {
        ("main.sh", "shell")
    } else {
        ("main.py", "python")
    };
    let after = after.map(dir);
    Ok(format!(
        r#"import json as _j, os as _o, subprocess as _s
_d = {dir}
_a = {after}
if _a and not _o.path.isdir(_a):
    raise FileNotFoundError("No job " + _o.path.basename(_a) + " on this runtime to wait for.")
_o.makedirs(_d, exist_ok=True)
open(_o.path.join(_d, {file}), "w").write({source})
open(_o.path.join(_d, "runner.py"), "w").write({runner})
if {autosave} is not None:
    open(_o.path.join(_d, "autosave.json"), "w").write({autosave})
_p = _s.Popen(["python", _o.path.join(_d, "runner.py"), _d, {kind}] + ([_a] if _a else []), cwd="/content", start_new_session=True, stdin=_s.DEVNULL, stdout=_s.DEVNULL, stderr=_s.DEVNULL)
open(_o.path.join(_d, "pid"), "w").write(str(_p.pid))
print(_j.dumps({{"job": {id}, "pid": _p.pid}}))"#,
        dir = serde_json::to_string(&dir(id))?,
        file = serde_json::to_string(file)?,
        source = serde_json::to_string(source)?,
        runner = serde_json::to_string(RUNNER)?,
        kind = serde_json::to_string(kind)?,
        id = serde_json::to_string(id)?,
        after = match &after {
            Some(after) => serde_json::to_string(after)?,
            None => "None".to_owned(),
        },
        autosave = match autosave {
            Some(spec) => serde_json::to_string(&spec.to_string())?,
            None => "None".to_owned(),
        },
    ))
}

/// Terminate the job's whole process group, then report what happened.
pub fn stop_snippet(id: &str) -> Result<String> {
    Ok(format!(
        r#"import json as _j, os as _o, signal as _g, time as _t
_d = {dir}
_pid = int(open(_o.path.join(_d, "pid")).read())
def _gone():
    try:
        # Reap the job if this kernel started it (a zombie still "exists").
        if _o.waitpid(_pid, _o.WNOHANG)[0]:
            return True
    except ChildProcessError:
        pass
    try:
        _o.killpg(_pid, 0)
        return False
    except ProcessLookupError:
        return True
_r = "stopped"
if not _gone():
    _o.killpg(_pid, _g.SIGTERM)
    for _ in range(50):
        if _gone():
            break
        _t.sleep(0.1)
    else:
        _o.killpg(_pid, _g.SIGKILL)
        _r = "killed"
_e = _o.path.join(_d, "exit_code")
if not _o.path.exists(_e):
    open(_e, "w").write("-15")
print(_j.dumps({{"job": {id}, "result": _r}}))"#,
        dir = serde_json::to_string(&dir(id))?,
        id = serde_json::to_string(id)?,
    ))
}

/// The kernel prints one JSON line; read it back from the run's text.
pub fn parse_line(result: &Value) -> Result<Value> {
    if result["status"] != "ok" {
        return Err(Error::new(
            "job_failed",
            format!(
                "The job command did not run: {}",
                result["text"].as_str().unwrap_or("").trim()
            ),
            "Check the job ID with `colab job list`.",
        ));
    }
    let text = result["text"].as_str().unwrap_or_default();
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .ok_or_else(|| {
            Error::new(
                "job_failed",
                "The job command returned no result.",
                "Retry.",
            )
        })
}

/// Last `lines` lines of a log tail, starting at a line boundary when cut.
/// Progress bars redraw a line with carriage returns; only the last state counts.
pub fn last_lines(bytes: &[u8], lines: usize, cut: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = if cut {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let all: Vec<&str> = text
        .lines()
        .map(|line| {
            line.trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or("")
        })
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A supervisor silent for this long without recording an end has died with
/// its job (or the runtime restarted). Generous, because a runtime short of
/// memory can stall it.
const LOST_AFTER: f64 = 120.0;

fn mb(stats: &Value, key: &str) -> Value {
    stats[key]
        .as_f64()
        .map_or(Value::Null, |b| json!((b / 1_048_576.0).round() as i64))
}

/// State, memory and the likely cause of an unclean end, from the exit code
/// and the supervisor's stats.json (absent for jobs started by older versions).
pub fn describe(exit_code: Option<i64>, stats: Option<&Value>, now: f64) -> Value {
    let mut out = json!({"exit_code": exit_code});
    let silent = stats
        .and_then(|s| s["updated_at"].as_f64())
        .is_some_and(|t| now - t > LOST_AFTER);
    let waiting = stats.is_some_and(|s| s.get("waiting_for").is_some());
    out["state"] = json!(match exit_code {
        None if silent => "lost",
        None if waiting => "waiting",
        None => "running",
        Some(0) => "succeeded",
        Some(-15) => "stopped",
        Some(137) => "killed",
        Some(_) => "failed",
    });
    let Some(stats) = stats else { return out };
    if waiting {
        out["waiting_for"] = stats["waiting_for"].clone();
        return out;
    }
    let mut memory = json!({
        "total_mb": mb(stats, "total"),
        "peak_job_mb": mb(stats, "peak_rss"),
        "min_available_mb": mb(stats, "min_available"),
    });
    if exit_code.is_none() {
        memory["job_mb"] = mb(stats, "rss");
        memory["available_mb"] = mb(stats, "available");
    } else if stats["max_process_rss"].is_number() {
        memory["largest_process_mb"] = mb(stats, "max_process_rss");
    }
    out["memory"] = memory;
    // Little memory left in the last minute before a SIGKILL or a silent death.
    let total = stats["total"].as_f64().unwrap_or(0.0);
    let low = stats["recent_min_available"]
        .as_f64()
        .is_some_and(|a| total > 0.0 && a < total * 0.1);
    let (cause, next_step) = match out["state"].as_str() {
        Some("killed" | "lost") if low => (
            "out_of_memory",
            "The runtime ran out of memory. Use less at once (fewer parallel processes, smaller batches or buffers, streaming reads), split the work into separate jobs, or use --high-ram.",
        ),
        Some("killed") => (
            "killed",
            "Something sent SIGKILL (not `job stop`); memory looked fine, so check the log for what else could have ended it.",
        ),
        Some("lost") => (
            "lost",
            "The job's supervisor stopped reporting without recording an end; the runtime may have restarted or be overloaded. Check `colab status` and the log.",
        ),
        _ => return out,
    };
    out["likely_cause"] = cause.into();
    out["next_step"] = next_step.into();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_embed_values_as_literals() {
        let snippet = start_snippet("job-1", "print(\"a\\\"b\")\n'''", false, None, None).unwrap();
        assert!(snippet.contains(r#"_d = "/content/.colab-cli/jobs/job-1""#));
        assert!(snippet.contains(r#""print(\"a\\\"b\")\n'''""#));
        assert!(snippet.contains(r#"_d, "python"]"#));
        assert!(snippet.contains("def group_rss"));
        let shell = start_snippet(
            "x",
            "ls",
            true,
            Some("first"),
            Some(&json!({"outputs": ["content/out"]})),
        )
        .unwrap();
        assert!(shell.contains(r#""main.sh""#) && shell.contains(r#"_d, "shell"]"#));
        assert!(check_id("../x").is_err());
        assert!(new_id(Some("train")).unwrap().starts_with("train-"));
    }

    #[test]
    fn start_snippet_is_valid_python() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let check = "import ast, sys; ast.parse(sys.stdin.read())";
        let Ok(mut python) = Command::new("python3")
            .args(["-c", check])
            .stdin(Stdio::piped())
            .spawn()
        else {
            return; // No Python here; CI has it.
        };
        let snippet = start_snippet(
            "j",
            "print('''x''')\n",
            false,
            Some("a"),
            Some(&json!({"folder": "r"})),
        )
        .unwrap();
        python
            .stdin
            .take()
            .unwrap()
            .write_all(snippet.as_bytes())
            .unwrap();
        assert!(python.wait().unwrap().success());
    }

    #[test]
    fn outcomes_and_memory_are_described() {
        const GB: f64 = 1_073_741_824.0;
        let now = 1000.0;
        assert_eq!(describe(None, None, now)["state"], "running");
        assert_eq!(describe(Some(-15), None, now)["state"], "stopped");
        let queued = json!({"waiting_for": "first-1", "updated_at": 995.0});
        let waiting = describe(None, Some(&queued), now);
        assert_eq!(
            (waiting["state"].as_str(), waiting["waiting_for"].as_str()),
            (Some("waiting"), Some("first-1"))
        );
        assert_eq!(describe(None, Some(&queued), now + 300.0)["state"], "lost");
        let running = json!({"updated_at": 990.0, "total": 12.0 * GB, "available": 4.0 * GB,
                             "rss": 6.0 * GB, "peak_rss": 7.0 * GB, "min_available": 3.0 * GB,
                             "recent_min_available": 3.0 * GB});
        let status = describe(None, Some(&running), now);
        assert_eq!(status["state"], "running");
        assert_eq!(status["memory"]["job_mb"], 6144);
        assert!(status.get("likely_cause").is_none());
        // Silent supervisor, low memory just before: lost to out-of-memory.
        let mut starved = running.clone();
        starved["recent_min_available"] = json!(0.5 * GB);
        let lost = describe(None, Some(&starved), now + 200.0);
        assert_eq!(
            (lost["state"].as_str(), lost["likely_cause"].as_str()),
            (Some("lost"), Some("out_of_memory"))
        );
        let killed = describe(Some(137), Some(&starved), now);
        assert_eq!(
            (killed["state"].as_str(), killed["likely_cause"].as_str()),
            (Some("killed"), Some("out_of_memory"))
        );
        let killed = describe(Some(137), Some(&running), now);
        assert_eq!(killed["likely_cause"], "killed");
        let failed = describe(Some(1), Some(&starved), now);
        assert_eq!(failed["state"], "failed");
        assert!(failed.get("likely_cause").is_none());
    }

    #[test]
    fn log_tails_start_on_a_line() {
        assert_eq!(last_lines(b"partial\nb\nc\nd\n", 2, true), "c\nd");
        assert_eq!(last_lines(b"a\nb\n", 5, false), "a\nb");
        assert_eq!(
            last_lines(b"get 10%\rget 50%\rget 100%\nepoch 1\n", 5, false),
            "get 100%\nepoch 1"
        );
        let parsed =
            parse_line(&json!({"status": "ok", "text": "noise\n{\"job\": \"j\", \"pid\": 3}\n"}))
                .unwrap();
        assert_eq!(parsed["pid"], 3);
    }
}
