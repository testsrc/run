//! testsrc runner: fetch test media through the API and check how a player plays it.
//!
//!   export TESTSRC_TOKEN=tsk_...
//!   testsrc list   [CASE or PREFIX ...]
//!   testsrc fetch  [CASE or PREFIX ...]
//!   testsrc run    --player libvlc [CASE or PREFIX | starter ...] [--exclude duration/] [--junit report.xml]
//!
//! Files are cached by snapshot and checked against their SHA-256; facts come from /v1/cases/{id}/facts.
//! Each case plays in its own process, so a player crash or hang is a result, not the end of the run.
//! When the API stops answering mid-run (an evaluation token's case limit, the daily quota), the run stops fetching
//! and still writes its results, with the cases left marked skipped, then exits 2.
//! The player never sees TESTSRC_TOKEN: it is the code under test, and its logs may end up in public artifacts.

mod adapters;
mod api;
mod matrix;
mod report;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use testsrc_core::{check, Line, Matrix, Range, Status};

use adapters::libvlc::Capture;
use report::CaseResult;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Ten cases that cover the classic breakages in about two minutes of playback.
const STARTER: [&str; 10] = [
    "codec/h264-aac.mp4", "codec/hevc-aac.mp4", "codec/av1-opus.webm", "pixel/h264-full-range.mp4", "hdr/hevc-pq-hdr10.mp4",
    "resolution/portrait-1080x1920.mp4", "fps/60.mp4", "hls/fmp4.m3u8", "hls/ts.m3u8", "audio-only/opus.opus",
];

pub fn die(msg: &str) -> ! {
    eprintln!("testsrc: {msg}");
    std::process::exit(2)
}

#[derive(Parser)]
#[command(name = "testsrc", version, about = "Fetch test media through the testsrc API and check how a player plays it")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Player {
    Libvlc,
    Ffmpeg,
    Gstreamer,
    Mpv,
    Cmd,
}

impl Player {
    fn name(self) -> &'static str {
        match self {
            Player::Libvlc => "libvlc",
            Player::Ffmpeg => "ffmpeg",
            Player::Gstreamer => "gstreamer",
            Player::Mpv => "mpv",
            Player::Cmd => "cmd",
        }
    }
}

#[derive(clap::Args)]
struct Common {
    /// Case ids or prefixes (hls/, audio-only/aac), or "starter"; default: every case
    cases: Vec<String>,
    /// Leave out cases whose id starts with this (repeatable)
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long, env = "TESTSRC_API", default_value = "https://testsrc.com/v1")]
    api: String,
    /// Pin a snapshot (2026-10-29); default: the latest
    #[arg(long, env = "TESTSRC_SNAPSHOT")]
    snapshot: Option<String>,
    /// Where files are cached, per snapshot
    #[arg(long, env = "TESTSRC_CACHE")]
    cache: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List cases
    List(Common),
    /// Download cases into the cache
    Fetch(Common),
    /// Play cases through a player and check them
    Run {
        #[command(flatten)]
        common: Common,
        #[arg(long, value_enum, default_value = "libvlc")]
        player: Player,
        /// For --player cmd: the command line, with {file} for the case's path
        #[arg(long)]
        command: Option<String>,
        #[arg(long, default_value = "testsrc-results")]
        out: PathBuf,
        /// Write a JUnit XML report here
        #[arg(long)]
        junit: Option<PathBuf>,
        /// Capture width (the player scales to it)
        #[arg(long, default_value_t = 640)]
        width: u32,
        /// libvlc: the decoder's YUV (default), or RGB converted by libvlc, as callback apps get it
        #[arg(long, value_parser = ["yuv", "rgb"], default_value = "yuv")]
        capture: String,
        /// Play a failing case again up to N times; a pass on a retry counts, with the first failure kept as INFO
        #[arg(long, default_value_t = 0)]
        retries: u32,
        /// Delete each case's files from the cache once it has played (facts stay): for runs larger than the disk
        #[arg(long)]
        discard: bool,
    },
    /// Turn several engines' results.json into the player-matrix dashboard (Markdown)
    Matrix {
        /// results.json files, one per engine
        results: Vec<PathBuf>,
        #[arg(long, default_value = "https://testsrc.com")]
        site: String,
        /// Case page URL with {id}, to link each case
        #[arg(long)]
        case_url: Option<String>,
        /// Write here instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// One case in this process (the runner starts it once per case)
    #[command(name = "_play", hide = true)]
    Play {
        player: Player,
        path: String,
        out: PathBuf,
        #[arg(long)]
        timeout: f64,
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value = "yuv")]
        capture: String,
        #[arg(long, default_value = "bt709")]
        matrix: String,
        #[arg(long, default_value = "limited")]
        range: String,
        /// The picture size from the fact sheet (WxH), for when the player can't tell it before playing
        #[arg(long)]
        size: Option<String>,
        #[arg(long)]
        command: Option<String>,
    },
}

