//! libvlc 3.0 as the player, in real time, through its video and audio callbacks. libvlc is loaded at run
//! time (libvlc.so.5, or libvlc.dll from VLC_DIR), so one binary works with whichever VLC 3.0.x is there.
//!
//! Frames are timestamped with libvlc_clock() when VLC presents them, audio with the play dates VLC gives
//! the audio callback, so the A/V offset measured is the one a viewer would see. Frames are captured at
//! --width (VLC scales, keeping the storage aspect; the track's SAR is reported apart) and read on a worker
//! thread, so only frame codes are kept.
//!
//! --capture yuv (default): the decoder's picture as I420 (J420 for full range), converted by the reader
//! with the file's matrix, so colour judges the decode. --capture rgb: RV24 from VLC's software converter,
//! what an app asking the callbacks for RGB shows (VLC 3 converts with BT.601 whatever the file says).
//!
//! libvlc 3 callback quirks handled here:
//! - the format callback offers the decoder's padded size (1920x1088 for H.264, 1920x1152 for AV1); VLC
//!   fills whatever size is asked for with the visible picture, so ask for the visible size;
//! - the player opens its audio output before the audio callbacks are set, so VLC probes every output.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use testsrc_core::{find_beeps, pyfmt, Matrix, Range, Reader, RgbFrame, YuvFrame};

const RATE: u32 = 44100;
const ENDED: c_int = 6;
const ERROR: c_int = 7;

type P = *mut c_void;

#[repr(C)]
struct VideoTrack {
    height: c_uint,
    width: c_uint,
    sar_num: c_uint,
    sar_den: c_uint,
}

#[repr(C)]
struct Track {
    codec: u32,
    fourcc: u32,
    id: c_int,
    kind: c_int,
    profile: c_int,
    level: c_int,
    video: *const VideoTrack,
}

struct Lib {
    _lib: libloading::Library,
    new: unsafe extern "C" fn(c_int, *const *const c_char) -> P,
    release: unsafe extern "C" fn(P),
    media_new_path: unsafe extern "C" fn(P, *const c_char) -> P,
    media_release: unsafe extern "C" fn(P),
    tracks_get: unsafe extern "C" fn(P, *mut *mut *mut Track) -> c_uint,
    tracks_release: unsafe extern "C" fn(*mut *mut Track, c_uint),
    mp_new: unsafe extern "C" fn(P) -> P,
    mp_play: unsafe extern "C" fn(P) -> c_int,
    mp_stop: unsafe extern "C" fn(P),
    mp_release: unsafe extern "C" fn(P),
    mp_state: unsafe extern "C" fn(P) -> c_int,
    mp_length: unsafe extern "C" fn(P) -> i64,
    clock: unsafe extern "C" fn() -> i64,
    version: unsafe extern "C" fn() -> *const c_char,
    video_callbacks: unsafe extern "C" fn(P, LockCb, UnlockCb, DisplayCb, P),
    video_format_callbacks: unsafe extern "C" fn(P, FormatCb, CleanupCb),
    audio_callbacks: unsafe extern "C" fn(P, PlayCb, PtsCb, PtsCb, PtsCb, DrainCb, P),
    audio_format: unsafe extern "C" fn(P, *const c_char, c_uint, c_uint),
}

type LockCb = unsafe extern "C" fn(P, *mut P) -> P;
type UnlockCb = unsafe extern "C" fn(P, P, *const P);
type DisplayCb = unsafe extern "C" fn(P, P);
type FormatCb = unsafe extern "C" fn(*mut P, *mut c_char, *mut c_uint, *mut c_uint, *mut c_uint, *mut c_uint) -> c_uint;
type CleanupCb = unsafe extern "C" fn(P);
type PlayCb = unsafe extern "C" fn(P, *const c_void, c_uint, i64);
type PtsCb = unsafe extern "C" fn(P, i64);
type DrainCb = unsafe extern "C" fn(P);

