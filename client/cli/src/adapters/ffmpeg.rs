//! ffmpeg as the player: decodes as fast as it can, timestamps from the file (showinfo). The control the
//! generator verified every file with. Same behaviour as the reference adapter (testsrc_check.ffmpeg_adapter).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

use regex::Regex;
use serde_json::{json, Value};
use testsrc_core::{find_beeps, pyfmt, Reader, RgbFrame};

const RATE: u32 = 44100;

pub fn play(path: &str, width: u32) -> Vec<Value> {
    play_with("ffmpeg", "ffprobe", path, width)
}

/// `play` with the two programs named, so a test can put fakes in their place.
fn play_with(ffmpeg: &str, ffprobe: &str, path: &str, width: u32) -> Vec<Value> {
    let t0 = Instant::now();
    let mut events = vec![];
    // either program missing: one message saying what to install, not a failure further on
    if let Err(e) = Command::new(ffmpeg).arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
        return super::not_installed("ffmpeg", "ffmpeg", &format!("can't run {ffmpeg}: {e}"));
    }
    let probed = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type:stream_disposition=attached_pic", "-of", "json", path])
        .output();
    let probe = match probed {
        Ok(o) => serde_json::from_slice::<Value>(&o.stdout).unwrap_or(json!({})),
        Err(e) => return super::not_installed("ffmpeg", "ffmpeg", &format!("can't run {ffprobe}: {e}")),
    };
    // an attached picture (cover art) is a video stream to ffprobe, but not video to play
    let types: Vec<&str> = probe["streams"].as_array().into_iter().flatten()
        .filter(|s| s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0)
        .filter_map(|s| s["codec_type"].as_str()).collect();
    // none for a file that doesn't declare one: unknown, not 0
    let duration = probe["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok());
    events.push(json!({"t": "meta", "player": version(ffmpeg), "duration": duration}));
    let has_audio = types.contains(&"audio");

    if !types.contains(&"video") {
        // audio only: just the beeps
        let o = Command::new(ffmpeg)
            .args(["-v", "error", "-i", path, "-map", "0:a:0", "-ac", "1", "-ar", "44100", "-f", "s16le", "-"])
            .output();
        let (code, samples, stderr) = match o {
            Ok(o) => (o.status.code().unwrap_or(-1), s16(&o.stdout), String::from_utf8_lossy(&o.stderr).into_owned()),
            Err(e) => (-1, vec![], e.to_string()),
        };
        events.push(json!({"t": "audio", "start": 0.0, "beeps": find_beeps(&samples, RATE, 0.0)}));
        events.extend(stderr.lines().filter(|l| !l.trim().is_empty()).map(|l| json!({"t": "log", "level": "error", "msg": l})));
        events.push(json!({"t": "exit", "code": code, "wall": pyfmt::round_to(t0.elapsed().as_secs_f64(), 2)}));
        return events;
    }

    // what a player does: apply SAR for display, convert to RGB
    let chain = format!("scale={width}:-2,scale=iw*sar:ih,setsar=1,format=rgb24");
    let first = Command::new(ffmpeg)
        .args(["-v", "info", "-i", path, "-vf", &format!("{chain},showinfo"), "-frames:v", "1", "-f", "null", "-"])
        .output().map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();
    let size = Regex::new(r" s:(\d+)x(\d+)").unwrap();
    let Some(c) = size.captures(&first) else {
        events.push(json!({"t": "log", "level": "error", "msg": "ffmpeg found no picture size"}));
        events.push(json!({"t": "exit", "code": 1, "wall": 0.0}));
        return events;
    };
    let (w, h): (usize, usize) = (c[1].parse().unwrap(), c[2].parse().unwrap());

    let afile = std::env::temp_dir().join(format!("testsrc-a-{}.s16", std::process::id()));
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-loglevel", "level+info", "-nostats", "-copyts", "-i", path, "-map", "0:v:0", "-vf", &format!("{chain},showinfo"),
              "-fps_mode", "passthrough", "-f", "rawvideo", "pipe:1"]);
    if has_audio {
        cmd.args(["-map", "0:a:0", "-af", "ashowinfo", "-ac", "1", "-ar", "44100", "-f", "s16le", "-y"]).arg(&afile);
    }
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            events.push(json!({"t": "log", "level": "error", "setup": true,
                               "msg": format!("can't run ffmpeg: {e}. {}", super::install_hint("ffmpeg"))}));
            events.push(json!({"t": "exit", "code": 1, "wall": 0.0}));
            return events;
        }
    };
    // keyed by frame index: ffmpeg drops the odd showinfo/ashowinfo line, so position in the log can't be trusted
    let stderr = child.stderr.take().unwrap();
    let drain = std::thread::spawn(move || {
        let (mut vpts, mut aframes, mut logs) = (HashMap::new(), HashMap::new(), vec![]);
        for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            let any = frame_info(&line, &mut vpts, &mut aframes);
            if any {
                continue;
            }
            if line.contains("[error]") || line.contains("[fatal]") {
                logs.push(json!({"t": "log", "level": "error", "msg": line}));
            } else if line.contains("[warning]") {
                logs.push(json!({"t": "log", "level": "warning", "msg": line}));
            }
        }
        (vpts, aframes, logs)
    });
    let mut out = child.stdout.take().unwrap();
    let (mut reader, mut frames) = (Reader::new(), vec![]);
    let mut buf = vec![0u8; w * h * 3];
    while out.read_exact(&mut buf).is_ok() {
        let n = frames.len();
        frames.push(reader.read(&RgbFrame::new(&buf, w, h), n % 25 == 0));
    }
    let status = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    let (vpts, aframes, logs) = drain.join().unwrap_or_default();
    for (i, r) in frames.iter().enumerate() {
        let mut e = json!({"t": "frame", "pts": vpts.get(&i)});
        e.as_object_mut().unwrap().extend(r.to_json().as_object().unwrap().clone());
        events.push(e);
    }
    if has_audio {
        if let Ok(raw) = std::fs::read(&afile) {
            let start = audio_start(&aframes, || first_audio_pts(ffprobe, path));
            events.push(json!({"t": "audio", "start": start, "beeps": find_beeps(&s16(&raw), RATE, start)}));
        }
        let _ = std::fs::remove_file(&afile);
    }
    events.extend(logs);
    events.push(json!({"t": "exit", "code": status, "wall": pyfmt::round_to(t0.elapsed().as_secs_f64(), 2)}));
    events
}