fn default_cache() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".cache").join("testsrc")
}

fn main() {
    match Cli::parse().cmd {
        Cmd::Play { player, path, out, timeout, width, capture, matrix, range, size, command } => {
            let hint = size.as_deref().and_then(|s| s.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
            let events = match player {
                Player::Libvlc => adapters::libvlc::play(&path, &format!("{}.vlclog", out.display()), &adapters::libvlc::Options {
                    timeout, width, hint,
                    capture: if capture == "rgb" { Capture::Rgb } else { Capture::Yuv },
                    matrix: Matrix::from_name(&matrix).unwrap_or(Matrix::Bt709),
                    range: Range::from_name(&range).unwrap_or(Range::Limited),
                }),
                Player::Ffmpeg => adapters::ffmpeg::play(&path, width),
                Player::Gstreamer => adapters::gstreamer::play(&path, &adapters::gstreamer::Options {
                    timeout, width, hint,
                    matrix: Matrix::from_name(&matrix).unwrap_or(Matrix::Bt709),
                    range: Range::from_name(&range).unwrap_or(Range::Limited),
                }),
                Player::Mpv => adapters::mpv::play(&path, &adapters::mpv::Options { timeout, width }),
                Player::Cmd => adapters::cmd::play(command.as_deref().unwrap_or_else(|| die("--player cmd needs --command")), &path, timeout),
            };
            let text: String = events.iter().map(|e| e.to_string() + "\n").collect();
            std::fs::write(&out, text).unwrap_or_else(|e| die(&format!("{}: {e}", out.display())));
        }
        Cmd::List(c) => {
            let (api, catalog, cases) = open(&c);
            let snapshot = catalog["snapshot"].as_str().unwrap_or("");
            for c in &cases {
                println!("{:44} layer {}  {:8.1} MB  {}", c["id"].as_str().unwrap_or(""), c["layer"],
                         c["size_bytes"].as_f64().unwrap_or(0.0) / 1e6, c["outcome"].as_str().unwrap_or(""));
            }
            println!("{} cases in snapshot {snapshot}", cases.len());
            drop(api);
        }
        Cmd::Fetch(c) => {
            let (api, catalog, cases) = open(&c);
            let cache = cache_dir(&c, &catalog);
            for (i, case) in cases.iter().enumerate() {
                api::fetch(&api, case, &cache).unwrap_or_else(|e| die(&e.to_string()));
                print!("\r{}/{} {:60}", i + 1, cases.len(), case["id"].as_str().unwrap_or(""));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            println!("\n{} cases in {}", cases.len(), cache.display());
        }
        Cmd::Run { common, player, command, out, junit, width, capture, retries, discard } => {
            run(&common, player, command, &out, junit, width, &capture, retries, discard)
        }
        Cmd::Matrix { results, site, case_url, out } => {
            let docs: Vec<Value> = results.iter().map(|p| {
                serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| die(&format!("{}: {e}", p.display()))))
                    .unwrap_or_else(|e| die(&format!("{}: {e}", p.display())))
            }).collect();
            let md = matrix::render(&docs, &matrix::Options { site, case_url });
            match out {
                Some(p) => std::fs::write(&p, md).unwrap_or_else(|e| die(&format!("{}: {e}", p.display()))),
                None => print!("{md}"),
            }
        }
    }
}

fn open(c: &Common) -> (api::Api, Value, Vec<Value>) {
    let api = api::Api::new(&c.api, std::env::var("TESTSRC_TOKEN").ok().filter(|t| !t.is_empty()), c.snapshot.clone());
    let catalog = api.json("/catalog", false).unwrap_or_else(|e| die(&e.to_string()));
    let patterns: Vec<String> = c.cases.iter().flat_map(|p| {
        if p == "starter" { STARTER.iter().map(|s| s.to_string()).collect() } else { vec![p.clone()] }
    }).collect();
    let all = catalog["cases"].as_array().cloned().unwrap_or_default();
    let cases: Vec<Value> = api::select(&all, &patterns, &c.exclude).into_iter().cloned().collect();
    if cases.is_empty() {
        die("no case matches");
    }
    (api, catalog, cases)
}

