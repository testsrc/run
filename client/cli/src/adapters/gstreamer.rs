//! GStreamer as the player: playbin3 in real time, with an appsink for video and one for audio. GStreamer is
//! loaded at run time (libgstreamer-1.0.so.0 and friends), so one binary works with whichever 1.x is there.
//!
//! Video: I420 at --width, keeping the storage aspect, then `videoflip video-direction=auto` (how a GStreamer app
//! honours a file's orientation; playbin3 alone ignores it), into an appsink that syncs to the clock and drops
//! frames more than 20 ms late, like a video sink. Each frame is timestamped with the pipeline's running time
//! when the sink hands it over: when a video sink would show it. Audio: mono S16 at 44.1 kHz into an appsink
//! that syncs to the clock; each buffer is dated with its running time, when an audio sink would play it. Both
//! dates are on the pipeline clock, so the A/V offset measured is the one a viewer would see.
//!
//! The I420 picture is read with the matrix and range GStreamer attaches to it (what its display would convert
//! with), the fact sheet's where it doesn't say. Conversion and scaling run in one pass (videoconvertscale), as
//! cheap as possible on the CPU, since a real app does them on the GPU.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use serde_json::{json, Value};
use testsrc_core::{find_beeps, pyfmt, Matrix, Range, Reader, YuvFrame};

const RATE: u32 = 44100;
const FORMAT_TIME: c_int = 3;
const STATE_NULL: c_int = 1;
const STATE_PLAYING: c_int = 4;
const MSG_EOS: c_int = 1 << 0;
const MSG_ERROR: c_int = 1 << 1;
const MAP_READ: c_int = 1;
const CLOCK_NONE: u64 = u64::MAX;
const SECOND: f64 = 1e9;

type P = *mut c_void;

#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}

/// GstMapInfo (gst/gstmemory.h)
#[repr(C)]
struct MapInfo {
    memory: P,
    flags: c_int,
    data: *const u8,
    size: usize,
    maxsize: usize,
    user_data: [P; 4],
    reserved: [P; 4],
}

// GstMiniObject is 64 bytes on 64-bit targets; GstMessage's type follows it, GstBuffer's pool then pts.
const MESSAGE_TYPE_OFFSET: usize = 64;
const BUFFER_PTS_OFFSET: usize = 72;

struct Lib {
    _libs: Vec<libloading::Library>,
    init: unsafe extern "C" fn(*mut c_int, *mut c_void),
    version_string: unsafe extern "C" fn() -> *mut c_char,
    factory_make: unsafe extern "C" fn(*const c_char, *const c_char) -> P,
    parse_bin: unsafe extern "C" fn(*const c_char, c_int, *mut *mut GError) -> P,
    bin_get_by_name: unsafe extern "C" fn(P, *const c_char) -> P,
    set_state: unsafe extern "C" fn(P, c_int) -> c_int,
    get_bus: unsafe extern "C" fn(P) -> P,
    bus_pop: unsafe extern "C" fn(P, u64, c_int) -> P,
    parse_error: unsafe extern "C" fn(P, *mut *mut GError, *mut *mut c_char),
    mini_unref: unsafe extern "C" fn(P),
    object_unref: unsafe extern "C" fn(P),
    query_duration: unsafe extern "C" fn(P, c_int, *mut i64) -> c_int,
    get_clock: unsafe extern "C" fn(P) -> P,
    clock_time: unsafe extern "C" fn(P) -> u64,
    base_time: unsafe extern "C" fn(P) -> u64,
    sample_buffer: unsafe extern "C" fn(P) -> P,
    sample_caps: unsafe extern "C" fn(P) -> P,
    sample_segment: unsafe extern "C" fn(P) -> P,
    to_running_time: unsafe extern "C" fn(P, c_int, u64) -> u64,
    buffer_map: unsafe extern "C" fn(P, *mut MapInfo, c_int) -> c_int,
    buffer_unmap: unsafe extern "C" fn(P, *mut MapInfo),
    caps_structure: unsafe extern "C" fn(P, c_uint) -> P,
    structure_int: unsafe extern "C" fn(P, *const c_char, *mut c_int) -> c_int,
    structure_fraction: unsafe extern "C" fn(P, *const c_char, *mut c_int, *mut c_int) -> c_int,
    structure_string: unsafe extern "C" fn(P, *const c_char) -> *const c_char,
    filename_to_uri: unsafe extern "C" fn(*const c_char, *mut *mut GError) -> *mut c_char,
    pull: unsafe extern "C" fn(P, u64) -> P,
    is_eos: unsafe extern "C" fn(P) -> c_int,
    object_set: unsafe extern "C" fn(P, *const c_char, ...),
    error_free: unsafe extern "C" fn(*mut GError),
    free: unsafe extern "C" fn(*mut c_void),
}

