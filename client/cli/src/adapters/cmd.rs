//! Level 0: any player as a command line, `{file}` replaced by the case's path. No integration at all, so
//! it sees only what the process does: exit code, crash, hang, how long it took.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use testsrc_core::pyfmt;

pub fn play(template: &str, path: &str, timeout: f64) -> Vec<Value> {
    let line = template.replace("{file}", &quote(path));
    let program = template.split_whitespace().next().unwrap_or("cmd").to_string();
    let mut events = vec![json!({"t": "meta", "player": format!("cmd: {program}")})];
    let t0 = Instant::now();
    let mut child = match shell(&line).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            events.push(json!({"t": "log", "level": "error", "msg": format!("can't start {program}: {e}")}));
            events.push(json!({"t": "exit", "code": 127, "wall": 0.0}));
            return events;
        }
    };
    let stderr = child.stderr.take().unwrap();
    let tail = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut s);
        s
    });
    let code: Value = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().map(Value::from).unwrap_or_else(|| "crash".into()),
            Ok(None) if t0.elapsed().as_secs_f64() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break "timeout".into();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break format!("{e}").into(),
        }
    };
    let err = tail.join().unwrap_or_default();
    if code != json!(0) {
        if let Some(last) = err.lines().rev().find(|l| !l.trim().is_empty()) {
            events.push(json!({"t": "log", "level": "error", "msg": last.trim()}));
        }
    }
    events.push(json!({"t": "exit", "code": code, "wall": pyfmt::round_to(t0.elapsed().as_secs_f64(), 2)}));
    events
}

#[cfg(windows)]
fn shell(line: &str) -> Command {
    let mut c = Command::new("cmd");
    c.args(["/C", line]);
    c
}

#[cfg(not(windows))]
fn shell(line: &str) -> Command {
    let mut c = Command::new("sh");
    c.args(["-c", line]);
    c
}

#[cfg(windows)]
fn quote(p: &str) -> String {
    format!("\"{p}\"")
}

#[cfg(not(windows))]
fn quote(p: &str) -> String {
    format!("'{}'", p.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(events: &[Value]) -> &Value {
        &events.last().unwrap()["code"]
    }

    #[test]
    fn exit_codes_and_timeout() {
        assert_eq!(exit(&play("exit 0", "x", 10.0)), &json!(0));
        let failed = play("echo broken {file} 1>&2 && exit 3", "a b.mp4", 10.0);
        assert_eq!(exit(&failed), &json!(3));
        let log = failed.iter().find(|e| e["t"] == "log").unwrap();
        assert!(log["msg"].as_str().unwrap().contains("broken") && log["msg"].as_str().unwrap().contains("a b.mp4"));
        let slow = if cfg!(windows) { "ping -n 5 127.0.0.1 >nul" } else { "sleep 5" };
        assert_eq!(exit(&play(slow, "x", 0.5)), &json!("timeout"));
    }
}