impl Lib {
    fn load() -> Result<Lib, String> {
        let lib = unsafe { libloading::Library::new(library_path()) }.map_err(|e| format!("can't load libvlc: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Lib {
            new: sym!("libvlc_new"),
            release: sym!("libvlc_release"),
            media_new_path: sym!("libvlc_media_new_path"),
            media_release: sym!("libvlc_media_release"),
            tracks_get: sym!("libvlc_media_tracks_get"),
            tracks_release: sym!("libvlc_media_tracks_release"),
            mp_new: sym!("libvlc_media_player_new_from_media"),
            mp_play: sym!("libvlc_media_player_play"),
            mp_stop: sym!("libvlc_media_player_stop"),
            mp_release: sym!("libvlc_media_player_release"),
            mp_state: sym!("libvlc_media_player_get_state"),
            mp_length: sym!("libvlc_media_player_get_length"),
            clock: sym!("libvlc_clock"),
            version: sym!("libvlc_get_version"),
            video_callbacks: sym!("libvlc_video_set_callbacks"),
            video_format_callbacks: sym!("libvlc_video_set_format_callbacks"),
            audio_callbacks: sym!("libvlc_audio_set_callbacks"),
            audio_format: sym!("libvlc_audio_set_format"),
            _lib: lib,
        })
    }
}

#[cfg(windows)]
fn library_path() -> std::path::PathBuf {
    let dir = std::env::var("VLC_DIR").unwrap_or_else(|_| r"C:\Program Files\VideoLAN\VLC".into());
    // libvlc.dll finds libvlccore.dll and the plugins next to itself only if that folder is on the search path
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{dir};{path}"));
    std::path::Path::new(&dir).join("libvlc.dll")
}

#[cfg(not(windows))]
fn library_path() -> std::path::PathBuf {
    std::env::var("LIBVLC").unwrap_or_else(|_| "libvlc.so.5".into()).into()
}

#[derive(Clone, Copy, PartialEq)]
pub enum Capture {
    Yuv,
    Rgb,
}

/// What the callbacks share. Only touched from libvlc's threads while the player runs.
struct State {
    lib: *const Lib,
    media: P,
    width: u32,
    hint: Option<(u32, u32)>,
    capture: Capture,
    full_range: bool,
    buf: Vec<u8>,
    planes: Vec<usize>,
    w: usize,
    h: usize,
    offered: Option<(u32, u32)>,
    shown: usize,
    frames: mpsc::Sender<(i64, usize, usize, Vec<u8>)>,
    audio: Mutex<Vec<(i64, Vec<u8>)>>,
}

fn video_track(lib: &Lib, media: P) -> (u32, u32, Option<String>) {
    let mut tracks: *mut *mut Track = std::ptr::null_mut();
    let n = unsafe { (lib.tracks_get)(media, &mut tracks) };
    let mut out = (0, 0, None);
    for i in 0..n as usize {
        let t = unsafe { &**tracks.add(i) };
        if t.kind == 1 && !t.video.is_null() {
            let v = unsafe { &*t.video };
            out = (v.width, v.height, (v.sar_den != 0).then(|| format!("{}:{}", v.sar_num, v.sar_den)));
            break;
        }
    }
    if n > 0 {
        unsafe { (lib.tracks_release)(tracks, n) };
    }
    out
}

unsafe extern "C" fn setup(opaque: *mut P, chroma: *mut c_char, w: *mut c_uint, h: *mut c_uint, pitches: *mut c_uint, lines: *mut c_uint) -> c_uint {
    let st = &mut *(*opaque as *mut State);
    let (mut vw, mut vh, _) = video_track(&*st.lib, st.media);
    st.offered = Some((*w, *h));
    if !(vw > 0 && vh > 0 && vw <= *w && vh <= *h) {
        // no size in the track info yet (MPEG-PS: the streams are found while playing): the fact sheet's size if
        // it fits the decoder's padded one, else the padded size (which VLC fills by stretching the picture)
        (vw, vh) = match st.hint {
            Some((hw, hh)) if hw > 0 && hh > 0 && hw <= *w && hh <= *h => (hw, hh),
            _ => (*w, *h),
        };
    }
    if st.width > 0 && vw > st.width {
        // keep the storage aspect; the SAR is reported apart
        vh = (pyfmt::round(vh as f64 * st.width as f64 / vw as f64 / 2.0) * 2).max(2) as u32;
        vw = st.width;
    }
    (*w, *h) = (vw, vh);
    let (w, h) = (vw as usize, vh as usize);
    st.w = w;
    st.h = h;
    let set = |s: &[u8; 4]| std::ptr::copy_nonoverlapping(s.as_ptr() as *const c_char, chroma, 4);
    let size = if st.capture == Capture::Rgb {
        set(b"RV24");
        *pitches = (w * 3) as c_uint;
        *lines = h as c_uint;
        st.planes = vec![0];
        w * h * 3
    } else {
        // J420 for full-range files: asked for I420, VLC first squeezes full range into limited
        set(if st.full_range { b"J420" } else { b"I420" });
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        for i in 0..3 {
            *pitches.add(i) = if i == 0 { w } else { cw } as c_uint;
            *lines.add(i) = if i == 0 { h } else { ch } as c_uint;
        }
        st.planes = vec![0, w * h, w * h + cw * ch];
        w * h + 2 * cw * ch
    };
    st.buf = vec![0; size];
    1
}