#[cfg(not(windows))]
const LIBS: [&str; 4] = ["libgstreamer-1.0.so.0", "libgstapp-1.0.so.0", "libgobject-2.0.so.0", "libglib-2.0.so.0"];
#[cfg(windows)]
const LIBS: [&str; 4] = ["gstreamer-1.0-0.dll", "gstapp-1.0-0.dll", "gobject-2.0-0.dll", "glib-2.0-0.dll"];

impl Lib {
    fn load() -> Result<Lib, String> {
        #[cfg(windows)]
        if let Ok(root) = std::env::var("GSTREAMER_1_0_ROOT_MSVC_X86_64") {
            let path = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{}\\bin;{path}", root.trim_end_matches('\\')));
        }
        let libs: Vec<libloading::Library> = LIBS.iter()
            .map(|n| unsafe { libloading::Library::new(n) }.map_err(|e| format!("can't load {n}: {e}")))
            .collect::<Result<_, _>>()?;
        let [gst, app, gobject, glib] = [&libs[0], &libs[1], &libs[2], &libs[3]];
        macro_rules! sym {
            ($lib:expr, $name:literal) => {
                *unsafe { $lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Lib {
            init: sym!(gst, "gst_init"),
            version_string: sym!(gst, "gst_version_string"),
            factory_make: sym!(gst, "gst_element_factory_make"),
            parse_bin: sym!(gst, "gst_parse_bin_from_description"),
            bin_get_by_name: sym!(gst, "gst_bin_get_by_name"),
            set_state: sym!(gst, "gst_element_set_state"),
            get_bus: sym!(gst, "gst_element_get_bus"),
            bus_pop: sym!(gst, "gst_bus_timed_pop_filtered"),
            parse_error: sym!(gst, "gst_message_parse_error"),
            mini_unref: sym!(gst, "gst_mini_object_unref"),
            object_unref: sym!(gst, "gst_object_unref"),
            query_duration: sym!(gst, "gst_element_query_duration"),
            get_clock: sym!(gst, "gst_element_get_clock"),
            clock_time: sym!(gst, "gst_clock_get_time"),
            base_time: sym!(gst, "gst_element_get_base_time"),
            sample_buffer: sym!(gst, "gst_sample_get_buffer"),
            sample_caps: sym!(gst, "gst_sample_get_caps"),
            sample_segment: sym!(gst, "gst_sample_get_segment"),
            to_running_time: sym!(gst, "gst_segment_to_running_time"),
            buffer_map: sym!(gst, "gst_buffer_map"),
            buffer_unmap: sym!(gst, "gst_buffer_unmap"),
            caps_structure: sym!(gst, "gst_caps_get_structure"),
            structure_int: sym!(gst, "gst_structure_get_int"),
            structure_fraction: sym!(gst, "gst_structure_get_fraction"),
            structure_string: sym!(gst, "gst_structure_get_string"),
            filename_to_uri: sym!(gst, "gst_filename_to_uri"),
            pull: sym!(app, "gst_app_sink_try_pull_sample"),
            is_eos: sym!(app, "gst_app_sink_is_eos"),
            object_set: sym!(gobject, "g_object_set"),
            error_free: sym!(glib, "g_error_free"),
            free: sym!(glib, "g_free"),
            _libs: libs,
        })
    }

    fn version(&self) -> String {
        let p = unsafe { (self.version_string)() };
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.free)(p as *mut c_void) };
        s
    }
}

/// Raw pointers to GStreamer objects, shared with the sink threads. GStreamer objects are thread-safe.
#[derive(Clone, Copy)]
struct Ptr(P);
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

pub struct Options {
    pub timeout: f64,
    pub width: u32,
    /// The visible size from the fact sheet: the picture is scaled only if it is wider than --width
    pub hint: Option<(u32, u32)>,
    pub matrix: Matrix,
    pub range: Range,
}

struct Mapped<'a> {
    lib: &'a Lib,
    buf: P,
    info: MapInfo,
}

