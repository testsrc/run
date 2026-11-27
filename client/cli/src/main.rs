//! testsrc runner: fetch test media through the API and check how a player plays it.
//!
//!   export TESTSRC_TOKEN=tsk_...
//!   testsrc list   [--player libvlc] [CASE or PREFIX | starter ...]
//!   testsrc fetch  [--player libvlc] [CASE or PREFIX | starter ...]
//!   testsrc run    --player libvlc [CASE or PREFIX | starter ...] [--exclude duration/] [--junit report.xml]
//!                  [--lock testsrc.lock]
//!
//! Files are cached by snapshot and checked against their SHA-256; facts come from /v1/cases/{id}/facts.
//! Each case plays in its own process, so a player crash or hang is a result, not the end of the run.
//! When the API stops answering mid-run (an evaluation token's case limit, the daily quota), the run stops fetching
//! and still writes its results, with the cases left marked skipped, then exits 2.
//! --time-limit stops a run the same way, before the CI job's own limit kills it and its report with it.
//! results.json says what was tested: the commit and branch, the engine's version as the adapter read it, the
//! player's version when given (--player-version), the runner, OS and architecture.
//! The lock file's `expected` list (testsrc.lock, read when it exists) names cases expected to fail: they run and are
//! reported, and only an unexpected failure makes the exit code 1.
//! The player never sees TESTSRC_TOKEN: it is the code under test, and its logs may end up in public artifacts.

mod adapters;
mod api;
mod lock;
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

/// The first "player couldn't start" error of the run (an engine not installed), said once rather than per case
static SETUP: std::sync::OnceLock<String> = std::sync::OnceLock::new();

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
    List {
        #[command(flatten)]
        common: Common,
        /// With "starter": also the cases testsrc's own matrix found this engine getting wrong, as run would play
        #[arg(long, value_enum)]
        player: Option<Player>,
    },
    /// Download cases into the cache
    Fetch {
        #[command(flatten)]
        common: Common,
        /// With "starter": also the cases testsrc's own matrix found this engine getting wrong, as run would play
        #[arg(long, value_enum)]
        player: Option<Player>,
    },
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
        /// Stop after this many minutes, keeping what ran: the results so far, the cases left marked skipped, exit 2.
        /// Set it below the CI job's own limit, which ends the job without a report
        #[arg(long, env = "TESTSRC_TIME_LIMIT", value_name = "MINUTES")]
        time_limit: Option<f64>,
        /// The version of the player under test, as you name it (1.4.2, a build number): kept in results.json.
        /// The engine's own version (libvlc, FFmpeg, GStreamer, mpv) is read by the runner
        #[arg(long, env = "TESTSRC_PLAYER_VERSION")]
        player_version: Option<String>,
        /// The lock file whose `expected` list names cases expected to fail: they run and are reported, but their
        /// failure doesn't fail the run. Read when it exists
        #[arg(long, env = "TESTSRC_LOCK", default_value = "testsrc.lock")]
        lock: PathBuf,
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
            let mut events = events;
            adapters::tidy(player.name(), &mut events);
            let text: String = events.iter().map(|e| e.to_string() + "\n").collect();
            std::fs::write(&out, text).unwrap_or_else(|e| die(&format!("{}: {e}", out.display())));
        }
        Cmd::List { common: c, player } => {
            let (api, catalog, cases) = open(&c, player.map(Player::name));
            let snapshot = catalog["snapshot"].as_str().unwrap_or("");
            for c in &cases {
                println!("{:44} layer {}  {:8.1} MB  {}", c["id"].as_str().unwrap_or(""), c["layer"],
                         c["size_bytes"].as_f64().unwrap_or(0.0) / 1e6, c["outcome"].as_str().unwrap_or(""));
            }
            println!("{} cases in snapshot {snapshot}", cases.len());
            drop(api);
        }
        Cmd::Fetch { common: c, player } => {
            let (api, catalog, cases) = open(&c, player.map(Player::name));
            let cache = cache_dir(&c, &catalog);
            for (i, case) in cases.iter().enumerate() {
                api::fetch(&api, case, &cache).unwrap_or_else(|e| die(&e.to_string()));
                print!("\r{}/{} {:60}", i + 1, cases.len(), case["id"].as_str().unwrap_or(""));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            println!("\n{} cases in {}", cases.len(), cache.display());
        }
        Cmd::Run { common, player, command, out, junit, width, capture, retries, discard, time_limit, player_version, lock } => {
            let limit = time_limit.map(|m| {
                if !(m > 0.0 && m.is_finite()) {
                    die(&format!("--time-limit is in minutes and must be more than 0, not {m}"));
                }
                m
            });
            let player_version = player_version.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let expected = match std::fs::read_to_string(&lock) {
                Ok(t) => lock::expected(&t).unwrap_or_else(|e| die(&format!("{}, {e}", lock.display()))),
                Err(_) => vec![],
            };
            run(&common, player, command, &out, junit, width, &capture, retries, discard, limit, player_version, &expected)
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

/// What "starter" means: the catalog's sets.starter (the general cases, plus the ones our matrix run found this
/// engine getting wrong), or the built-in general list from an API that doesn't send sets; and a line saying which
fn starter(catalog: &Value, engine: Option<&str>) -> (Vec<String>, String) {
    let ids = |v: &Value| -> Option<Vec<String>> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()) };
    let set = &catalog["sets"]["starter"];
    let Some(mut list) = ids(&set["general"]) else {
        return (STARTER.iter().map(|s| s.to_string()).collect(), format!("starter: {} general cases", STARTER.len()));
    };
    let general = list.len();
    let extra = engine.and_then(|e| ids(&set["engines"][e])).unwrap_or_default();
    // the ones customers' runs often fail (pooled counts, also in engines), said apart from the matrix's
    let pooled = engine.and_then(|e| ids(&set["pooled"][e]["cases"])).unwrap_or_default();
    let pooled: Vec<&String> = extra.iter().filter(|id| pooled.contains(id)).collect();
    let matrix = extra.len() - pooled.len();
    let mut note = format!("starter: {general} general cases");
    if let (Some(e), true) = (engine, matrix > 0) {
        let from = &set["from"][e];
        let who = from["player"].as_str().unwrap_or(e).to_string();
        let when = from["date"].as_str().map(|d| format!(" of {d}")).unwrap_or_default();
        note += &format!(" and {matrix} that {who} got wrong in testsrc's own matrix run{when}");
    }
    if let (Some(e), false) = (engine, pooled.is_empty()) {
        note += &format!(" and {} that customers' runs of {e} often fail", pooled.len());
    }
    for id in extra {
        if !list.contains(&id) {
            list.push(id);
        }
    }
    (list, note)
}

