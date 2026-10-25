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

/// Python run by the kernel to launch the job detached from it. Values are
/// embedded as JSON string literals, which are valid Python literals.
pub fn start_snippet(id: &str, source: &str, shell: bool) -> Result<String> {
    let dir = dir(id);
    let (file, command) = if shell {
        ("main.sh", format!("cd /content && bash {dir}/main.sh"))
    } else {
        ("main.py", format!("cd /content && python -u {dir}/main.py"))
    };
    let wrapped = format!("{command} > {dir}/log 2>&1; echo $? > {dir}/exit_code");
    Ok(format!(
        r#"import json as _j, os as _o, subprocess as _s
_d = {dir}
_o.makedirs(_d, exist_ok=True)
open(_o.path.join(_d, {file}), "w").write({source})
_p = _s.Popen(["bash", "-c", {wrapped}], start_new_session=True, stdin=_s.DEVNULL, stdout=_s.DEVNULL, stderr=_s.DEVNULL)
open(_o.path.join(_d, "pid"), "w").write(str(_p.pid))
print(_j.dumps({{"job": {id}, "pid": _p.pid}}))"#,
        dir = serde_json::to_string(&dir)?,
        file = serde_json::to_string(file)?,
        source = serde_json::to_string(source)?,
        wrapped = serde_json::to_string(&wrapped)?,
        id = serde_json::to_string(id)?,
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
pub fn last_lines(bytes: &[u8], lines: usize, cut: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = if cut {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn state(exit_code: Option<i64>) -> Value {
    match exit_code {
        None => json!("running"),
        Some(0) => json!("succeeded"),
        Some(-15) => json!("stopped"),
        Some(_) => json!("failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_embed_values_as_literals() {
        let snippet = start_snippet("job-1", "print(\"a\\\"b\")\n'''", false).unwrap();
        assert!(snippet.contains(r#"_d = "/content/.colab-cli/jobs/job-1""#));
        assert!(snippet.contains(r#""print(\"a\\\"b\")\n'''""#));
        assert!(snippet.contains("python -u /content/.colab-cli/jobs/job-1/main.py"));
        assert!(
            start_snippet("x", "ls", true)
                .unwrap()
                .contains("bash /content/.colab-cli/jobs/x/main.sh")
        );
        assert!(check_id("../x").is_err());
        assert!(new_id(Some("train")).unwrap().starts_with("train-"));
    }

    #[test]
    fn log_tails_start_on_a_line() {
        assert_eq!(last_lines(b"partial\nb\nc\nd\n", 2, true), "c\nd");
        assert_eq!(last_lines(b"a\nb\n", 5, false), "a\nb");
        let parsed =
            parse_line(&json!({"status": "ok", "text": "noise\n{\"job\": \"j\", \"pid\": 3}\n"}))
                .unwrap();
        assert_eq!(parsed["pid"], 3);
    }
}