fn cache_dir(c: &Common, catalog: &Value) -> PathBuf {
    c.cache.clone().unwrap_or_else(default_cache).join(catalog["snapshot"].as_str().unwrap_or("latest"))
}

#[allow(clippy::too_many_arguments)]
fn run(c: &Common, player: Player, command: Option<String>, out: &Path, junit: Option<PathBuf>, width: u32, capture: &str, retries: u32, discard: bool) {
    if player == Player::Cmd && command.is_none() {
        die("--player cmd needs --command \"yourplayer --headless {file}\"");
    }
    let (api, catalog, cases) = open(c);
    let snapshot = catalog["snapshot"].as_str().unwrap_or("").to_string();
    let cache = cache_dir(c, &catalog);
    std::fs::create_dir_all(out).unwrap_or_else(|e| die(&format!("{}: {e}", out.display())));
    let started = report::utc_now();
    let name = player.name();
    println!("testsrc runner {VERSION}: {} cases from snapshot {snapshot}, player {name}", cases.len());
    let mut results = vec![];
    let mut stopped: Option<String> = None;
    for (i, case) in cases.iter().enumerate() {
        let id = case["id"].as_str().unwrap_or("").to_string();
        let fetched = match &stopped {
            Some(_) => None,
            None => match api::fetch(&api, case, &cache) {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("testsrc: {}. Stopping here: the results so far are kept, the cases left are skipped.",
                              e.to_string().trim_end_matches('.'));
                    stopped = Some(e.to_string());
                    None
                }
            },
        };
        let Some(path) = fetched else {
            let why = stopped.as_deref().unwrap_or("");
            results.push(CaseResult { id: id.clone(), status: "skipped", wall: 0.0, player: None,
                                      checks: vec![Line { status: Status::Info, what: "run".into(), detail: format!("not run: {why}") }] });
            println!("[{:3}/{}] {:7} {:44} {:6.1} s  not run", i + 1, cases.len(), "SKIPPED", id, 0.0);
            continue;
        };
        let facts: Value = serde_json::from_slice(&std::fs::read(format!("{}.facts.json", path.display())).unwrap_or_default()).unwrap_or_default();
        let r = if facts["video"].is_null() && facts["audio"].is_null() {
            CaseResult { id, status: "skipped", wall: 0.0, player: None,
                         checks: vec![Line { status: Status::Info, what: "run".into(), detail: "subtitle sidecar: not played on its own".into() }] }
        } else {
            let mut r = run_case(player, command.as_deref(), &id, &path, &facts, out, width, capture);
            for attempt in 1..=retries {
                if r.status == "pass" {
                    break;
                }
                let again = run_case(player, command.as_deref(), &id, &path, &facts, out, width, capture);
                if again.status == "pass" {
                    // flaky, not fixed: keep what the first run saw
                    let first = r.first_failure();
                    r = again;
                    r.checks.push(Line { status: Status::Info, what: "retry".into(),
                                         detail: format!("passed on retry {attempt}; the first run failed: {first}") });
                    break;
                }
            }
            r
        };
        if discard {
            for f in api::files_of(case, &cache) {
                let _ = std::fs::remove_file(f);
            }
        }
        let first = r.first_failure();
        println!("[{:3}/{}] {:7} {:44} {:6.1} s  {}", i + 1, cases.len(), r.status.to_uppercase(), r.id, r.wall, testsrc_core::pyfmt::prefix(&first, 90));
        results.push(r);
    }
    let counts = report::counts(&results);
    let mut player_name = results.iter().find_map(|r| r.player.clone()).unwrap_or_else(|| name.into());
    if player == Player::Libvlc {
        player_name += if capture == "rgb" { " (RGB callbacks)" } else { " (YUV callbacks)" };
    }
    let doc = serde_json::json!({
        "runner": VERSION, "snapshot": snapshot, "player": player_name, "started": started, "stopped": stopped,
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "counts": counts.iter().map(|(s, n)| (s.to_string(), Value::from(*n))).collect::<serde_json::Map<_, _>>(),
        "results": results.iter().map(CaseResult::to_json).collect::<Vec<_>>(),
    });
    std::fs::write(out.join("results.json"), serde_json::to_string_pretty(&doc).unwrap()).unwrap_or_else(|e| die(&e.to_string()));
    if let Some(j) = junit {
        std::fs::write(&j, report::junit(&results, &format!("testsrc {player_name}"))).unwrap_or_else(|e| die(&e.to_string()));
    }
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(summary) {
            let mut md = report::github_summary(&results, &player_name, &snapshot);
            if let Some(why) = &stopped {
                let left = results.iter().filter(|r| r.checks.first().is_some_and(|l| l.detail.starts_with("not run: "))).count();
                md += &format!("\n**Stopped early:** {}. {left} cases weren't run.\n", why.replace('|', "\\|"));
            }
            let _ = f.write_all(md.as_bytes());
        }
    }
    let line: Vec<String> = counts.iter().filter(|(_, n)| *n > 0).map(|(s, n)| format!("{n} {s}")).collect();
    println!("\n{player_name}, snapshot {snapshot}: {}", line.join(", "));
    if let Some(why) = &stopped {
        eprintln!("testsrc: stopped early: {why}");
        std::process::exit(2);
    }
    let bad: usize = counts.iter().filter(|(s, _)| matches!(*s, "fail" | "crash" | "hang")).map(|(_, n)| n).sum();
    std::process::exit(if bad > 0 { 1 } else { 0 });
}

