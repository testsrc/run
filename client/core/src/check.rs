//! Compares what a player showed (adapter events) with a case's expected outcome (`facts.expect`).
//!
//! Events, one JSON object each, as adapters write them:
//!   {"t":"meta","player":..,"duration":..,"sar":"40:33"?}
//!   {"t":"frame","pts":..,"code":..,"orient":..,"box":[x,y,w,h],"rgb":[[r,g,b] x22]?}
//!   {"t":"audio","start":..,"beeps":[..]}     {"t":"seek","target":..,"code":..}
//!   {"t":"log","level":"error","msg":..}      {"t":"exit","code":..,"wall":..}
//!
//! Works on JSON values with the reference implementation's loose typing (an exit code may be a number
//! or "timeout", a pts may be null), so the same events always give the same lines, word for word.

use serde_json::Value;

use crate::code::Orientation;
use crate::colour::{diagnose_colour, Matrix, Range};
use crate::pyfmt::{self, py_str};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Info,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Info => "INFO",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub status: Status,
    pub what: String,
    pub detail: String,
}

struct Out(Vec<Line>);

impl Out {
    fn add(&mut self, ok: bool, what: &str, detail: String, warn: bool) {
        let status = if ok { Status::Pass } else if warn { Status::Warn } else { Status::Fail };
        self.0.push(Line { status, what: what.into(), detail });
    }
    fn info(&mut self, what: &str, detail: String) {
        self.0.push(Line { status: Status::Info, what: what.into(), detail });
    }
}

fn kind(e: &Value) -> &str {
    e.get("t").and_then(Value::as_str).unwrap_or("")
}

/// A key that is present and not null.
fn some<'a>(e: &'a Value, k: &str) -> Option<&'a Value> {
    e.get(k).filter(|v| !v.is_null())
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| num(v) as i64)
}

/// Python's `==` between a JSON value and an integer.
fn eq_int(v: &Value, n: i64) -> bool {
    match v {
        Value::Number(x) => x.as_f64() == Some(n as f64),
        Value::Bool(b) => *b as i64 == n,
        _ => false,
    }
}

/// The value seen most often; on a tie, the one seen first (Python's Counter.most_common(1)).
fn most_common<T: PartialEq + Clone>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut counts: Vec<(T, usize)> = vec![];
    for it in items {
        match counts.iter_mut().find(|(k, _)| *k == it) {
            Some((_, n)) => *n += 1,
            None => counts.push((it, 1)),
        }
    }
    let mut best: Option<(T, usize)> = None;
    for (k, n) in counts {
        if best.as_ref().map_or(true, |(_, b)| n > *b) {
            best = Some((k, n));
        }
    }
    best.map(|(k, _)| k)
}

/// The upper median, as `sorted(xs)[len(xs) // 2]`.
fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs[xs.len() / 2]
}

/// First item with the largest absolute value (Python's `max(xs, key=abs)`).
fn max_abs(xs: &[f64]) -> f64 {
    let mut best = xs[0];
    for &x in &xs[1..] {
        if x.abs() > best.abs() {
            best = x;
        }
    }
    best
}

/// "1-3, 7, 9-10", the first six runs and " ..." after them.
pub fn ranges(xs: &[i64]) -> String {
    let mut out: Vec<String> = vec![];
    let mut start: Option<i64> = None;
    for (i, &x) in xs.iter().enumerate() {
        let s = *start.get_or_insert(x);
        if i + 1 == xs.len() || xs[i + 1] != x + 1 {
            out.push(if s == x { s.to_string() } else { format!("{s}-{x}") });
            start = None;
        }
    }
    let more = if out.len() > 6 { " ..." } else { "" };
    out.truncate(6);
    out.join(", ") + more
}

