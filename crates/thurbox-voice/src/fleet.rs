//! Reaching a thurbox 3.x session, which is a Fleet Task.
//!
//! thurbox names a session `<fleet>:<task>` (`local:operator-1`). Fleet knows
//! which runtime-mux session runs that Task (`fleet get task`), and runtime-mux
//! is what can type into its terminal and read its screen. `fleet send` is not
//! used: it queues the text as the agent's next prompt and submits it, where a
//! dictation goes into the composer for the user to read before pressing Enter.

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

/// The only Fleet whose sessions run on this machine, next to the microphone.
const LOCAL: &str = "local";

/// The runtime-mux protocol version these requests are written in.
const PROTOCOL: u64 = 2;

/// A Task's live session, as runtime-mux addresses it.
pub struct Session {
    /// The Task's name, for messages.
    pub task: String,
    /// The runtime-mux place it runs on.
    pub place: String,
    /// runtime-mux's id for the session (`status.session.id`).
    pub key: String,
    /// The Task as Fleet stores it, for context.
    pub task_json: Value,
}

/// Split thurbox's `<fleet>:<task>` id. A bare name is a local Task.
pub fn parse_id(id: &str) -> (&str, &str) {
    match id.split_once(':') {
        Some((fleet, task)) if !fleet.is_empty() && !task.is_empty() => (fleet, task),
        _ => (LOCAL, id),
    }
}

/// Look up the session running the Task thurbox calls `id`.
pub fn resolve(id: &str) -> Result<Session> {
    let (fleet, task) = parse_id(id);
    if fleet != LOCAL {
        bail!("{id} is on the {fleet} Fleet; dictation pastes only into local sessions");
    }
    let output = Command::new("fleet")
        .args(["get", "task", task])
        .stdin(Stdio::null())
        .output()
        .context("run fleet")?;
    if !output.status.success() {
        bail!(
            "fleet get task {task}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let task_json: Value = serde_json::from_slice(&output.stdout).context("read fleet's answer")?;
    session_of(task, task_json)
}

fn session_of(task: &str, task_json: Value) -> Result<Session> {
    let session = &task_json["status"]["session"];
    let Some(key) = session["id"].as_str() else {
        bail!("Task {task} has no running session");
    };
    // A host is `local` or `<runtime>/<place>`; runtime-mux wants the place.
    let host = session["host"].as_str().unwrap_or(LOCAL);
    let place = host.rsplit('/').next().unwrap_or(host).to_string();
    Ok(Session {
        task: task.to_string(),
        place,
        key: key.to_string(),
        task_json,
    })
}

impl Session {
    /// Type `text` into the session's composer without pressing Enter.
    pub fn type_text(&self, text: &str) -> Result<()> {
        let answer = self.call("input", json!({ "text": text, "submit": false }))?;
        match answer {
            Value::String(s) if s == "none" => {
                bail!("the session of Task {} is gone", self.task)
            }
            _ => Ok(()),
        }
    }

    /// The last `lines` of the session's screen.
    pub fn screen(&self, lines: usize) -> Option<String> {
        let answer = self.call("capture", json!({ "lines": lines })).ok()?;
        answer["screen"]["text"].as_str().map(str::to_string)
    }

    /// One runtime-mux verb; the `known` part of its answer.
    fn call(&self, verb: &str, fields: Value) -> Result<Value> {
        let mut request = json!({
            "protocol": PROTOCOL,
            "place": { "name": self.place },
            "key": self.key,
        });
        if let (Some(request), Value::Object(fields)) = (request.as_object_mut(), fields) {
            request.extend(fields);
        }
        let mut child = Command::new("runtime-mux")
            .arg(verb)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run runtime-mux")?;
        child
            .stdin
            .take()
            .context("runtime-mux stdin")?
            .write_all(request.to_string().as_bytes())?;
        let output = child.wait_with_output()?;
        known(verb, &output.stdout, &output.stderr)
    }
}

fn known(verb: &str, stdout: &[u8], stderr: &[u8]) -> Result<Value> {
    let answer: Value = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "runtime-mux {verb}: {}",
            String::from_utf8_lossy(stderr).trim()
        )
    })?;
    if let Some(known) = answer.get("known") {
        return Ok(known.clone());
    }
    let why = answer["unknown"]["why"]
        .as_str()
        .or_else(|| answer["failed"]["why"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| answer.to_string());
    bail!("runtime-mux {verb}: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_names_its_fleet_and_task() {
        assert_eq!(parse_id("local:operator-1"), ("local", "operator-1"));
        assert_eq!(parse_id("vega:coder-x"), ("vega", "coder-x"));
        assert_eq!(parse_id("operator-1"), ("local", "operator-1"));
    }

    #[test]
    fn a_remote_fleet_is_refused_before_anything_runs() {
        let err = resolve("vega:coder-x").err().unwrap().to_string();
        assert!(err.contains("vega"), "{err}");
    }

    #[test]
    fn the_session_comes_from_the_task_status() {
        let task = json!({"status": {"session": {
            "host": "local", "id": "abc.u1", "key": "operator-1.u1"}}});
        let session = session_of("operator-1", task).unwrap();
        assert_eq!(session.key, "abc.u1");
        assert_eq!(session.place, "local");

        let task = json!({"status": {"session": {"host": "mux/box", "id": "k"}}});
        assert_eq!(session_of("t", task).unwrap().place, "box");

        assert!(session_of("t", json!({"status": {}})).is_err());
    }

    #[test]
    fn an_answer_is_known_or_says_why() {
        assert_eq!(
            known("input", br#"{"known":"none"}"#, b"").unwrap(),
            json!("none")
        );
        let err = known("input", br#"{"unknown":{"why":"bad place"}}"#, b"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad place"), "{err}");
        assert!(known("input", b"", b"boom").is_err());
    }
}
