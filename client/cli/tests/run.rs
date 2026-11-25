//! `testsrc run` end to end against a fake API on localhost (plain std, no extra crates), with the cmd player.
//! Covers what a run does when the API stops it mid-way, that the player never sees the token, and --discard.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const TOKEN: &str = "tsk_TESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTEST";

/// How the fake API fails: request number `at` (1-based, counting every request), or every request from it on when
/// `repeat`, gets `status` with the JSON error `code` and `msg` (and `retry-after: 1` on a 429); status 0 closes the
/// connection without an answer, like a network blip; status 1 sends the headers with the full content-length and
/// half the body, then closes (a connection lost mid-download); status 2 sends the body with its bytes changed.
#[derive(Clone, Copy)]
struct Fail {
    at: usize,
    status: u16,
    code: &'static str,
    msg: &'static str,
    repeat: bool,
}

const fn fail(at: usize, status: u16, code: &'static str, msg: &'static str) -> Option<Fail> {
    Some(Fail { at, status, code, msg, repeat: false })
}

/// The catalog, facts and files of `cases` (ids like "codec/a.mp4"), failing as `fail_at` says. Returns the base URL
/// and the request log.
fn api(cases: &[&str], fail_at: Option<Fail>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
    let mut entries = vec![];
    for id in cases {
        let name = id.rsplit('/').next().unwrap();
        let body = format!("media of {id}").into_bytes();
        let sha = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let facts = serde_json::json!({
            "id": id, "files": [name], "sha256": {name: sha}, "video": {"codec": "h264", "width": 64, "height": 36},
            "expect": {"duration_s": 1.0},
        });
        routes.insert(format!("/v1/cases/{id}/facts"), facts.to_string().into_bytes());
        routes.insert(format!("/v1/files/{id}"), body);
        entries.push(serde_json::json!({"id": id, "layer": 1, "outcome": "must play", "size_bytes": 10, "files": [name]}));
    }
    routes.insert("/v1/catalog".into(), serde_json::json!({"snapshot": "2026-11-06", "cases": entries}).to_string().into_bytes());
    let log = Arc::new(Mutex::new(vec![]));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                    break;
                }
                if h.to_ascii_lowercase().starts_with("authorization:") {
                    auth = h[14..].trim().to_string();
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").split('?').next().unwrap().to_string();
            let n = {
                let mut l = seen.lock().unwrap();
                l.push(path.clone());
                l.len()
            };
            let failing = fail_at.filter(|f| f.at == n || (f.repeat && n > f.at));
            if failing.is_some_and(|f| f.status == 0) {
                continue; // dropped without an answer
            }
            if let (Some(f), Some(b)) = (failing, routes.get(&path)) {
                if f.status == 1 || f.status == 2 {
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", b.len());
                    let _ = stream.write_all(head.as_bytes());
                    if f.status == 1 {
                        let _ = stream.write_all(&b[..b.len() / 2]); // then the connection drops
                    } else {
                        let _ = stream.write_all(&b.iter().map(|x| x ^ 1).collect::<Vec<u8>>());
                    }
                    continue;
                }
            }
            let retry = if failing.is_some_and(|f| f.status == 429) { "retry-after: 1\r\n" } else { "" };
            let (status, body) = match (failing, routes.get(&path)) {
                (Some(f), _) => (f.status, format!(r#"{{"error": "{}", "code": "{}"}}"#, f.msg, f.code).into_bytes()),
                (_, Some(b)) if path == "/v1/catalog" || auth == format!("Bearer {TOKEN}") => (200, b.clone()),
                (_, Some(_)) => (401, br#"{"error": "no token"}"#.to_vec()),
                (_, None) => (404, br#"{"error": "No such thing."}"#.to_vec()),
            };
            let head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{retry}connection: close\r\n\r\n", body.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, log)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("testsrc-run-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(base: &str, dir: &Path, command: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_testsrc"))
        .args(["run", "--player", "cmd", "--command", command, "--out"])
        .arg(dir.join("out"))
        .arg("--junit").arg(dir.join("report.xml"))
        .args(extra)
        .env("TESTSRC_API", base).env("TESTSRC_TOKEN", TOKEN).env("TESTSRC_CACHE", dir.join("cache"))
        .env("GITHUB_STEP_SUMMARY", dir.join("summary.md"))
        .env_remove("TESTSRC_SNAPSHOT")
        .output().unwrap()
}

fn results(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("out").join("results.json")).expect("results.json written")).unwrap()
}

const CASES: [&str; 3] = ["codec/a.mp4", "codec/b.mp4", "codec/c.mp4"];

#[test]
fn all_pass() {
    let (base, _) = api(&CASES, None);
    let dir = tmp("pass");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let r = results(&dir);
    assert_eq!(r["counts"]["pass"], 3);
    assert_eq!(r["stopped"], serde_json::Value::Null);
}

// Review of 11 November (#17, should fix 2): an API error mid-run (429 quota, 403 trial limit) made the runner exit
// at once, before results.json, the JUnit report or the summary existed: every result so far was lost.
#[test]
fn quota_mid_run_keeps_the_results() {
    // requests: catalog, a facts, a file, b facts, b file, c facts <- 429
    let (base, log) = api(&CASES, fail(6, 429, "quota", "Daily quota used up. It resets at 00:00 UTC."));
    let dir = tmp("quota");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    let r = results(&dir);
    assert_eq!(r["counts"]["pass"], 2);
    assert_eq!(r["counts"]["skipped"], 1);
    assert_eq!(r["results"][2]["id"], "codec/c.mp4");
    assert_eq!(r["results"][2]["status"], "skipped");
    assert!(r["results"][2]["checks"][0][2].as_str().unwrap().starts_with("not run: 429 from /cases/codec/c.mp4/facts: Daily quota"));
    assert!(r["stopped"].as_str().unwrap().contains("429"));
    let junit = std::fs::read_to_string(dir.join("report.xml")).unwrap();
    assert!(junit.contains(r#"<skipped message="not run: 429 from /cases/codec/c.mp4/facts"#));
    let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
    assert!(summary.contains("**Stopped early:** 429 from /cases/codec/c.mp4/facts") && summary.contains("1 cases weren't run"));
    assert_eq!(log.lock().unwrap().len(), 6, "nothing fetched after the 429");
}

#[test]
fn trial_limit_on_a_file_stops_the_rest() {
    // catalog, a facts, a file <- 403: nothing played
    let (base, log) = api(&CASES, fail(3, 403, "trial_limit", "Evaluation tokens can fetch 30 different cases."));
    let dir = tmp("trial");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    let r = results(&dir);
    assert_eq!(r["counts"]["skipped"], 3);
    assert!(String::from_utf8_lossy(&out.stderr).contains("403 from /files/codec/a.mp4: Evaluation tokens"));
    assert_eq!(log.lock().unwrap().len(), 3);
    // no half-downloaded file left behind
    assert!(!dir.join("cache/2026-11-06/codec/a.mp4").exists());
    assert!(!dir.join("cache/2026-11-06/codec/a.mp4.part").exists());
}

#[test]
fn an_error_before_any_case_is_still_exit_2_without_results() {
    let (base, _) = api(&CASES, fail(1, 503, "internal", "down"));
    let dir = tmp("catalog");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("503 from /catalog: down"));
    assert!(!dir.join("out/results.json").exists());
}

#[test]
fn checksum_mismatch_stops_the_run() {
    let (base, _) = api(&["codec/a.mp4"], None);
    let dir = tmp("sha");
    // a cached facts file that names other bytes: the download can't match it
    let facts = dir.join("cache/2026-11-06/codec/a.mp4.facts.json");
    std::fs::create_dir_all(facts.parent().unwrap()).unwrap();
    std::fs::write(&facts, r#"{"files": ["a.mp4"], "sha256": {"a.mp4": "00"}, "video": {}}"#).unwrap();
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(results(&dir)["stopped"].as_str().unwrap().starts_with("checksum mismatch for codec/a.mp4"));
}

// Review of 11 November (#17, nit 6): the player under test inherited TESTSRC_TOKEN.
#[test]
fn the_player_never_sees_the_token() {
    let (base, _) = api(&["codec/a.mp4"], None);
    let dir = tmp("env");
    let seen = dir.join("seen.txt");
    let cmd = format!("env > '{}'; true {{file}}", seen.display());
    let out = run(&base, &dir, &cmd, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(env.contains("TESTSRC_CACHE="), "the player's environment was captured");
    assert!(!env.contains("TESTSRC_TOKEN") && !env.contains(TOKEN));
}

#[test]
fn discard_deletes_each_case_s_files_after_it_played() {
    let (base, _) = api(&CASES, None);
    let kept = tmp("keep");
    assert_eq!(run(&base, &kept, "true {file}", &[]).status.code(), Some(0));
    assert!(kept.join("cache/2026-11-06/codec/a.mp4").exists());
    let dir = tmp("discard");
    let out = run(&base, &dir, "test -s {file}", &["--discard"]);
    assert_eq!(out.status.code(), Some(0), "the file was there while the case played");
    for id in CASES {
        assert!(!dir.join("cache/2026-11-06").join(id).exists(), "{id} deleted");
        assert!(dir.join("cache/2026-11-06").join(format!("{id}.facts.json")).exists(), "{id}'s facts kept");
    }
}

fn testsrc(args: &[&str], base: &str, envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_testsrc"));
    c.args(args).env("TESTSRC_API", base).env("TESTSRC_TOKEN", TOKEN).env_remove("TESTSRC_SNAPSHOT").env_remove("TESTSRC_CACHE");
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn list_and_fetch() {
    let (base, _) = api(&CASES, None);
    let dir = tmp("list");
    let out = testsrc(&["list", "codec/"], &base, &[]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("codec/b.mp4") && text.contains("3 cases in snapshot 2026-11-06"), "{text}");
    let cache = dir.join("cache");
    let out = testsrc(&["fetch", "codec/a.mp4", "--cache", cache.to_str().unwrap()], &base, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(cache.join("2026-11-06/codec/a.mp4").exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 cases in"));
    // the default cache is ~/.cache/testsrc
    let home = dir.join("home");
    let out = testsrc(&["fetch", "codec/b.mp4"], &base, &[("HOME", home.to_str().unwrap())]);
    assert_eq!(out.status.code(), Some(0));
    assert!(home.join(".cache/testsrc/2026-11-06/codec/b.mp4").exists());
}

#[test]
fn fetch_stops_at_an_api_error() {
    let (base, _) = api(&CASES, fail(2, 429, "quota", "Daily quota used up."));
    let dir = tmp("fetch429");
    let out = testsrc(&["fetch", "--cache", dir.to_str().unwrap()], &base, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("429 from /cases/codec/a.mp4/facts: Daily quota used up."));
}

#[test]
fn case_selection() {
    let (base, _) = api(&["codec/h264-aac.mp4", "fps/60.mp4", "edge/x.mp4"], None);
    let out = testsrc(&["list", "starter"], &base, &[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("2 cases in snapshot"), "starter names the starter cases there are: {text}");
    let out = testsrc(&["list", "nothing/"], &base, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no case matches"));
    let out = testsrc(&["list", "--exclude", "edge/"], &base, &[]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("2 cases in snapshot"));
}

#[test]
fn cmd_needs_a_command() {
    let out = testsrc(&["run", "--player", "cmd"], "http://127.0.0.1:9/v1", &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--player cmd needs --command"));
}

#[test]
fn a_flaky_player_passes_on_a_retry_and_keeps_the_first_failure() {
    let (base, _) = api(&["codec/a.mp4"], None);
    let dir = tmp("retry");
    let flag = dir.join("played-once");
    // fails the first time, passes after
    let cmd = format!("test -e '{f}' || {{ touch '{f}'; exit 3; }} # {{file}}", f = flag.display());
    let out = run(&base, &dir, &cmd, &["--retries", "2"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let r = results(&dir);
    assert_eq!(r["results"][0]["status"], "pass");
    let retry = r["results"][0]["checks"].as_array().unwrap().iter().find(|c| c[1] == "retry").expect("a retry line");
    assert!(retry[2].as_str().unwrap().starts_with("passed on retry 1; the first run failed: run: exit code 3"));
    // always failing: every retry used, the failure stays
    let dir = tmp("retry-fail");
    let out = run(&base, &dir, "exit 3 # {file}", &["--retries", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(results(&dir)["counts"]["fail"], 1);
}

#[test]
fn a_subtitle_sidecar_is_skipped_not_played() {
    let (base, _) = api(&["subs/a.srt"], None);
    let dir = tmp("sidecar");
    // the fake API gives every case video facts: a cached facts file without video or audio stands for a sidecar
    let name = "subs/a.srt";
    let body = format!("media of {name}");
    let sha = Sha256::digest(body.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>();
    let facts = dir.join("cache/2026-11-06/subs/a.srt.facts.json");
    std::fs::create_dir_all(facts.parent().unwrap()).unwrap();
    std::fs::write(&facts, serde_json::json!({"files": ["a.srt"], "sha256": {"a.srt": sha}}).to_string()).unwrap();
    let out = run(&base, &dir, "exit 3 # {file}", &[]);
    assert_eq!(out.status.code(), Some(0));
    let r = results(&dir);
    assert_eq!(r["results"][0]["status"], "skipped");
    assert!(r["results"][0]["checks"][0][2].as_str().unwrap().contains("subtitle sidecar"));
}

#[test]
fn a_player_that_cant_load_is_a_result_and_the_run_goes_on() {
    let (base, _) = api(&["codec/a.mp4", "codec/b.mp4"], None);
    for player in ["libvlc", "gstreamer", "mpv", "ffmpeg"] {
        let dir = tmp(&format!("noplayer-{player}"));
        let out = testsrc(&["run", "--player", player, "--capture", "rgb", "--out", dir.join("out").to_str().unwrap(),
                            "--cache", dir.join("cache").to_str().unwrap()], &base,
                          &[("LIBVLC", "/nonexistent/libvlc.so.5"), ("LIBMPV", "/nonexistent/libmpv.so.2"), ("PATH", "/nonexistent"),
                            ("LD_LIBRARY_PATH", "/nonexistent")]);
        assert_eq!(out.status.code(), Some(1), "{player}: {}", String::from_utf8_lossy(&out.stderr));
        let r = results(&dir);
        assert_eq!(r["results"].as_array().unwrap().len(), 2, "{player}: both cases have a result");
        for c in r["results"].as_array().unwrap() {
            assert!(matches!(c["status"].as_str().unwrap(), "fail" | "crash"), "{player}: {}", c["status"]);
        }
        assert_eq!(r["engine"], serde_json::Value::Null, "{player}: no version read, none made up");
        // what to do, said at the first case and at the end, in results.json and the summary (not per case)
        let err = String::from_utf8_lossy(&out.stderr);
        let hint = if player == "ffmpeg" { "Put ffmpeg and ffprobe 5.1 or later on PATH" } else { "sudo apt-get install -y" };
        assert_eq!(err.matches(hint).count(), 2, "{player}: {err}");
        assert!(r["setup_error"].as_str().unwrap().contains(hint), "{player}");
        if player == "libvlc" {
            assert!(r["player"].as_str().unwrap().ends_with("(RGB callbacks)"));
        }
    }
}

#[test]
fn matrix_from_results_files() {
    let (base, _) = api(&CASES, None);
    let dir = tmp("matrix");
    assert_eq!(run(&base, &dir, "true {file}", &[]).status.code(), Some(0));
    let results = dir.join("out/results.json");
    let md = dir.join("README.md");
    let out = testsrc(&["matrix", results.to_str().unwrap(), "--out", md.to_str().unwrap(), "--case-url", "https://testsrc.com/cases/{id}"], &base, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(text.contains("Snapshot 2026-11-06") && text.contains("| codec (3) |") && text.contains("✅ 3/3"), "{text}");
    let out = testsrc(&["matrix", results.to_str().unwrap()], &base, &[]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Snapshot 2026-11-06"));
    let out = testsrc(&["matrix", "/nonexistent.json"], &base, &[]);
    assert_eq!(out.status.code(), Some(2));
}

// Review round 2, 12 November (#17 should fix 1): the per-minute limit (429 rate_limited, retry-after) and network
// blips are transient, but they ended the run like the quota does. They are retried now; the quota isn't.
#[test]
fn a_rate_limit_is_waited_out_not_the_end_of_the_run() {
    // catalog, a facts, a file, b facts <- 429 rate_limited once, retried after its retry-after (1 s)
    let (base, log) = api(&CASES, fail(4, 429, "rate_limited", "Too many requests this minute. Slow down and retry."));
    let dir = tmp("ratelimit");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(results(&dir)["counts"]["pass"], 3);
    assert_eq!(log.lock().unwrap().len(), 8, "one request more: the retry");
    assert!(String::from_utf8_lossy(&out.stderr).contains("rate limited"), "it says why it waits");
}

#[test]
fn a_rate_limit_that_doesnt_end_stops_after_three_retries() {
    let (base, log) = api(&CASES, Some(Fail { at: 4, status: 429, code: "rate_limited", msg: "Too many requests this minute.", repeat: true }));
    let dir = tmp("ratelimit-forever");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    let r = results(&dir);
    assert_eq!(r["counts"]["pass"], 1);
    assert_eq!(r["counts"]["skipped"], 2);
    assert_eq!(log.lock().unwrap().len(), 7, "the request and three retries, then nothing");
}

#[test]
fn a_dropped_connection_is_retried() {
    let (base, log) = api(&CASES, fail(3, 0, "", ""));
    let dir = tmp("blip");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(results(&dir)["counts"]["pass"], 3);
    assert_eq!(log.lock().unwrap().len(), 8);
}

// Review round 2, nit 7: "See GET /v1/usage.. Stopping here", a doubled full stop.
#[test]
fn the_stop_message_has_one_full_stop() {
    let (base, _) = api(&CASES, fail(2, 429, "quota", "Daily quota used up. See GET /v1/usage."));
    let dir = tmp("fullstop");
    let out = run(&base, &dir, "true {file}", &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("See GET /v1/usage. Stopping here"), "{err}");
    assert!(!err.contains(".."), "{err}");
}

// Review round 3, 12 November (R3-1): the retry was around the request only, so a connection lost while the body was
// being read still ended the run. The whole download is retried now, and a download whose checksum doesn't match.
fn gets_of(log: &Arc<Mutex<Vec<String>>>, path: &str) -> usize {
    log.lock().unwrap().iter().filter(|p| p.as_str() == path).count()
}

#[test]
fn a_connection_lost_mid_download_is_retried() {
    // catalog, a facts, a file <- half the body, then the connection drops
    let (base, log) = api(&CASES, fail(3, 1, "", ""));
    let dir = tmp("midbody");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(results(&dir)["counts"]["pass"], 3);
    assert_eq!(gets_of(&log, "/v1/files/codec/a.mp4"), 2, "the file is fetched again");
    assert!(!dir.join("cache/2026-11-06/codec/a.mp4.part").exists());
}

#[test]
fn a_corrupt_download_is_fetched_again() {
    let (base, log) = api(&CASES, fail(3, 2, "", ""));
    let dir = tmp("corrupt");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(results(&dir)["counts"]["pass"], 3);
    assert_eq!(gets_of(&log, "/v1/files/codec/a.mp4"), 2);
}

#[test]
fn a_download_that_never_matches_stops_after_two_retries() {
    let (base, log) = api(&CASES, Some(Fail { at: 3, status: 2, code: "", msg: "", repeat: true }));
    let dir = tmp("corrupt-forever");
    let out = run(&base, &dir, "true {file}", &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(gets_of(&log, "/v1/files/codec/a.mp4"), 3, "the download and two retries, as get() retries a network error");
    let r = results(&dir);
    assert_eq!(r["counts"]["skipped"], 3);
    assert!(r["stopped"].as_str().unwrap().starts_with("checksum mismatch for codec/a.mp4"));
    assert!(!dir.join("cache/2026-11-06/codec/a.mp4").exists(), "no bad file left in the cache");
}

// Launch plan item 14: --time-limit stops the run like the API does (exit 2, results so far, the rest not run), so the
// report outlives the CI job's own limit.
#[test]
fn the_time_limit_stops_the_run_and_keeps_the_report() {
    let (base, log) = api(&CASES, None);
    let dir = tmp("timelimit");
    // 0.03 min = 1.8 s: a plays (1 s), b is cut while it plays, c is never fetched
    let out = run(&base, &dir, "sleep 1 # {file}", &["--time-limit", "0.03"]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let r = results(&dir);
    assert_eq!(r["counts"]["pass"], 1);
    assert_eq!(r["counts"]["skipped"], 2);
    assert_eq!(r["counts"]["hang"], 0, "a case cut by the limit isn't the player's hang");
    assert_eq!(r["time_limit_min"], 0.03);
    assert_eq!(r["stopped"], "the time limit of 0.03 minutes was reached");
    assert_eq!(r["results"][1]["checks"][0][2], "not run: the time limit of 0.03 minutes was reached while it played");
    assert_eq!(r["results"][2]["checks"][0][2], "not run: the time limit of 0.03 minutes was reached");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("testsrc: the time limit of 0.03 minutes was reached. Stopping here"), "{err}");
    let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
    assert!(summary.contains("**Stopped early:** the time limit of 0.03 minutes was reached. 2 cases weren't run."), "{summary}");
    assert!(std::fs::read_to_string(dir.join("report.xml")).unwrap().contains("<skipped message=\"not run: the time limit"));
    assert_eq!(gets_of(&log, "/v1/cases/codec/c.mp4/facts"), 0, "nothing fetched past the limit");
}

#[test]
fn a_retry_never_runs_past_the_time_limit() {
    let (base, _) = api(&["codec/a.mp4", "codec/b.mp4"], None);
    let dir = tmp("timelimit-retry");
    // a fails after 1.2 s; its retry is cut at 1.8 s, so a keeps its failure, and b isn't run
    let out = run(&base, &dir, "sleep 1.2; exit 3 # {file}", &["--time-limit", "0.03", "--retries", "3"]);
    assert_eq!(out.status.code(), Some(2));
    let r = results(&dir);
    assert_eq!(r["results"][0]["status"], "fail");
    assert_eq!(r["results"][1]["status"], "skipped");
}

#[test]
fn a_time_limit_of_zero_is_refused() {
    let out = testsrc(&["run", "--player", "cmd", "--command", "true {file}", "--time-limit", "0"], "http://127.0.0.1:9/v1", &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--time-limit is in minutes and must be more than 0, not 0"));
}

// Launch plan item 15: results.json says what was tested.
#[test]
fn results_say_what_was_tested() {
    let (base, _) = api(&["codec/a.mp4"], None);
    let dir = tmp("versions");
    let mut c = Command::new(env!("CARGO_BIN_EXE_testsrc"));
    c.args(["run", "--player", "cmd", "--command", "true {file}", "--player-version", " 1.4.2-rc1 ", "--out"]).arg(dir.join("out"))
        .env("TESTSRC_API", &base).env("TESTSRC_TOKEN", TOKEN).env("TESTSRC_CACHE", dir.join("cache"))
        .env("GITHUB_STEP_SUMMARY", dir.join("summary.md")).env_remove("TESTSRC_SNAPSHOT")
        .env("TESTSRC_COMMIT", "0123456789abcdef0123456789abcdef01234567").env("GITHUB_SHA", "ffff")
        .env("GITHUB_HEAD_REF", "feature/x").env("GITHUB_REF_NAME", "7/merge").env("GITHUB_REPOSITORY", "acme/player");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let r = results(&dir);
    assert_eq!(r["commit"], "0123456789abcdef0123456789abcdef01234567", "TESTSRC_COMMIT (a pull request's head) first");
    assert_eq!(r["branch"], "feature/x", "a pull request's head branch, not 7/merge");
    assert_eq!(r["repository"], "acme/player");
    assert_eq!(r["player_version"], "1.4.2-rc1");
    assert_eq!(r["engine"], serde_json::Value::Null, "a cmd player has no engine the runner knows");
    assert_eq!(r["time_limit_min"], serde_json::Value::Null);
    assert!(r["finished"].as_str().unwrap().ends_with('Z'));
    let line = "Tested: commit 0123456789ab (feature/x), player version 1.4.2-rc1, runner ";
    assert!(String::from_utf8_lossy(&out.stdout).contains(line));
    assert!(std::fs::read_to_string(dir.join("summary.md")).unwrap().contains(line));
}

// A stub git on PATH (CI's image has none): what `git rev-parse` would print in a checkout of branch $STUB_BRANCH.
#[test]
fn outside_actions_the_commit_and_branch_come_from_git() {
    let (base, _) = api(&["codec/a.mp4"], None);
    let dir = tmp("versions-git");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("git");
    std::fs::write(&stub, "#!/bin/sh\ncase \"$*\" in\n  'rev-parse HEAD') echo 0123abcd ;;\n  'rev-parse --abbrev-ref HEAD') echo \"$STUB_BRANCH\" ;;\n  *) exit 1 ;;\nesac\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run_in = |envs: &[(&str, &str)]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_testsrc"));
        c.args(["run", "--player", "cmd", "--command", "true {file}", "--out"]).arg(dir.join("out"))
            .env("PATH", &path).env("STUB_BRANCH", "dev")
            .env("TESTSRC_API", &base).env("TESTSRC_TOKEN", TOKEN).env("TESTSRC_CACHE", dir.join("cache"))
            .env_remove("TESTSRC_SNAPSHOT").env_remove("TESTSRC_COMMIT").env_remove("GITHUB_SHA").env_remove("GITHUB_HEAD_REF")
            .env_remove("GITHUB_REF_NAME").env_remove("GITHUB_REPOSITORY").env_remove("GITHUB_STEP_SUMMARY").env_remove("TESTSRC_PLAYER_VERSION");
        for (k, v) in envs {
            c.env(k, v);
        }
        assert_eq!(c.output().unwrap().status.code(), Some(0));
        results(&dir)
    };
    let r = run_in(&[]);
    assert_eq!(r["commit"], "0123abcd");
    assert_eq!(r["branch"], "dev");
    assert_eq!(r["repository"], serde_json::Value::Null);
    assert_eq!(r["player_version"], serde_json::Value::Null);
    // a detached checkout ("HEAD") has no branch; GITHUB_SHA and GITHUB_REF_NAME win over git
    assert_eq!(run_in(&[("STUB_BRANCH", "HEAD")])["branch"], serde_json::Value::Null);
    let r = run_in(&[("GITHUB_SHA", "abc"), ("GITHUB_REF_NAME", "main"), ("TESTSRC_PLAYER_VERSION", "  ")]);
    assert_eq!((r["commit"].as_str(), r["branch"].as_str()), (Some("abc"), Some("main")));
    assert_eq!(r["player_version"], serde_json::Value::Null, "a blank version is no version");
    // no git at all, or not a checkout: nothing
    let r = run_in(&[("PATH", "/usr/bin:/bin"), ("GIT_DIR", "/nonexistent")]);
    assert_eq!((r["commit"].clone(), r["branch"].clone()), (serde_json::Value::Null, serde_json::Value::Null));
}