/// pts_jumps: [[code, seconds]]: the file's timestamps jump by that much at that frame and playback goes on. A
/// player may report the jump or play on with a continuous clock: frames reported after the jump are moved back.
fn undo_jumps(events: &[Value], ex: &Value) -> Option<Vec<Value>> {
    let jumps = ex.get("pts_jumps").and_then(Value::as_array).filter(|j| !j.is_empty())?;
    let mut out: Vec<Value> = events.to_vec();
    for j in jumps {
        let (code, jump) = (int(&j[0]), num(&j[1]));
        let is_frame = |e: &Value| kind(e) == "frame" && some(e, "code").is_some() && some(e, "pts").is_some();
        let last = out.iter().filter(|e| is_frame(e) && int(&e["code"]) < code)
            .map(|e| num(&e["pts"])).fold(f64::NEG_INFINITY, f64::max);
        if !last.is_finite() {
            continue;
        }
        for e in out.iter_mut() {
            if is_frame(e) && int(&e["code"]) >= code && num(&e["pts"]) >= last + jump / 2.0 {
                let p = num(&e["pts"]) - jump;
                e["pts"] = serde_json::json!(p);
            }
        }
    }
    Some(out)
}

/// A file a player may refuse (outcome may-refuse): the player must end, without a crash (a negative exit, killed by
/// a signal) or a hang; that is all that can fail. Showing nothing, or every frame in order, passes; showing part of
/// it (or out of order) passes with a warning (WARN), which the runner reports. Nothing else is judged.
fn check_refusal(events: &[Value], ex: &Value) -> Vec<Line> {
    let mut res = Out(vec![]);
    let exit_ev = events.iter().find(|e| kind(e) == "exit");
    let codes: Vec<i64> = events.iter()
        .filter(|e| kind(e) == "frame" && some(e, "code").is_some())
        .map(|e| int(&e["code"]))
        .collect();
    match exit_ev {
        None => res.add(false, "run", "no exit event: crash or hang".into(), false),
        Some(x) => {
            let code = x.get("code").unwrap_or(&Value::Null);
            let wall = x.get("wall").map(py_str).unwrap_or_else(|| "?".into());
            let ended = code.as_i64().is_some_and(|c| c >= 0);
            let tail = if ended { "" } else { ": a crash or a hang" };
            res.add(ended, "run", format!("exit code {}, {wall} s{tail}", py_str(code)), false);
        }
    }
    if codes.is_empty() {
        res.add(true, "frames", "refused: no frame shown".into(), false);
        return res.0;
    }
    let want: Vec<i64> = match ex.get("codes").and_then(Value::as_array) {
        Some(rs) => rs.iter().flat_map(|r| int(&r[0])..=int(&r[1])).collect(),
        None => (int(&ex["first_code"])..int(&ex["first_code"]) + int(&ex["frames"])).collect(),
    };
    let seen: std::collections::HashSet<i64> = codes.iter().copied().collect();
    let wanted: std::collections::HashSet<i64> = want.iter().copied().collect();
    let missing: Vec<i64> = want.iter().copied().filter(|c| !seen.contains(c)).collect();
    let mut first_seen = std::collections::HashSet::new();
    let firsts: Vec<i64> = codes.iter().copied().filter(|c| wanted.contains(c) && first_seen.insert(*c)).collect();
    let backwards = firsts.windows(2).filter(|w| w[1] < w[0]).count();
    let played = seen.intersection(&wanted).count();
    if missing.is_empty() && backwards == 0 {
        res.add(true, "frames", format!("played: {played}/{} frames shown", want.len()), false);
        return res.0;
    }
    // part of it, or out of order: a pass, with a warning the runner reports (decided 12 November 2026)
    let ended = res.0.first().is_some_and(|l| l.status == Status::Pass);
    let mut detail = format!("partial playback{}: {played}/{} frames played", if ended { ", ended cleanly" } else { "" }, want.len());
    if !missing.is_empty() {
        detail += &format!(", missing {}", ranges(&missing));
    }
    if backwards > 0 {
        detail += &format!(", {backwards} out of order");
    }
    res.add(false, "frames", detail, true);
    res.0
}