impl<'a> Mapped<'a> {
    fn new(lib: &'a Lib, buf: P) -> Option<Mapped<'a>> {
        let mut info: MapInfo = unsafe { std::mem::zeroed() };
        (unsafe { (lib.buffer_map)(buf, &mut info, MAP_READ) } != 0).then_some(Mapped { lib, buf, info })
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.info.data, self.info.size) }
    }
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.buffer_unmap)(self.buf, &mut self.info) };
    }
}

fn running_time(lib: &Lib, pipeline: P, clock: P) -> f64 {
    if clock.is_null() {
        return 0.0;
    }
    let now = unsafe { (lib.clock_time)(clock) };
    let base = unsafe { (lib.base_time)(pipeline) };
    now.saturating_sub(base) as f64 / SECOND
}

/// GStreamer's colorimetry string -> matrix and range, where the reader knows them: a preset name, or
/// range:matrix:transfer:primaries (range 1 = full, 2 = limited; matrix 3 = BT.709, 4 = BT.601).
fn colorimetry(s: &str) -> (Option<Matrix>, Option<Range>) {
    match s {
        "bt709" => return (Some(Matrix::Bt709), Some(Range::Limited)),
        "bt601" => return (Some(Matrix::Bt601), Some(Range::Limited)),
        _ => {}
    }
    let f: Vec<&str> = s.split(':').collect();
    if f.len() != 4 {
        return (None, None);
    }
    let range = match f[0] { "1" => Some(Range::Full), "2" => Some(Range::Limited), _ => None };
    let matrix = match f[1] { "3" => Some(Matrix::Bt709), "4" => Some(Matrix::Bt601), _ => None };
    (matrix, range)
}

/// I420 as GStreamer lays it out by default (rows padded to 4 bytes) -> tightly packed planes
fn pack_i420(data: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ys, cs) = ((w + 3) & !3, (cw + 3) & !3);
    let (h2, ch2) = ((h + 1) & !1, ((h + 1) & !1) / 2);
    let (u0, v0) = (ys * h2, ys * h2 + cs * ch2);
    if data.len() < v0 + cs * (ch - 1) + cw {
        return None;
    }
    let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
    for r in 0..h {
        out.extend_from_slice(&data[r * ys..r * ys + w]);
    }
    for base in [u0, v0] {
        for r in 0..ch {
            out.extend_from_slice(&data[base + r * cs..base + r * cs + cw]);
        }
    }
    Some(out)
}

