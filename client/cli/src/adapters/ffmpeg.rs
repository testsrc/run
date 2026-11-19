//! ffmpeg as the player: decodes as fast as it can, timestamps from the file (showinfo). The control the
//! generator verified every file with. Same behaviour as the reference adapter (testsrc_check.ffmpeg_adapter).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Instant;

use regex::Regex;
use serde_json::{json, Value};
use testsrc_core::{find_beeps, pyfmt, Reader, RgbFrame};

const RATE: u32 = 44100;

pub fn play(path: &str, width: u32) -> Vec<Value> {
    let t0 = Instant::now();
    let mut events = vec![];
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type:stream_disposition=attached_pic", "-of", "json", path])
        .output().ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()).unwrap_or(json!({}));
    // an attached picture (cover art) is a video stream to ffprobe, but not video to play
    let types: Vec<&str> = probe["streams"].as_array().into_iter().flatten()
        .filter(|s| s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0)
        .filter_map(|s| s["codec_type"].as_str()).collect();
    // none for a file that doesn't declare one: unknown, not 0
    let duration = probe["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok());
    events.push(json!({"t": "meta", "player": version(), "duration": duration}));
    let has_audio = types.contains(&"audio");

    if !types.contains(&"video") {
        // audio only: just the beeps
        let o = Command::new("ffmpeg")
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
    let first = Command::new("ffmpeg")
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
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-loglevel", "level+info", "-nostats", "-copyts", "-i", path, "-map", "0:v:0", "-vf", &format!("{chain},showinfo"),
              "-fps_mode", "passthrough", "-f", "rawvideo", "pipe:1"]);
    if has_audio {
        cmd.args(["-map", "0:a:0", "-af", "ashowinfo", "-ac", "1", "-ar", "44100", "-f", "s16le", "-y"]).arg(&afile);
    }
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            events.push(json!({"t": "log", "level": "error", "msg": format!("can't run ffmpeg: {e}")}));
            events.push(json!({"t": "exit", "code": 1, "wall": 0.0}));
            return events;
        }
    };
    // keyed by frame index: ffmpeg drops the odd showinfo/ashowinfo line, so position in the log can't be trusted
    let stderr = child.stderr.take().unwrap();
    let drain = std::thread::spawn(move || {
        let shown = Regex::new(r"Parsed_showinfo_\d+ @[^\]]*\](?: \[\w+\])?\s*n:\s*(\d+)\s.*?pts_time:\s*([-\d.]+)").unwrap();
        let ashown = Regex::new(concat!(r"Parsed_ashowinfo_\d+ @[^\]]*\](?: \[\w+\])?\s*n:\s*(\d+)\s.*?pts_time:\s*([-\d.]+)",
                                        r".*?rate:\s*(\d+).*?nb_samples:\s*(\d+)")).unwrap();
        let (mut vpts, mut aframes, mut logs) = (HashMap::new(), HashMap::new(), vec![]);
        for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            let mut any = false;
            for m in shown.captures_iter(&line) {
                vpts.insert(m[1].parse::<usize>().unwrap_or(usize::MAX), m[2].parse::<f64>().unwrap_or(f64::NAN));
                any = true;
            }
            if let Some(m) = ashown.captures(&line) {
                let n = m[1].parse::<u64>().unwrap_or(u64::MAX);
                // the first one seen: ashowinfo counts from 0 again when the audio format changes mid-stream
                aframes.entry(n).or_insert((m[2].parse::<f64>().unwrap_or(f64::NAN), m[3].parse::<u64>().unwrap_or(1), m[4].parse::<u64>().unwrap_or(0)));
                any = true;
            }
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
            let start = audio_start(&aframes);
            events.push(json!({"t": "audio", "start": start, "beeps": find_beeps(&s16(&raw), RATE, start)}));
        }
        let _ = std::fs::remove_file(&afile);
    }
    events.extend(logs);
    events.push(json!({"t": "exit", "code": status, "wall": pyfmt::round_to(t0.elapsed().as_secs_f64(), 2)}));
    events
}

/// Time of the first decoded audio sample, from ashowinfo lines {n: (pts_time, rate, nb_samples)}. If frame 0's
/// line is missing, count back from the first one logged, assuming the frames before it were the same size.
pub fn audio_start(aframes: &HashMap<u64, (f64, u64, u64)>) -> f64 {
    let Some(n) = aframes.keys().min().copied() else { return 0.0 };
    let (pts, rate, nb) = aframes[&n];
    if n == 0 { pts } else { pyfmt::round_to(pts - (n * nb) as f64 / rate as f64, 6) }
}

/// "ffmpeg 7.1.5-0+deb13u1", from the first line of `ffmpeg -version`.
fn version() -> String {
    Command::new("ffmpeg").arg("-version").output().ok()
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
        assert_eq!(audio_start(&m(&[])), 0.0);
        assert_eq!(audio_start(&m(&[(0, 0.0, 48000, 4608), (1, 0.096, 48000, 4608)])), 0.0);
        assert_eq!(audio_start(&m(&[(1, 0.096, 48000, 4608), (2, 0.192, 48000, 4608)])), 0.0);
        assert_eq!(audio_start(&m(&[(3, 10.064, 48000, 1024)])), 10.0);
    }
}