/// What showinfo and ashowinfo say about each frame, read wherever it is on a line (the same patterns as
/// testsrc_check.frame_info). FFmpeg writes a log line in several pieces and the video and audio filter threads write
/// at the same time, so one filter's text can land inside the other's line, and a pattern that wanted the
/// `[Parsed_showinfo...]` prefix first lost that frame's time. The two are told apart by what follows pts_time:
/// `duration` for a video frame, the format for audio. FFmpeg 5.1 and 6.0 write `pos:` after pts_time (video: before
/// `fmt:... sar:`, audio: before `fmt:`), so the patterns are wider than testsrc_check.py's on purpose.
fn frame_info(line: &str, vpts: &mut HashMap<usize, f64>, aframes: &mut HashMap<u64, (f64, u64, u64)>) -> bool {
    static PATTERNS: OnceLock<(Regex, Regex)> = OnceLock::new();
    let (video, audio) = PATTERNS.get_or_init(|| (
        Regex::new(r"\bn:\s*(\d+)\s+pts:\s*\S+\s+pts_time:\s*([-\d.]+)\s+(?:duration:|pos:\s*-?\d+\s+fmt:\S+\s+sar:)").unwrap(),
        Regex::new(concat!(r"\bn:\s*(\d+)\s+pts:\s*\S+\s+pts_time:\s*([-\d.]+)\s+(?:pos:\s*-?\d+\s+)?fmt:\S+\s+channels:\s*\d+\s+chlayout:\S+",
                           r"\s+rate:\s*(\d+)\s+nb_samples:\s*(\d+)")).unwrap(),
    ));
    let mut found = false;
    for m in video.captures_iter(line) {
        vpts.insert(m[1].parse().unwrap_or(usize::MAX), m[2].parse().unwrap_or(f64::NAN));
        found = true;
    }
    for m in audio.captures_iter(line) {
        // the first one seen: ashowinfo counts from 0 again when the audio format changes mid-stream
        aframes.entry(m[1].parse().unwrap_or(u64::MAX))
            .or_insert((m[2].parse().unwrap_or(f64::NAN), m[3].parse().unwrap_or(1), m[4].parse().unwrap_or(0)));
        found = true;
    }
    found
}