pub fn play(path: &str, o: &Options) -> Vec<Value> {
    let lib = match Lib::load() {
        Ok(l) => l,
        Err(e) => return vec![json!({"t": "meta", "player": "gstreamer"}), json!({"t": "log", "level": "error", "msg": e})],
    };
    unsafe { (lib.init)(std::ptr::null_mut(), std::ptr::null_mut()) };
    let ver = lib.version(); // "GStreamer 1.28.8"
    let mut events = vec![];
    let mut logs = vec![];

    // as libvlc's adapter: scale only pictures wider than --width, keeping the storage aspect (the pixel aspect is
    // reported apart), then turn the picture as the file says
    let size = match o.hint {
        Some((w, h)) if w > 0 && h > 0 && w > o.width => {
            format!(",width={},height={}", o.width, (pyfmt::round(h as f64 * o.width as f64 / w as f64 / 2.0) * 2).max(2))
        }
        Some((w, h)) if w > 0 && h > 0 => format!(",width={w},height={h}"),
        _ => format!(",width={}", o.width),
    };
    let vdesc = format!("videoconvertscale ! video/x-raw,format=I420{size} ! videoflip video-direction=auto ! \
                         appsink name=v sync=true qos=true max-lateness=20000000");
    let adesc = format!("audioconvert ! audioresample ! audio/x-raw,format=S16LE,layout=interleaved,channels=1,rate={RATE} ! \
                         appsink name=a sync=true");
    let abs = std::fs::canonicalize(path).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(path.into());
    let cpath = CString::new(abs).unwrap();

    let t0 = Instant::now();
    let (mut duration, mut code, mut timed_out) = (None::<f64>, Value::from(0), false);
    let (frames, chunks, sar, size, held): (Vec<Value>, _, _, _, usize);
    unsafe {
        let mut gerr: *mut GError = std::ptr::null_mut();
        let uri = (lib.filename_to_uri)(cpath.as_ptr(), &mut gerr);
        let pipeline = (lib.factory_make)(c"playbin3".as_ptr(), std::ptr::null());
        let vbin = (lib.parse_bin)(CString::new(vdesc).unwrap().as_ptr(), 1, &mut gerr);
        let abin = (lib.parse_bin)(CString::new(adesc).unwrap().as_ptr(), 1, &mut gerr);
        if pipeline.is_null() || vbin.is_null() || abin.is_null() || uri.is_null() {
            let msg = if gerr.is_null() { "can't build the playbin3 pipeline".to_string() }
                      else { CStr::from_ptr((*gerr).message).to_string_lossy().into_owned() };
            return vec![json!({"t": "meta", "player": ver}), json!({"t": "log", "level": "error", "msg": msg}),
                        json!({"t": "exit", "code": 1, "wall": 0.0})];
        }
        let vsink = (lib.bin_get_by_name)(vbin, c"v".as_ptr());
        let asink = (lib.bin_get_by_name)(abin, c"a".as_ptr());
        (lib.object_set)(pipeline, c"uri".as_ptr(), uri, std::ptr::null::<c_char>());
        (lib.object_set)(pipeline, c"video-sink".as_ptr(), vbin, std::ptr::null::<c_char>());
        (lib.object_set)(pipeline, c"audio-sink".as_ptr(), abin, std::ptr::null::<c_char>());
        (lib.free)(uri as *mut c_void);

        let stop = Arc::new(AtomicBool::new(false));
        let (pl, vs, as_) = (Ptr(pipeline), Ptr(vsink), Ptr(asink));
        let libp = Ptr(&lib as *const Lib as P);

        // video: read frame codes on a worker, as libvlc's adapter does
        let (tx, rx) = mpsc::channel::<(f64, usize, usize, Vec<u8>, Option<Matrix>, Option<Range>)>();
        let (matrix, range) = (o.matrix, o.range);
        let reader = std::thread::spawn(move || {
            let (mut reader, mut out, mut last, mut held) = (Reader::new(), vec![], None, 0usize);
            for (n, (t, w, h, raw, cm, cr)) in rx.into_iter().enumerate() {
                // read the picture as GStreamer describes it (what its display would convert with), the fact
                // sheet's matrix and range where it doesn't say
                let r = reader.read(&YuvFrame::new(&raw, w, h, cm.unwrap_or(matrix), cr.unwrap_or(range)), n % 25 == 0);
                // the same picture again right away (a field of an interlaced frame, shown on its own by the
                // deinterlacer): a held frame, not a repeat, as with libvlc and mpv
                if r.code.is_some() && r.code == last {
                    held += 1;
                    continue;
                }
                last = r.code;
                let mut e = json!({"t": "frame", "pts": pyfmt::round_to(t, 6)});
                e.as_object_mut().unwrap().extend(r.to_json().as_object().unwrap().clone());
                out.push(e);
            }
            (out, held)
        });
        let stop_v = stop.clone();
        let video = std::thread::spawn(move || {
            let (pl, vs, libp) = (pl, vs, libp);
            let lib = &*(libp.0 as *const Lib);
            let (mut sar, mut size, mut clock, mut got) = (None::<String>, None::<(i32, i32)>, std::ptr::null_mut::<c_void>(), false);
            while !stop_v.load(Ordering::Relaxed) {
                let sample = (lib.pull)(vs.0, 50_000_000);
                if sample.is_null() {
                    // a sink that hasn't started yet also says EOS: only trust it once samples came
                    if got && (lib.is_eos)(vs.0) != 0 {
                        break;
                    }
                    continue;
                }
                got = true;
                if clock.is_null() {
                    clock = (lib.get_clock)(pl.0);
                }
                let t = running_time(lib, pl.0, clock);
                let caps = (lib.sample_caps)(sample);
                let st = (lib.caps_structure)(caps, 0);
                let (mut w, mut h, mut pn, mut pd) = (0, 0, 1, 1);
                (lib.structure_int)(st, c"width".as_ptr(), &mut w);
                (lib.structure_int)(st, c"height".as_ptr(), &mut h);
                if (lib.structure_fraction)(st, c"pixel-aspect-ratio".as_ptr(), &mut pn, &mut pd) != 0 {
                    sar = Some(format!("{pn}:{pd}"));
                }
                size = Some((w, h));
                let cs = (lib.structure_string)(st, c"colorimetry".as_ptr());
                let (cm, cr) = if cs.is_null() { (None, None) } else { colorimetry(&CStr::from_ptr(cs).to_string_lossy()) };
                if let Some(m) = Mapped::new(lib, (lib.sample_buffer)(sample)) {
                    if let Some(raw) = pack_i420(m.bytes(), w as usize, h as usize) {
                        let _ = tx.send((t, w as usize, h as usize, raw, cm, cr));
                    }
                }
                (lib.mini_unref)(sample);
            }
            if !clock.is_null() {
                (lib.object_unref)(clock);
            }
            (sar, size)
        });
        let stop_a = stop.clone();
        let audio = std::thread::spawn(move || {
            let (as_, libp) = (as_, libp);
            let lib = &*(libp.0 as *const Lib);
            let (mut chunks, mut got): (Vec<(f64, Vec<i16>)>, bool) = (vec![], false);
            while !stop_a.load(Ordering::Relaxed) {
                let sample = (lib.pull)(as_.0, 50_000_000);
                if sample.is_null() {
                    if got && (lib.is_eos)(as_.0) != 0 {
                        break;
                    }
                    continue;
                }
                got = true;
                let buf = (lib.sample_buffer)(sample);
                let pts = *((buf as *const u8).add(BUFFER_PTS_OFFSET) as *const u64);
                let rt = (lib.to_running_time)((lib.sample_segment)(sample), FORMAT_TIME, pts);
                if pts != CLOCK_NONE && rt != CLOCK_NONE {
                    if let Some(m) = Mapped::new(lib, buf) {
                        let s = m.bytes().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                        chunks.push((rt as f64 / SECOND, s));
                    }
                }
                (lib.mini_unref)(sample);
            }
            chunks
        });

        (lib.set_state)(pipeline, STATE_PLAYING);
        let bus = (lib.get_bus)(pipeline);
        loop {
            if duration.is_none() {
                let mut d = 0i64;
                if (lib.query_duration)(pipeline, FORMAT_TIME, &mut d) != 0 && d > 0 {
                    duration = Some(d as f64 / SECOND);
                }
            }
            let msg = (lib.bus_pop)(bus, 20_000_000, MSG_EOS | MSG_ERROR);
            if !msg.is_null() {
                let kind = *((msg as *const u8).add(MESSAGE_TYPE_OFFSET) as *const c_int);
                if kind == MSG_ERROR {
                    let (mut e, mut dbg): (*mut GError, *mut c_char) = (std::ptr::null_mut(), std::ptr::null_mut());
                    (lib.parse_error)(msg, &mut e, &mut dbg);
                    if !e.is_null() {
                        logs.push(CStr::from_ptr((*e).message).to_string_lossy().into_owned());
                        (lib.error_free)(e);
                    }
                    if !dbg.is_null() {
                        (lib.free)(dbg as *mut c_void);
                    }
                    code = 1.into();
                }
                (lib.mini_unref)(msg);
                break;
            }
            if t0.elapsed().as_secs_f64() > o.timeout {
                timed_out = true;
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let (s, sz) = video.join().unwrap_or((None, None));
        chunks = audio.join().unwrap_or_default();
        (lib.set_state)(pipeline, STATE_NULL);
        (lib.object_unref)(bus);
        (lib.object_unref)(vsink);
        (lib.object_unref)(asink);
        (lib.object_unref)(pipeline);
        (frames, held) = reader.join().unwrap_or_default();
        sar = s;
        size = sz;
    }
    let wall = t0.elapsed().as_secs_f64();
    let cap = format!("playbin3, appsink I420{}", size.map(|(w, h)| format!(" {w}x{h}")).unwrap_or_default());
    events.push(json!({"t": "meta", "player": ver.replace("GStreamer ", "gstreamer "), "capture": cap,
                       "duration": duration, "sar": sar, "held_redisplays": held}));
    events.extend(frames);
    if !chunks.is_empty() {
        // beep onsets mapped back to the running time of the buffer they're in
        let (mut mono, mut marks) = (Vec::<i16>::new(), vec![]);
        for (t, s) in &chunks {
            marks.push((mono.len(), *t));
            mono.extend_from_slice(s);
        }
        let beeps: Vec<f64> = find_beeps(&mono, RATE, 0.0).iter().map(|b| {
            let idx = (b * RATE as f64) as usize;
            let (start, t) = marks.iter().rev().find(|m| m.0 <= idx).copied().unwrap_or((0, 0.0));
            pyfmt::round_to(t + (idx - start) as f64 / RATE as f64, 4)
        }).collect();
        events.push(json!({"t": "audio", "start": chunks[0].0, "beeps": beeps}));
    }
    for l in logs {
        events.push(json!({"t": "log", "level": "error", "msg": l}));
    }
    let code: Value = if timed_out { "timeout".into() } else { code };
    events.push(json!({"t": "exit", "code": code, "wall": pyfmt::round_to(wall, 2)}));
    events
}