unsafe extern "C" fn cleanup(_: P) {}

unsafe extern "C" fn lock(opaque: P, planes: *mut P) -> P {
    let st = &mut *(opaque as *mut State);
    let base = st.buf.as_mut_ptr();
    for (i, &off) in st.planes.iter().enumerate() {
        *planes.add(i) = base.add(off) as P;
    }
    std::ptr::null_mut()
}

unsafe extern "C" fn unlock(_: P, _: P, _: *const P) {}

unsafe extern "C" fn display(opaque: P, _: P) {
    let st = &mut *(opaque as *mut State);
    let clk = ((*st.lib).clock)();
    let _ = st.frames.send((clk, st.w, st.h, st.buf.clone()));
    st.shown += 1;
}

unsafe extern "C" fn audio_play(data: P, samples: *const c_void, count: c_uint, pts: i64) {
    let st = &*(data as *const State);
    let bytes = std::slice::from_raw_parts(samples as *const u8, count as usize * 4).to_vec();
    st.audio.lock().unwrap().push((pts, bytes));
}

unsafe extern "C" fn audio_pts(_: P, _: i64) {}
unsafe extern "C" fn audio_drain(_: P) {}

pub struct Options {
    pub timeout: f64,
    pub width: u32,
    /// The visible size from the fact sheet, used only when libvlc's track info has none yet
    pub hint: Option<(u32, u32)>,
    pub capture: Capture,
    pub matrix: Matrix,
    pub range: Range,
}