/// Time of the first decoded audio sample, from ashowinfo lines {n: (pts_time, rate, nb_samples)}. If frame 0's
/// line is missing (ffmpeg drops the odd log line), ask `first_pts` (the first decoded frame's time, or None);
/// failing that, count back from the first one logged, assuming the frames before it were the same size. Counting
/// back is wrong after a jump in the timestamps (an AVI whose MP3 starts with 300 KB of zeros).
pub fn audio_start(aframes: &HashMap<u64, (f64, u64, u64)>, first_pts: impl FnOnce() -> Option<f64>) -> f64 {
    let Some(n) = aframes.keys().min().copied() else { return 0.0 };
    let (pts, rate, nb) = aframes[&n];
    if n == 0 { return pts; }
    first_pts().unwrap_or_else(|| pyfmt::round_to(pts - (n * nb) as f64 / rate as f64, 6))
}

/// The time of the first decoded audio frame of `path`, by ffprobe (from its first 10 packets: one AAC packet
/// decodes to nothing); None if it can't say.
fn first_audio_pts(ffprobe: &str, path: &str) -> Option<f64> {
    let o = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "frame=pts_time", "-read_intervals", "%+#10", "-of", "csv=p=0", path])
        .output().ok()?;
    String::from_utf8_lossy(&o.stdout).split_whitespace().next()?.parse().ok()
}

/// "ffmpeg 7.1.5-0+deb13u1", from the first line of `ffmpeg -version`.
fn version(ffmpeg: &str) -> String {
    Command::new(ffmpeg).arg("-version").output().ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(String::from))
        .and_then(|l| l.strip_prefix("ffmpeg version ").map(|v| {
            let v = v.split_whitespace().next().unwrap_or(v);
            // release builds name the git tag: "n9.0.2" is FFmpeg 9.0.2
            let v = v.strip_prefix('n').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(v);
            format!("ffmpeg {v}")
        }))
        .unwrap_or_else(|| "ffmpeg".into())
}