pub fn check(events: &[Value], expect: &Value) -> Vec<Line> {
    let ex = expect;
    if ex.get("outcome").and_then(Value::as_str) == Some("may-refuse") {
        return check_refusal(events, ex);
    }
    let undone = undo_jumps(events, ex);
    let events: &[Value] = undone.as_deref().unwrap_or(events);
    let mut res = Out(vec![]);
    let empty = Value::Object(Default::default());
    let meta = events.iter().find(|e| kind(e) == "meta").unwrap_or(&empty);
    let exit_ev = events.iter().find(|e| kind(e) == "exit");
    let frames: Vec<&Value> = events.iter().filter(|e| kind(e) == "frame").collect();
    let read: Vec<&Value> = frames.iter().copied().filter(|e| some(e, "code").is_some()).collect();

    match exit_ev {
        None => res.add(false, "run", "no exit event: crash or hang".into(), false),
        Some(x) => {
            let code = x.get("code").unwrap_or(&Value::Null);
            let wall = x.get("wall").map(py_str).unwrap_or_else(|| "?".into());
            // a damaged file may end with an error; a crash (killed by a signal: negative) or a hang may not
            let damaged_end = ex.get("damaged").and_then(Value::as_bool) == Some(true) && code.as_i64().is_some_and(|c| c > 0);
            res.add(eq_int(code, 0) || damaged_end, "run", format!("exit code {}, {wall} s", py_str(code)), false);
        }
    }
    let errs: Vec<&Value> = events.iter()
        .filter(|e| kind(e) == "log" && e.get("level").and_then(Value::as_str) == Some("error"))
        .collect();
    let max_errs = ex.get("max_log_errors").map(int).unwrap_or(0);
    let first = errs.first().map(|e| format!(", first: {}", pyfmt::prefix(&py_str(&e["msg"]), 100))).unwrap_or_default();
    res.add(errs.len() as i64 <= max_errs, "log", format!("{} error lines{first}", errs.len()), true);

    // (a file whose timestamps jump declares the whole span: duration_not_judged)
    if let Some(d) = some(meta, "duration").map(num).filter(|_| ex.get("duration_not_judged").and_then(Value::as_bool) != Some(true)) {
        res.add((d - num(&ex["duration_s"])).abs() <= num(&ex["duration_tol_s"]), "duration",
                format!("player reports {d:.3} s, expected {} +/- {}", py_str(&ex["duration_s"]), py_str(&ex["duration_tol_s"])),
                false);
    }

    let audio = events.iter().find(|e| kind(e) == "audio");
    if ex.get("frames").is_none() {
        // audio only: a beep at the start of every second, spaced one second apart
        let Some(audio) = audio else {
            res.add(false, "audio", "no audio played".into(), false);
            return res.0;
        };
        let beeps: Vec<f64> = audio["beeps"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default();
        let want = num(&ex["duration_s"]).ceil() as i64; // one at the start of every started second
        let tol = num(&ex["av_tol_ms"]);
        // as for video, the first second is start-up and reported apart: the grid comes from beeps 1 and on
        let steady = beeps.get(1..).unwrap_or(&[]);
        let base = if steady.is_empty() { 0.0 } else {
            median(steady.iter().enumerate().map(|(k, b)| b - (k + 1) as f64).collect())
        };
        let worst = steady.iter().enumerate().map(|(k, b)| (b - base - (k + 1) as f64).abs()).fold(0.0f64, |a, b| if b > a { b } else { a });
        res.add(beeps.len() as i64 == want && worst * 1000.0 <= tol, "beeps",
                format!("{} beeps, {want} expected, worst {:.0} ms off the one-second grid (after 1 s)", beeps.len(), worst * 1000.0),
                false);
        if !steady.is_empty() && (beeps[0] - base).abs() * 1000.0 > tol {
            res.info("start-up", format!("first beep {:+.0} ms off the grid", (beeps[0] - base) * 1000.0));
        }
        return res.0;
    }

    // frames
    let codes: Vec<i64> = read.iter().map(|e| int(&e["code"])).collect();
    let first_code = int(&ex["first_code"]);
    let want: Vec<i64> = match ex.get("codes").and_then(Value::as_array) {
        // the frames a correct player shows, as [first, last] ranges (missing frames, hidden lead-in)
        Some(rs) => rs.iter().flat_map(|r| int(&r[0])..=int(&r[1])).collect(),
        None => (first_code..first_code + int(&ex["frames"])).collect(),
    };
    let seen: std::collections::HashSet<i64> = codes.iter().copied().collect();
    let missing: Vec<i64> = want.iter().copied().filter(|c| !seen.contains(c)).collect();
    let dups = codes.windows(2).filter(|w| w[0] == w[1]).count();
    let mut backwards = codes.windows(2).filter(|w| w[1] < w[0]).count();
    if ex.get("damaged").and_then(Value::as_bool) == Some(true) {
        // an earlier picture shown again is concealment, and frames inside a lost stretch aren't judged: order is
        // judged on the first appearance of each expected frame
        let wanted: std::collections::HashSet<i64> = want.iter().copied().collect();
        let mut seen_first = std::collections::HashSet::new();
        let firsts: Vec<i64> = codes.iter().copied().filter(|c| wanted.contains(c) && seen_first.insert(*c)).collect();
        backwards = firsts.windows(2).filter(|w| w[1] < w[0]).count();
    }
    let unread = frames.len() - read.len();
    let mut detail = format!("{}/{} frames shown", seen.len(), want.len());
    if !missing.is_empty() {
        detail += &format!(", missing {}", ranges(&missing));
    }
    if dups > 0 {
        detail += &format!(", {dups} repeated");
    }
    if backwards > 0 {
        detail += &format!(", {backwards} out of order");
    }
    if unread > 0 {
        detail += &format!(", {unread} unreadable");
    }
    // a damaged file: concealed (unreadable) and repeated frames are what players do with it
    let damaged = ex.get("damaged").and_then(Value::as_bool) == Some(true);
    let frames_ok = missing.is_empty() && backwards == 0 && (damaged || (dups == 0 && unread == 0));
    res.add(frames_ok, "frames", detail, false);

    if !read.is_empty() {
        let orient = most_common(read.iter().map(|e| py_str(&e["orient"]))).unwrap();
        res.add(orient == py_str(&ex["orientation"]), "orientation",
                format!("{orient}, expected {}", py_str(&ex["orientation"])), false);
        let turned = Orientation::from_name(&orient).is_some_and(Orientation::turned);
        // the picture's size as shown: the most common box, the pixel aspect applied, turned back upright
        let shown = |fs: &[&Value]| -> (i64, i64) {
            let (mut w, h) = most_common(fs.iter().map(|e| (int(&e["box"][2]), int(&e["box"][3])))).unwrap();
            if let Some(sar) = some(meta, "sar").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                // adapter captured storage-size frames and reports the pixel aspect separately
                let (sn, sd) = split_ratio(sar);
                w = pyfmt::round((w * sn) as f64 / sd as f64);
            }
            if turned { (h, w) } else { (w, h) }
        };
        let judge = |w: i64, h: i64, aspect: &str| -> (bool, String) {
            let (n, d) = split_ratio(aspect);
            let want_ratio = n as f64 / d as f64;
            let got = w as f64 / h as f64;
            ((got / want_ratio - 1.0).abs() < 0.02, format!("picture {w}x{h} = {got:.3}, expected {aspect} = {want_ratio:.3}"))
        };
        let (bw, bh) = shown(&read);
        if let Some(segs) = ex.get("aspects").and_then(Value::as_array) {
            // the shown aspect changes mid-stream: [first, last, "16:9"] per range of frames
            let (mut ok, mut parts) = (true, vec![]);
            for sg in segs {
                let (a, b, aspect) = (int(&sg[0]), int(&sg[1]), py_str(&sg[2]));
                let seg: Vec<&Value> = read.iter().copied().filter(|e| (a..=b).contains(&int(&e["code"]))).collect();
                if seg.is_empty() {
                    ok = false;
                    parts.push(format!("frames {a}-{b}: none shown"));
                    continue;
                }
                let (w, h) = shown(&seg);
                let (good, d) = judge(w, h, &aspect);
                ok = ok && good;
                parts.push(format!("frames {a}-{b}: {d}"));
            }
            res.add(ok, "aspect", parts.join("; "), false);
        } else {
            let (good, d) = judge(bw, bh, &py_str(&ex["display_aspect"]));
            res.add(good, "aspect", d, false);
        }
        let bx = &read[0]["box"];
        if num(&bx[0]) != 0.0 || num(&bx[1]) != 0.0 {
            res.info("framing", format!("picture found at {} inside the capture (letterbox/pillarbox)", py_str(bx)));
        }

        // timestamps (not for a damaged file: it's judged on the frames it shows; timing is its clean case's job)
        let mut timed: Vec<&Value> = read.iter().copied().filter(|e| some(e, "pts").is_some() && !damaged).collect();
        let fps = num(&ex["fps"]);
        // when each shown frame is due, in seconds: the regular grid, or the file's own times (variable frame rate)
        let times: Option<std::collections::HashMap<i64, f64>> = ex.get("frame_times").and_then(Value::as_array)
            .map(|ts| want.iter().copied().zip(ts.iter().map(num)).collect());
        if let Some(t) = &times {
            timed.retain(|e| t.contains_key(&int(&e["code"])));
        }
        let due = |e: &Value| match &times {
            Some(t) => t[&int(&e["code"])],
            None => int(&e["code"]) as f64 / fps,
        };
        let starting = |e: &Value| match &times {
            Some(t) => t[&int(&e["code"])] < 1.0,
            None => (int(&e["code"]) as f64) < fps,
        };
        let grid = if times.is_some() { "frame times" } else { "frame grid" };
        let steady: Vec<&Value> = timed.iter().copied().filter(|e| !starting(e)).collect();
        if steady.len() > 1 {
            // the grid's origin is the median offset, so one late frame can't shift every other frame off it
            let base = median(steady.iter().map(|e| num(&e["pts"]) - due(e)).collect());
            let devs: Vec<f64> = steady.iter().map(|e| (num(&e["pts"]) - base - due(e)).abs()).collect();
            let dev = devs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let late = devs.iter().filter(|&&d| d > 0.5 / fps).count();
            let mut detail = format!("max deviation from {grid} {:.1} ms (after 1 s)", dev * 1000.0);
            if late > 0 {
                detail += &format!(", {late} of {} frames off by more than half a frame", devs.len());
            }
            // a real-time adapter's clock sees the odd late frame from the machine, not the player
            res.add(dev <= 0.5 / fps, "timestamps", detail, late <= devs.len() / 100);
            let early: Vec<f64> = timed.iter().filter(|e| starting(e)).map(|e| num(&e["pts"]) - base - due(e)).collect();
            if !early.is_empty() && early.iter().map(|x| x.abs()).fold(f64::NEG_INFINITY, f64::max) > 0.5 / fps {
                res.info("start-up", format!("first second off the {grid} by up to {:+.0} ms (frame {} first)",
                                             max_abs(&early) * 1000.0, py_str(&timed[0]["code"])));
            }
        }

        // colour: median of each channel over the sampled frames
        let samples: Vec<&Value> = read.iter().filter_map(|e| e.get("rgb")).collect();
        let colour = &ex["colour"];
        if !samples.is_empty() && colour["matrix"].is_null() {
            res.info("colour", "not judged: the patches can't be compared in this colour space (BT.2020)".into());
        } else if !samples.is_empty() && (bw as f64 / 22.0).min(bh as f64 / 18.0) < 8.0 {
            res.info("colour", format!("not judged: picture too small ({bw}x{bh}) for reliable patches"));
        } else if !samples.is_empty() {
            let npatch = samples[0].as_array().map_or(0, Vec::len);
            let med: Vec<[i64; 3]> = (0..npatch).map(|i| {
                std::array::from_fn(|c| {
                    let mut v: Vec<i64> = samples.iter().map(|s| int(&s[i][c])).collect();
                    v.sort();
                    v[v.len() / 2]
                })
            }).collect();
            let matrix = Matrix::from_name(colour["matrix"].as_str().unwrap_or("")).unwrap_or(Matrix::Bt709);
            let range = Range::from_name(colour["range"].as_str().unwrap_or("")).unwrap_or(Range::Limited);
            let ranked = diagnose_colour(&med, matrix, range);
            let (err, best) = (&ranked[0].0, &ranked[0].1);
            let tol = colour.get("tol").map(num).unwrap_or(8.0);
            let ok = best == "correct" && *err <= tol;
            let corr = ranked.iter().find(|(_, n)| n == "correct").map(|(e, _)| *e).unwrap_or(f64::NAN);
            let mut detail = format!("best match: {best} (mean error {})", pyfmt::repr(*err));
            if best != "correct" {
                detail += &format!("; correct decode would be {}", pyfmt::repr(corr));
            }
            res.add(ok, "colour", detail, false);
        }
    }

    if let (Some(audio), false) = (audio, read.is_empty()) {
        if ex.get("beep_every_frames").is_none() {
            res.info("audio", "the player played audio, but the file has none".into());
        } else if damaged {
            res.info("a/v sync", "not judged: a damaged file".into());
        } else if !want.iter().any(|&c| c != 0 && c.rem_euclid(int(&ex["beep_every_frames"])) == 0) {
            res.info("a/v sync", "not judged: the file has no frame on a beep after the first one".into());
        } else {
            let beeps: Vec<f64> = audio["beeps"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default();
            let every = int(&ex["beep_every_frames"]);
            let (want_off, tol) = (num(&ex["av_offset_ms"]), num(&ex["av_tol_ms"]));
            // {code: pts}: a repeated code keeps its last time
            let mut ptsmap = std::collections::BTreeMap::new();
            for e in read.iter().filter(|e| some(e, "pts").is_some()) {
                ptsmap.insert(int(&e["code"]), num(&e["pts"]));
            }
            let (mut offs, mut first) = (vec![], None);
            // each beep is matched around where it should be (frame time plus the expected offset): with one beep
            // a second, an expected offset of 500 ms sits halfway between two beeps when matched around the frame time
            let at = want_off / 1000.0;
            for (&c, &pts) in &ptsmap {
                if c.rem_euclid(every) == 0 && !beeps.is_empty() {
                    let mut nearest = beeps[0];
                    for &b in &beeps[1..] {
                        if (b - pts - at).abs() < (nearest - pts - at).abs() {
                            nearest = b;
                        }
                    }
                    if c == 0 {
                        first = Some(nearest - pts);
                    } else if (nearest - pts - at).abs() < 0.5 {
                        offs.push(nearest - pts);
                    }
                }
            }
            if let Some(f) = first.filter(|f| (f * 1000.0 - want_off).abs() > tol) {
                res.info("start-up", format!("first beep {:+.0} ms against frame 0", f * 1000.0));
            }
            if offs.is_empty() {
                res.add(false, "a/v sync", format!("no beeps matched ({} beeps found)", beeps.len()), false);
            } else {
                let mean = pyfmt::sum(offs.iter().copied()) / offs.len() as f64 * 1000.0;
                let worst = max_abs(&offs) * 1000.0;
                res.add((worst - want_off).abs() <= tol, "a/v sync",
                        format!("audio vs video: mean {mean:+.0} ms, worst {worst:+.0} ms over {} beeps after 1 s (+ = audio late)", offs.len()),
                        false);
            }
        }
    }

    let fps = num(&ex["fps"]);
    let exact = ex.get("exact_seek").and_then(Value::as_bool).unwrap_or(false);
    let frame_times: Option<Vec<f64>> = ex.get("frame_times").and_then(Value::as_array).map(|ts| ts.iter().map(num).collect());
    for s in events.iter().filter(|e| kind(e) == "seek") {
        let target = num(&s["target"]);
        let want_code = match &frame_times {
            // the last frame due at or before the target
            Some(ts) => want.iter().zip(ts).filter(|(_, &t)| t <= target).map(|(c, _)| *c).last().unwrap_or(want[0]),
            None => first_code + pyfmt::round(target * fps),
        };
        let code = s.get("code").unwrap_or(&Value::Null);
        res.add(eq_int(code, want_code), "seek",
                format!("seek to {} s showed frame {}, expected {want_code}", py_str(&s["target"]), py_str(code)), !exact);
    }
    res.0
}

fn split_ratio(s: &str) -> (i64, i64) {
    let mut it = s.split(':').map(|p| p.trim().parse::<i64>().unwrap_or(1));
    (it.next().unwrap_or(1), it.next().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_like_the_reference() {
        assert_eq!(ranges(&[1]), "1");
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), "1-3, 7, 9-10");
        assert_eq!(ranges(&[1, 3, 5, 7, 9, 11, 13, 15]), "1, 3, 5, 7, 9, 11 ...");
    }

    #[test]
    fn most_common_prefers_the_first_on_ties() {
        assert_eq!(most_common(["b", "a", "a", "b"]), Some("b"));
        assert_eq!(most_common(["b", "a", "a"]), Some("a"));
    }
}