#[allow(clippy::too_many_arguments)]
fn run_case(player: Player, command: Option<&str>, id: &str, path: &Path, facts: &Value, outdir: &Path, width: u32, capture: &str) -> CaseResult {
    let ex = &facts["expect"];
    let events_path = outdir.join("events").join(format!("{id}.ndjson"));
    std::fs::create_dir_all(events_path.parent().unwrap()).unwrap_or_else(|e| die(&e.to_string()));
    let _ = std::fs::remove_file(&events_path);
    let play_s = ex["duration_s"].as_f64().unwrap_or(10.0) * 2.0 + 20.0; // real time, with room for start-up
    let colour = &ex["colour"];
    let exe = std::env::current_exe().unwrap_or_else(|e| die(&e.to_string()));
    let mut cmd = Command::new(exe);
    cmd.env_remove("TESTSRC_TOKEN"); // the player is the code under test: it gets the file, not the token
    cmd.arg("_play").arg(player.name())
        .arg(path).arg(&events_path)
        .args(["--timeout", &play_s.to_string(), "--width", &width.to_string(), "--capture", capture])
        .args(["--matrix", colour["matrix"].as_str().unwrap_or("bt709"), "--range", colour["range"].as_str().unwrap_or("limited")]);
    if let (Some(w), Some(h)) = (facts["video"]["width"].as_u64(), facts["video"]["height"].as_u64()) {
        cmd.args(["--size", &format!("{w}x{h}")]);
    }
    if let Some(c) = command {
        cmd.args(["--command", c]);
    }
    let t0 = Instant::now();
    let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap_or_else(|e| die(&e.to_string()));
    let rc = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if t0.elapsed().as_secs_f64() > play_s + 60.0 => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break None,
        }
    };
    let wall = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let fail = |status, detail: String| CaseResult {
        id: id.into(), status, wall, player: None, checks: vec![Line { status: Status::Fail, what: "run".into(), detail }],
    };
    let Some(st) = rc else { return fail("hang", "player stopped responding".into()) };
    if !st.success() || !events_path.exists() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        let how = st.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into());
        return fail("crash", format!("player process ended with {how}: {}", testsrc_core::pyfmt::prefix(&last, 200)));
    }
    let events: Vec<Value> = std::fs::read_to_string(&events_path).unwrap_or_default().lines()
        .filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut checks = check(&events, ex);
    if player == Player::Cmd {
        // level 0 sees only the process: what it ran, not what it showed
        checks.retain(|l| l.what == "run" || l.what == "log");
    }
    let status = if checks.iter().any(|l| l.status == Status::Fail) { "fail" } else { "pass" };
    let player_name = events.iter().find(|e| e["t"] == "meta").and_then(|m| m["player"].as_str()).map(String::from);
    CaseResult { id: id.into(), status, wall, player: player_name, checks }
}