pub fn play(path: &str, logfile: &str, o: &Options) -> Vec<Value> {
    let lib = match Lib::load() {
        Ok(l) => l,
        Err(e) => return vec![json!({"t": "meta", "player": "libvlc"}), json!({"t": "log", "level": "error", "msg": e})],
    };
    let args: Vec<CString> = ["--quiet", "--intf=dummy", "--aout=amem", "--no-video-title-show", "--no-osd",
                              "--avcodec-hw=none", "--file-logging", &format!("--logfile={logfile}"), "--log-verbose=0"]
        .iter().map(|a| CString::new(*a).unwrap()).collect();
    let argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    let abs = std::fs::canonicalize(path).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(path.into());
    let cpath = CString::new(abs).unwrap();
    let (tx, rx) = mpsc::channel::<(i64, usize, usize, Vec<u8>)>();
    let (capture, matrix, range) = (o.capture, o.matrix, o.range);
    let worker = std::thread::spawn(move || {
        let (mut reader, mut out, mut last, mut held) = (Reader::new(), vec![], None, 0usize);
        for (n, (clk, w, h, raw)) in rx.into_iter().enumerate() {
            let r = if capture == Capture::Rgb {
                reader.read(&RgbFrame::new(&raw, w, h), n % 25 == 0)
            } else {
                reader.read(&YuvFrame::new(&raw, w, h, matrix, range), n % 25 == 0)
            };
            // VLC shows a picture again while it waits for the next one (low frame rates, the last frame): a
            // held frame, not a repeat. Each frame is reported once, when it first appeared.
            if r.code.is_some() && r.code == last {
                held += 1;
                continue;
            }
            last = r.code;
            let mut e = json!({"t": "frame", "pts": clk as f64 / 1e6});
            e.as_object_mut().unwrap().extend(r.to_json().as_object().unwrap().clone());
            out.push(e);
        }
        (out, held)
    });

    let t0 = Instant::now();
    let (length, state, timed_out, sar, offered, chunks);
    unsafe {
        let inst = (lib.new)(argv.len() as c_int, argv.as_ptr());
        let media = (lib.media_new_path)(inst, cpath.as_ptr());
        let mp = (lib.mp_new)(media);
        let mut st = Box::new(State {
            lib: &lib, media, width: o.width, hint: o.hint, capture: o.capture, full_range: o.range == Range::Full,
            buf: vec![], planes: vec![], w: 0, h: 0, offered: None, shown: 0, frames: tx, audio: Mutex::new(vec![]),
        });
        let sp = &mut *st as *mut State as P;
        (lib.video_callbacks)(mp, lock, unlock, display, sp);
        (lib.video_format_callbacks)(mp, setup, cleanup);
        (lib.audio_callbacks)(mp, audio_play, audio_pts, audio_pts, audio_pts, audio_drain, sp);
        let s16n = CString::new("S16N").unwrap();
        (lib.audio_format)(mp, s16n.as_ptr(), RATE, 2);
        (lib.mp_play)(mp);
        let (mut len, mut to) = (-1i64, false);
        let mut s;
        loop {
            s = (lib.mp_state)(mp);
            if len <= 0 {
                len = (lib.mp_length)(mp);
            }
            if s == ENDED || s == ERROR {
                break;
            }
            if t0.elapsed().as_secs_f64() > o.timeout {
                to = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        sar = video_track(&lib, media).2;
        (lib.mp_stop)(mp);
        (lib.mp_release)(mp);
        (lib.media_release)(media);
        (lib.release)(inst);
        length = len;
        state = s;
        timed_out = to;
        offered = st.offered;
        chunks = std::mem::take(&mut *st.audio.lock().unwrap());
        drop(st); // closes the frame channel, so the worker finishes
    }
    let wall = t0.elapsed().as_secs_f64();
    let (frames, held) = worker.join().unwrap_or_default();
    let ver = unsafe { CStr::from_ptr((lib.version)()) }.to_string_lossy().into_owned();
    let cap = match (o.capture, o.range) {
        (Capture::Rgb, _) => "RV24",
        (_, Range::Full) => "J420",
        _ => "I420",
    };
    let mut events = vec![json!({"t": "meta", "player": format!("libvlc {ver}"), "capture": format!("video callbacks, {cap}"),
                                 "offered": offered.map(|(w, h)| vec![w, h]), "held_redisplays": held,
                                 "duration": (length > 0).then(|| length as f64 / 1000.0), "sar": sar})];
    events.extend(frames);

    // audio: stereo s16 chunks with play dates -> mono samples, beep onsets mapped back to chunk dates
    let (mut mono, mut marks) = (Vec::<i16>::new(), vec![]);
    for (pts, raw) in &chunks {
        marks.push((mono.len(), *pts));
        for f in raw.chunks_exact(4) {
            let (l, r) = (i16::from_ne_bytes([f[0], f[1]]) as i32, i16::from_ne_bytes([f[2], f[3]]) as i32);
            mono.push((l + r).div_euclid(2) as i16);
        }
    }
    if !chunks.is_empty() {
        let beeps: Vec<f64> = find_beeps(&mono, RATE, 0.0).iter().map(|t| {
            let idx = (t * RATE as f64) as usize;
            let (start, pts) = marks.iter().rev().find(|m| m.0 <= idx).copied().unwrap_or((0, 0));
            pyfmt::round_to((pts as f64 + (idx - start) as f64 * 1e6 / RATE as f64) / 1e6, 4)
        }).collect();
        events.push(json!({"t": "audio", "start": chunks[0].0 as f64 / 1e6, "beeps": beeps}));
    }
    if let Ok(log) = std::fs::read(logfile) {
        for line in String::from_utf8_lossy(&log).lines() {
            if line.contains(" error:") {
                events.push(json!({"t": "log", "level": "error", "msg": line.trim().trim_start_matches('\u{feff}')}));
            }
        }
    }
    let code: Value = if timed_out { "timeout".into() } else if state == ENDED { 0.into() } else { state.into() };
    events.push(json!({"t": "exit", "code": code, "wall": pyfmt::round_to(wall, 2)}));
    events
}