fn s16(raw: &[u8]) -> Vec<i16> {
    raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_start_from_ashowinfo() {
        let m = |v: &[(u64, f64, u64, u64)]| v.iter().map(|&(n, p, r, b)| (n, (p, r, b))).collect::<HashMap<_, _>>();
        let none = || None;
        assert_eq!(audio_start(&m(&[]), none), 0.0);
        assert_eq!(audio_start(&m(&[(0, 0.0, 48000, 4608), (1, 0.096, 48000, 4608)]), none), 0.0);
        assert_eq!(audio_start(&m(&[(1, 0.096, 48000, 4608), (2, 0.192, 48000, 4608)]), none), 0.0);
        assert_eq!(audio_start(&m(&[(3, 10.064, 48000, 1024)]), none), 10.0);
    }

    #[test]
    fn audio_start_asks_ffprobe_when_frame_0_is_missing() {
        // an AVI whose MP3 starts with zeros: frame 0 at 0 s, frame 1 at 6.432 s; counting back from frame 1 says 6.4 s early
        let m: HashMap<u64, (f64, u64, u64)> = [(1, (6.432, 44100, 1152))].into();
        assert_eq!(audio_start(&m, || Some(0.0)), 0.0);
        assert!(audio_start(&m, || None) > 6.0);
        // frame 0 logged: ffprobe isn't asked
        let m: HashMap<u64, (f64, u64, u64)> = [(0, (0.5, 44100, 1152))].into();
        assert_eq!(audio_start(&m, || panic!("asked")), 0.5);
    }

    // a line FFmpeg 9.0.2 wrote while decoding edge/t-emsg.mp4 (480x270): video frame 3's showinfo text inside audio
    // frame 67's ashowinfo line, which lost frame 3's time when the pattern wanted the [Parsed_showinfo...] prefix first
    const SPLICED: &str = "[Parsed_ashowinfo_0 @ 0x752758002e40] [info] n:67 pts:68608 pts_time:1.429333 fmt:fltp channels:2 chlayout:stereo \
        rate:48000 nb_samples:1024 checksum:1BE25492 plane_checksums: [ B2142A49 B2142A49 n:   3 pts:   3072 \
        pts_time:0.3     duration:   1024 duration_time:0.1     fmt:yuv420p cl:unspecified sa";

    #[test]
    fn a_frame_whose_text_landed_inside_the_other_filters_line_keeps_its_time() {
        let (mut v, mut a) = (HashMap::new(), HashMap::new());
        assert!(frame_info(SPLICED, &mut v, &mut a));
        assert_eq!(v, HashMap::from([(3, 0.3)]));
        assert_eq!(a, HashMap::from([(67, (1.429333, 48000, 1024))]));
    }

    #[test]
    fn frame_info_reads_both_filters_and_nothing_else() {
        let (mut v, mut a) = (HashMap::new(), HashMap::new());
        let video = "[Parsed_showinfo_2 @ 0x1] [info] n:  10 pts:  92160 pts_time:1       duration:   9216 duration_time:0.1     fmt:yuv420p";
        let audio = "[Parsed_ashowinfo_0 @ 0x2] [info] n:0 pts:0 pts_time:0 fmt:fltp channels:2 chlayout:stereo rate:48000 nb_samples:1024 checksum:0";
        assert!(frame_info(video, &mut v, &mut a) && frame_info(audio, &mut v, &mut a));
        assert!(frame_info("[Parsed_ashowinfo_0 @ 0x2] [info] n:0 pts:9 pts_time:5 fmt:s16 channels:1 chlayout:mono rate:8000 nb_samples:9", &mut v, &mut a));
        assert_eq!((v, a), (HashMap::from([(10, 1.0)]), HashMap::from([(0, (0.0, 48000, 1024))]))); // the first audio frame 0 stays
        assert!(!frame_info("[mov,mp4 @ 0x3] [warning] stream 1, timescale not set", &mut HashMap::new(), &mut HashMap::new()));
    }

    #[test]
    fn frame_info_reads_the_pos_field_of_ffmpeg_5_1_and_6_0() {
        let lines = [
            // 5.1.9 (debian 12), as captured
            ("[Parsed_showinfo_0 @ 0x1] [info] n:   2 pts:      2 pts_time:0.2     pos:       -1 fmt:rgb24 sar:1/1 s:32x32 i:P iskey:1 type:I checksum:2FEBBC89", true),
            ("[Parsed_ashowinfo_0 @ 0x2] [info] n:0 pts:0 pts_time:0 pos:-1 fmt:s16 channels:1 chlayout:mono rate:8000 nb_samples:400 checksum:87B8894D", false),
            // 6.0: video has duration before pos, audio has pos before fmt
            ("[Parsed_showinfo_0 @ 0x1] [info] n:   3 pts:   3 pts_time:0.3     duration:   1 duration_time:0.1     pos:       -1 fmt:yuv420p sar:1/1", true),
            ("[Parsed_ashowinfo_0 @ 0x2] [info] n:1 pts:400 pts_time:0.05 pos:-1 fmt:fltp channels:2 chlayout:stereo rate:8000 nb_samples:400", false),
        ];
        for (line, is_video) in lines {
            let (mut v, mut a) = (HashMap::new(), HashMap::new());
            assert!(frame_info(line, &mut v, &mut a), "{line}");
            // a video line never lands in the audio map, nor the reverse
            assert_eq!((v.len(), a.len()), if is_video { (1, 0) } else { (0, 1) }, "{line}");
        }
        let (mut v, mut a) = (HashMap::new(), HashMap::new());
        frame_info(lines[0].0, &mut v, &mut a);
        frame_info(lines[3].0, &mut v, &mut a);
        assert_eq!((v, a), (HashMap::from([(2, 0.2)]), HashMap::from([(1, (0.05, 8000, 400))])));
    }

    // fake ffprobe and ffmpeg (shell scripts) stand in for the programs: play_with runs end to end without FFmpeg
    fn fake(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("testsrc-ffmpeg-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ffmpeg: -version; the first-picture run (size in showinfo); the decode run, whose log carries the spliced line,
    // loses audio frame 0, and writes two 2x2 RGB frames to stdout and some samples to the last argument
    const FAKE_FFMPEG: &str = r#"
case "$*" in
  *-version*) echo "ffmpeg version n9.0.2 Copyright"; exit 0;;
  *"-frames:v 1"*) echo "[Parsed_showinfo_1 @ 0x1] [info] n:0 pts:0 pts_time:0 duration:1 fmt:rgb24 s:2x2 i:P" >&2; exit 0;;
esac
for a; do last=$a; done
echo "[Parsed_showinfo_1 @ 0x1] [info] n:   0 pts:   0 pts_time:4       duration:   1 duration_time:0.1     fmt:rgb24" >&2
echo "[Parsed_ashowinfo_0 @ 0x2] [info] n:1 pts:1024 pts_time:4.5 fmt:fltp channels:1 chlayout:mono rate:44100 nb_samples:1024 checksum:0 plane_checksums: [ AB n:   1 pts:   1 pts_time:4.1     duration:   1 duration_time:0.1     fmt:rgb24 sa" >&2
echo "[mov @ 0x3] [warning] odd box" >&2
echo "[mov @ 0x3] [error] bad box" >&2
head -c 24 /dev/zero
if [ "$last" != "pipe:1" ]; then head -c 3000 /dev/zero > "$last"; fi
exit 0
"#;

    #[test]
    fn play_reads_times_from_spliced_lines_and_asks_ffprobe_for_the_missing_audio_frame() {
        let d = scratch("play");
        let ffprobe = fake(&d, "ffprobe", r#"case "$*" in
  *format=duration*) echo '{"format":{"duration":"6.0065"},"streams":[{"codec_type":"video","disposition":{"attached_pic":0}},{"codec_type":"audio","disposition":{"attached_pic":0}}]}';;
  *frame=pts_time*) echo 3.9935;;
esac"#);
        let ffmpeg = fake(&d, "ffmpeg", FAKE_FFMPEG);
        let ev = play_with(&ffmpeg, &ffprobe, "x.mp4", 64);
        let of = |t: &str| ev.iter().filter(|e| e["t"] == t).collect::<Vec<_>>();
        assert_eq!(ev[0]["player"], "ffmpeg 9.0.2");
        assert_eq!(ev[0]["duration"], 6.0065);
        let frames = of("frame");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["pts"], 4.0);
        assert_eq!(frames[1]["pts"], 4.1, "the spliced frame keeps its time");
        assert_eq!(of("audio")[0]["start"], 3.9935, "audio frame 0 is missing: ffprobe says when it starts");
        let logs: Vec<_> = of("log").iter().map(|e| e["level"].as_str().unwrap().to_string()).collect();
        assert_eq!(logs, ["warning", "error"], "showinfo lines aren't logged");
        assert_eq!(of("exit")[0]["code"], 0);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn play_of_audio_only_and_of_a_missing_picture_size() {
        let d = scratch("audio");
        let probe = |kinds: &str| fake(&d, "ffprobe", &format!("echo '{{\"format\":{{}},\"streams\":[{kinds}]}}'"));
        let ffmpeg = fake(&d, "ffmpeg", "echo '[mov @ 0x1] [error] no stream' >&2; head -c 4 /dev/zero; exit 3");
        let audio = r#"{"codec_type":"audio","disposition":{}}"#;
        let ev = play_with(&ffmpeg, &probe(audio), "x.mka", 64);
        assert_eq!(ev[0]["duration"], Value::Null);
        assert_eq!(ev[1]["t"], "audio");
        assert_eq!(ev[2]["msg"], "[mov @ 0x1] [error] no stream");
        assert_eq!(ev.last().unwrap()["code"], 3);
        // a video stream whose first picture gives no size
        let ev = play_with(&ffmpeg, &probe(r#"{"codec_type":"video","disposition":{}}"#), "x.mp4", 64);
        assert_eq!(ev[1]["msg"], "ffmpeg found no picture size");
        assert_eq!(ev[2]["code"], 1);
        // ffmpeg that can't run: the decode step reports it
        let nope = d.join("nope").to_string_lossy().into_owned();
        assert_eq!(version(&nope), "ffmpeg");
        let ev = play_with(&nope, &probe(r#"{"codec_type":"video","disposition":{}}"#), "x.mp4", 64);
        assert_eq!(ev[1]["setup"], true);
        assert!(ev[1]["msg"].as_str().unwrap().starts_with(&format!("can't run {nope}: ")), "{}", ev[1]);
        // no ffprobe: said before anything else
        let ev = play_with(&ffmpeg, &nope, "x.mp4", 64);
        assert_eq!((ev[0]["player"].as_str(), ev[1]["setup"].as_bool()), (Some("ffmpeg"), Some(true)));
        assert!(ev[1]["msg"].as_str().unwrap().contains("Put ffmpeg and ffprobe 5.1 or later on PATH"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn first_audio_pts_says_none_when_ffprobe_says_nothing() {
        let d = scratch("pts");
        assert_eq!(first_audio_pts(&fake(&d, "empty", "true"), "x"), None);
        assert_eq!(first_audio_pts(&fake(&d, "text", "echo oops"), "x"), None);
        assert_eq!(first_audio_pts(&fake(&d, "num", "echo 0.25"), "x"), Some(0.25));
        assert_eq!(first_audio_pts("/nonexistent/ffprobe", "x"), None);
        std::fs::remove_dir_all(d).unwrap();
    }
}