fn open(c: &Common, engine: Option<&str>) -> (api::Api, Value, Vec<Value>) {
    let api = api::Api::new(&c.api, std::env::var("TESTSRC_TOKEN").ok().filter(|t| !t.is_empty()), c.snapshot.clone());
    let catalog = api.json("/catalog", false).unwrap_or_else(|e| die(&e.to_string()));
    let patterns: Vec<String> = c.cases.iter().flat_map(|p| {
        if p != "starter" { return vec![p.clone()]; }
        let (list, note) = starter(&catalog, engine);
        eprintln!("testsrc: {note}");
        list
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

/// "25", "1.5", "0.05": minutes as the user wrote them
fn minutes(m: f64) -> String {
    let s = format!("{m:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A command's first line of output, when it ran and succeeded
fn output_of(program: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(program).args(args).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
    String::from_utf8_lossy(&o.stdout).lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The commit under test: TESTSRC_COMMIT (the action passes a pull request's head, as GITHUB_SHA is then a merge
/// commit GitHub made), GITHUB_SHA, or the checkout's HEAD
fn commit() -> Option<String> {
    env_nonempty("TESTSRC_COMMIT").or_else(|| env_nonempty("GITHUB_SHA")).or_else(|| output_of("git", &["rev-parse", "HEAD"]))
}

/// The branch: a pull request's head branch, the pushed branch, or the checkout's (none when detached)
fn branch() -> Option<String> {
    env_nonempty("GITHUB_HEAD_REF").or_else(|| env_nonempty("GITHUB_REF_NAME"))
        .or_else(|| output_of("git", &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD"))
}

/// The engine's version as its adapter read it ("libvlc 3.0.21", "ffmpeg 7.1.5"): from the first case that got as
/// far, as the adapter names the bare engine when it couldn't load it. None for cmd players: they bring their own.
fn engine(player: Player, results: &[CaseResult]) -> Option<String> {
    if player == Player::Cmd {
        return None;
    }
    results.iter().filter_map(|r| r.player.clone()).find(|p| p != player.name())
}

/// One line on what was tested, for the console and the job summary
fn tested_line(doc: &Value) -> String {
    let mut parts = vec![];
    if let Some(c) = doc["commit"].as_str() {
        let short = &c[..c.len().min(12)];
        parts.push(match doc["branch"].as_str() { Some(b) => format!("commit {short} ({b})"), None => format!("commit {short}") });
    }
    if let Some(e) = doc["engine"].as_str() {
        parts.push(e.to_string());
    }
    if let Some(v) = doc["player_version"].as_str() {
        parts.push(format!("player version {v}"));
    }
    parts.push(format!("runner {} on {} {}", doc["runner"].as_str().unwrap_or(""), doc["os"].as_str().unwrap_or(""), doc["arch"].as_str().unwrap_or("")));
    format!("Tested: {}", parts.join(", "))
}

#[allow(clippy::too_many_arguments)]
fn run(c: &Common, player: Player, command: Option<String>, out: &Path, junit: Option<PathBuf>, width: u32, capture: &str, retries: u32, discard: bool,
       time_limit: Option<f64>, player_version: Option<String>, expected: &[String]) {
    if player == Player::Cmd && command.is_none() {
        die("--player cmd needs --command \"yourplayer --headless {file}\"");
    }
    let t_start = Instant::now();
    let deadline = time_limit.map(|m| t_start + Duration::from_secs_f64(m * 60.0));
    let over = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);
    let limit_reached = time_limit.map(|m| format!("the time limit of {} minutes was reached", minutes(m))).unwrap_or_default();
    let stop_note = |why: &str| eprintln!("testsrc: {}. Stopping here: the results so far are kept, the cases left are skipped.",
                                          why.trim_end_matches('.'));
    let (api, catalog, cases) = open(c, Some(player.name()));
    let snapshot = catalog["snapshot"].as_str().unwrap_or("").to_string();
    let cache = cache_dir(c, &catalog);
    // a line of the list that names no case of this snapshot: a typo, or a case renamed since
    let ids: Vec<&str> = catalog["cases"].as_array().into_iter().flatten().filter_map(|c| c["id"].as_str()).collect();
    for e in expected.iter().filter(|e| !ids.iter().any(|id| id.starts_with(e.as_str()))) {
        eprintln!("testsrc: {e}, on the lock file's expected list, is no case of snapshot {snapshot}");
    }
    std::fs::create_dir_all(out).unwrap_or_else(|e| die(&format!("{}: {e}", out.display())));
    let started = report::utc_now();
    let name = player.name();
    println!("testsrc runner {VERSION}: {} cases from snapshot {snapshot}, player {name}", cases.len());
    let mut results = vec![];
    let mut stopped: Option<String> = None;
    for (i, case) in cases.iter().enumerate() {
        let id = case["id"].as_str().unwrap_or("").to_string();
        if stopped.is_none() && over(deadline) {
            stop_note(&limit_reached);
            stopped = Some(limit_reached.clone());
        }
        let fetched = match &stopped {
            Some(_) => None,
            None => match api::fetch(&api, case, &cache) {
                Ok(p) => Some(p),
                Err(e) => {
                    stop_note(&e.to_string());
                    stopped = Some(e.to_string());
                    None
                }
            },
        };
        let not_run = |id: &str, why: &str| CaseResult { id: id.into(), status: "skipped", wall: 0.0, player: None, trace: Value::Null, expected: false,
                                                         checks: vec![Line { status: Status::Info, what: "run".into(), detail: format!("not run: {why}") }] };
        let Some(path) = fetched else {
            results.push(not_run(&id, stopped.as_deref().unwrap_or("")));
            println!("[{:3}/{}] {:7} {:44} {:6.1} s  not run", i + 1, cases.len(), "SKIPPED", id, 0.0);
            continue;
        };
        let facts: Value = serde_json::from_slice(&std::fs::read(format!("{}.facts.json", path.display())).unwrap_or_default()).unwrap_or_default();
        let r = if facts["video"].is_null() && facts["audio"].is_null() {
            CaseResult { id, status: "skipped", wall: 0.0, player: None, trace: Value::Null, expected: false,
                         checks: vec![Line { status: Status::Info, what: "run".into(), detail: "subtitle sidecar: not played on its own".into() }] }
        } else {
            let play = || run_case(player, command.as_deref(), &id, &path, &facts, out, width, capture, deadline);
            let Some(mut r) = play() else {
                // cut by the time limit while it played: not a hang of the player's
                stop_note(&limit_reached);
                stopped = Some(limit_reached.clone());
                results.push(not_run(&id, &format!("{limit_reached} while it played")));
                println!("[{:3}/{}] {:7} {:44} {:6.1} s  not run: stopped at the time limit", i + 1, cases.len(), "SKIPPED", id, 0.0);
                continue;
            };
            for attempt in 1..=retries {
                if r.status == "pass" || over(deadline) {
                    break;
                }
                // a retry cut by the time limit leaves the first run's result
                let Some(again) = play() else { break };
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
        let mut r = r;
        r.expected = r.status != "skipped" && lock::matches(expected, &r.id);
        let mut first = r.first_failure();
        if r.expected {
            first = if r.status == "pass" { "passes, though expected to fail".into() } else { format!("(expected) {first}") };
        }
        println!("[{:3}/{}] {:7} {:44} {:6.1} s  {}", i + 1, cases.len(), r.status.to_uppercase(), r.id, r.wall, testsrc_core::pyfmt::prefix(&first, 90));
        results.push(r);
    }
    let counts = report::counts(&results);
    let mut player_name = results.iter().find_map(|r| r.player.clone()).unwrap_or_else(|| name.into());
    if player == Player::Libvlc {
        player_name += if capture == "rgb" { " (RGB callbacks)" } else { " (YUV callbacks)" };
    }
    let doc = serde_json::json!({
        "runner": VERSION, "snapshot": snapshot, "player": player_name, "started": started, "finished": report::utc_now(),
        "stopped": stopped, "time_limit_min": time_limit, "setup_error": SETUP.get(),
        "engine": engine(player, &results), "player_version": player_version,
        "set": if c.cases.is_empty() { vec!["all".to_string()] } else { c.cases.clone() },
        // the choices kept in the repository (the workflow's exclude, the lock file's expected list), for the dashboard
        "exclude": c.exclude, "expected": expected,
        "commit": commit(), "branch": branch(), "repository": env_nonempty("GITHUB_REPOSITORY"),
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
            let mut md = report::github_summary(&results, &player_name, &snapshot, &tested_line(&doc));
            if let Some(m) = SETUP.get() {
                md += &format!("\n**The player didn't start:** {}\n", m.replace('|', "\\|"));
            }
            if let Some(why) = &stopped {
                let left = results.iter().filter(|r| r.checks.first().is_some_and(|l| l.detail.starts_with("not run: "))).count();
                md += &format!("\n**Stopped early:** {}. {left} cases weren't run.\n", why.replace('|', "\\|"));
            }
            let _ = f.write_all(md.as_bytes());
        }
    }
    let line: Vec<String> = counts.iter().filter(|(_, n)| *n > 0).map(|(s, n)| format!("{n} {s}")).collect();
    println!("\n{player_name}, snapshot {snapshot}: {}", line.join(", "));
    println!("{}", tested_line(&doc));
    if let Some(m) = SETUP.get() {
        eprintln!("testsrc: the player didn't start: {m}");
    }
    if let Some(why) = &stopped {
        eprintln!("testsrc: stopped early: {why}");
        std::process::exit(2);
    }
    let now_passing = report::now_passing(&results);
    if !now_passing.is_empty() {
        eprintln!("testsrc: {} on the lock file's expected list now pass{}: {}", now_passing.len(), if now_passing.len() == 1 { "es" } else { "" },
                  now_passing.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "));
    }
    std::process::exit(if report::broke(&results).next().is_some() { 1 } else { 0 });
}

/// The expectation the checker judges a run against: the fact sheet's `expect`, except that a file declaring no
/// duration of its own (`container.duration_s` null: no Duration element and no index, like a Matroska file
/// written live) leaves the player nothing to report but an estimate, so its reported duration isn't judged.
/// What it showed (every frame, the beeps) still is.
fn expectation(facts: &Value) -> Value {
    let mut ex = facts["expect"].clone();
    let declares_none = facts["container"].is_object() && facts["container"]["duration_s"].is_null();
    if declares_none && ex.is_object() {
        ex["duration_not_judged"] = Value::Bool(true);
    }
    ex
}

/// One case in its own process. None when the time limit (`deadline`) came while it played: the process is killed
/// and the case counts as not run.
#[allow(clippy::too_many_arguments)]
fn run_case(player: Player, command: Option<&str>, id: &str, path: &Path, facts: &Value, outdir: &Path, width: u32, capture: &str,
            deadline: Option<Instant>) -> Option<CaseResult> {
    let judged = expectation(facts);
    let ex = &judged;
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
            Ok(None) if deadline.is_some_and(|d| Instant::now() >= d) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
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
    // what the player process wrote to stderr. It has ended or been killed, but a process it started may still hold
    // the pipe: wait 2 s for the end of it at most
    let stderr_tail = |child: &mut std::process::Child| {
        let Some(mut e) = child.stderr.take() else { return vec![] };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut err = Vec::new();
            let _ = std::io::Read::read_to_end(&mut e, &mut err);
            let _ = tx.send(err);
        });
        let err = String::from_utf8_lossy(&rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()).into_owned();
        report::tail(err.lines().map(String::from).collect::<Vec<_>>().into_iter())
    };
    let fail = |status, detail: String, log: Vec<String>| CaseResult {
        id: id.into(), status, wall, player: None, checks: vec![Line { status: Status::Fail, what: "run".into(), detail }],
        trace: serde_json::json!({"log": log}), expected: false,
    };
    let Some(st) = rc else {
        let log = stderr_tail(&mut child);
        return Some(fail("hang", "player stopped responding".into(), log));
    };
    if !st.success() || !events_path.exists() {
        let log = stderr_tail(&mut child);
        let last = log.last().cloned().unwrap_or_default();
        let how = st.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into());
        return Some(fail("crash", format!("player process ended with {how}: {}", testsrc_core::pyfmt::prefix(&last, 200)), log));
    }
    let events: Vec<Value> = std::fs::read_to_string(&events_path).unwrap_or_default().lines()
        .filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    if let Some(m) = events.iter().find(|e| e["t"] == "log" && e["setup"] == true).and_then(|e| e["msg"].as_str()) {
        if SETUP.set(m.to_string()).is_ok() {
            eprintln!("testsrc: {m}");
        }
    }
    let mut checks = check(&events, ex);
    if player == Player::Cmd {
        // level 0 sees only the process: what it ran, not what it showed
        checks.retain(|l| l.what == "run" || l.what == "log");
    }
    let status = if checks.iter().any(|l| l.status == Status::Fail) { "fail" } else { "pass" };
    let player_name = events.iter().find(|e| e["t"] == "meta").and_then(|m| m["player"].as_str()).map(String::from);
    let trace = if status == "pass" { Value::Null } else { report::trace(&events) };
    Some(CaseResult { id: id.into(), status, wall, player: player_name, checks, trace, expected: false })
}

#[cfg(test)]
mod expectation_tests {
    use super::expectation;
    use serde_json::json;

    #[test]
    fn a_file_without_a_declared_duration_isnt_judged_on_it() {
        let f = json!({"container": {"format": "matroska,webm", "duration_s": null}, "expect": {"duration_s": 6, "frames": 180}});
        let ex = expectation(&f);
        assert_eq!(ex["duration_not_judged"], json!(true));
        assert_eq!(ex["frames"], json!(180), "everything else stays");
    }

    #[test]
    fn a_declared_duration_is_judged_as_before() {
        let f = json!({"container": {"duration_s": 6.0}, "expect": {"duration_s": 6}});
        assert!(expectation(&f).get("duration_not_judged").is_none());
        // a fact sheet without a container block (older snapshots) is left as it is
        let old = json!({"expect": {"duration_s": 6}});
        assert_eq!(expectation(&old), old["expect"]);
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use serde_json::json;

    fn played(player: Option<&str>) -> CaseResult {
        CaseResult { id: "a".into(), status: "pass", wall: 1.0, player: player.map(String::from), checks: vec![], trace: Value::Null, expected: false }
    }

    #[test]
    fn the_engine_is_the_first_version_an_adapter_read() {
        let r = [played(None), played(Some("libvlc")), played(Some("libvlc 3.0.21")), played(Some("libvlc 3.0.20"))];
        assert_eq!(engine(Player::Libvlc, &r).as_deref(), Some("libvlc 3.0.21"));
        assert_eq!(engine(Player::Libvlc, &r[..2]), None, "the bare name: the library didn't load");
        assert_eq!(engine(Player::Cmd, &[played(Some("cmd: ./p"))]), None);
    }

    #[test]
    fn minutes_as_written() {
        assert_eq!(minutes(25.0), "25");
        assert_eq!(minutes(1.5), "1.5");
        assert_eq!(minutes(0.03), "0.03");
    }

    #[test]
    fn the_tested_line_names_what_it_has() {
        let full = json!({"commit": "0123456789abcdef", "branch": "main", "engine": "ffmpeg 7.1.5", "player_version": "2.0",
                          "runner": "0.2.2", "os": "linux", "arch": "x86_64"});
        assert_eq!(tested_line(&full), "Tested: commit 0123456789ab (main), ffmpeg 7.1.5, player version 2.0, runner 0.2.2 on linux x86_64");
        let bare = json!({"commit": "abc", "branch": null, "engine": null, "player_version": null, "runner": "0.2.2", "os": "windows", "arch": "x86_64"});
        assert_eq!(tested_line(&bare), "Tested: commit abc, runner 0.2.2 on windows x86_64");
    }
}

#[cfg(test)]
mod starter_tests {
    use super::{starter, STARTER};
    use serde_json::json;

    #[test]
    fn the_served_set_adds_what_the_engine_got_wrong() {
        let cat = json!({"sets": {"starter": {
            "general": ["codec/h264-aac.mp4", "fps/60.mp4"],
            "engines": {"libvlc": ["edge/a.mp4", "fps/60.mp4"], "ffmpeg": []},
            "from": {"libvlc": {"player": "libvlc 3.0.24 Vetinari", "date": "2026-11-03"}},
        }}});
        let (list, note) = starter(&cat, Some("libvlc"));
        assert_eq!(list, ["codec/h264-aac.mp4", "fps/60.mp4", "edge/a.mp4"], "once each");
        assert_eq!(note, "starter: 2 general cases and 2 that libvlc 3.0.24 Vetinari got wrong in testsrc's own matrix run of 2026-11-03");
        for engine in [Some("ffmpeg"), Some("cmd"), None] {
            let (list, note) = starter(&cat, engine);
            assert_eq!(list, ["codec/h264-aac.mp4", "fps/60.mp4"], "{engine:?}");
            assert_eq!(note, "starter: 2 general cases");
        }
        // an engine listed without a "from": its name
        let cat = json!({"sets": {"starter": {"general": [], "engines": {"mpv": ["edge/b.mkv"]}}}});
        assert_eq!(starter(&cat, Some("mpv")).1, "starter: 0 general cases and 1 that mpv got wrong in testsrc's own matrix run");
        // pooled counts (phase 8e): in engines too, said apart
        let cat = json!({"sets": {"starter": {"general": ["codec/a.mp4"], "engines": {"ffmpeg": ["edge/a.mp4", "edge/b.mkv"]},
            "pooled": {"ffmpeg": {"cases": ["edge/b.mkv", "edge/gone.mp4"]}}}}});
        let (list, note) = starter(&cat, Some("ffmpeg"));
        assert_eq!(list, ["codec/a.mp4", "edge/a.mp4", "edge/b.mkv"]);
        assert_eq!(note, "starter: 1 general cases and 1 that ffmpeg got wrong in testsrc's own matrix run and 1 that customers' runs of ffmpeg often fail");
        let cat = json!({"sets": {"starter": {"general": [], "engines": {"mpv": ["edge/b.mkv"]}, "pooled": {"mpv": {"cases": ["edge/b.mkv"]}}}}});
        assert_eq!(starter(&cat, Some("mpv")).1, "starter: 0 general cases and 1 that customers' runs of mpv often fail");
    }

    #[test]
    fn an_api_without_sets_gives_the_built_in_list() {
        let (list, note) = starter(&json!({"cases": []}), Some("libvlc"));
        assert_eq!(list.len(), STARTER.len());
        assert_eq!(note, format!("starter: {} general cases", STARTER.len()));
    }
}
