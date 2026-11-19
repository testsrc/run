//! mpv as the player, through libmpv (libmpv.so.2, loaded at run time), in real time.
//!
//! Video: libmpv's software renderer (the render API with MPV_RENDER_API_TYPE_SW), RGB converted by mpv itself,
//! at --width with the display aspect (mpv applies the pixel aspect and the rotation, so the pixels are square).
//! mpv blocks each render until the frame's display time, so the time a render returns is when a viewer would
//! see the frame.
//!
//! Audio: libmpv has no audio callback, and its PCM writer isn't timed. So the adapter starts a private JACK
//! server with the dummy driver (a sound card that keeps time without hardware), mpv plays into it with its JACK
//! output, and the adapter records mpv's ports from its own JACK client, dating every sample with JACK's clock.
//! Frames are dated with the same clock (jack_get_time), so the A/V offset is the one a viewer would see. Without
//! jackd, mpv plays through its null output (timed, silent) and no audio is recorded.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use testsrc_core::{find_beeps, pyfmt, Reader, RgbFrame};

const RATE: u32 = 44100;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_LOG: c_int = 2;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const RENDER_API_TYPE: c_int = 1;
const RENDER_SW_SIZE: c_int = 17;
const RENDER_SW_FORMAT: c_int = 18;
const RENDER_SW_STRIDE: c_int = 19;
const RENDER_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;
const END_EOF: c_int = 0;
const END_ERROR: c_int = 4;

type P = *mut c_void;

#[repr(C)]
struct Event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: P,
}

#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
}

#[repr(C)]
struct LogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}

#[repr(C)]
struct Param {
    kind: c_int,
    data: P,
}

struct Mpv {
    _lib: libloading::Library,
    create: unsafe extern "C" fn() -> P,
    initialize: unsafe extern "C" fn(P) -> c_int,
    set_option: unsafe extern "C" fn(P, *const c_char, *const c_char) -> c_int,
    set_property: unsafe extern "C" fn(P, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(P, *const *const c_char) -> c_int,
    wait_event: unsafe extern "C" fn(P, f64) -> *const Event,
    get_property: unsafe extern "C" fn(P, *const c_char, c_int, P) -> c_int,
    get_property_string: unsafe extern "C" fn(P, *const c_char) -> *mut c_char,
    request_log: unsafe extern "C" fn(P, *const c_char) -> c_int,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    free: unsafe extern "C" fn(P),
    terminate: unsafe extern "C" fn(P),
    render_create: unsafe extern "C" fn(*mut P, P, *const Param) -> c_int,
    render_set_update: unsafe extern "C" fn(P, Option<unsafe extern "C" fn(P)>, P),
    render_update: unsafe extern "C" fn(P) -> u64,
    render: unsafe extern "C" fn(P, *const Param) -> c_int,
    report_swap: unsafe extern "C" fn(P),
    render_free: unsafe extern "C" fn(P),
}

#[cfg(not(windows))]
const MPV_LIB: &str = "libmpv.so.2";
#[cfg(windows)]
const MPV_LIB: &str = "libmpv-2.dll";

impl Mpv {
    fn load() -> Result<Mpv, String> {
        let name = std::env::var("LIBMPV").unwrap_or_else(|_| MPV_LIB.into());
        let lib = unsafe { libloading::Library::new(&name) }.map_err(|e| format!("can't load {name}: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Mpv {
            create: sym!("mpv_create"),
            initialize: sym!("mpv_initialize"),
            set_option: sym!("mpv_set_option_string"),
            set_property: sym!("mpv_set_property_string"),
            command: sym!("mpv_command"),
            wait_event: sym!("mpv_wait_event"),
            get_property: sym!("mpv_get_property"),
            get_property_string: sym!("mpv_get_property_string"),
            request_log: sym!("mpv_request_log_messages"),
            error_string: sym!("mpv_error_string"),
            free: sym!("mpv_free"),
            terminate: sym!("mpv_terminate_destroy"),
            render_create: sym!("mpv_render_context_create"),
            render_set_update: sym!("mpv_render_context_set_update_callback"),
            render_update: sym!("mpv_render_context_update"),
            render: sym!("mpv_render_context_render"),
            report_swap: sym!("mpv_render_context_report_swap"),
            render_free: sym!("mpv_render_context_free"),
            _lib: lib,
        })
    }

    unsafe fn string(&self, h: P, name: &CStr) -> Option<String> {
        let p = (self.get_property_string)(h, name.as_ptr());
        if p.is_null() {
            return None;
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        (self.free)(p as P);
        Some(s)
    }

    unsafe fn int(&self, h: P, name: &CStr) -> Option<i64> {
        let mut v = 0i64;
        ((self.get_property)(h, name.as_ptr(), FORMAT_INT64, &mut v as *mut i64 as P) >= 0).then_some(v)
    }

    unsafe fn flag(&self, h: P, name: &CStr) -> Option<bool> {
        let mut v: c_int = 0;
        ((self.get_property)(h, name.as_ptr(), FORMAT_FLAG, &mut v as *mut c_int as P) >= 0).then_some(v != 0)
    }

    unsafe fn double(&self, h: P, name: &CStr) -> Option<f64> {
        let mut v = 0f64;
        ((self.get_property)(h, name.as_ptr(), FORMAT_DOUBLE, &mut v as *mut f64 as P) >= 0).then_some(v)
    }
}

// ---------------------------------------------------------------- JACK: a timed sound card to record mpv from

struct Jack {
    _lib: libloading::Library,
    client_open: unsafe extern "C" fn(*const c_char, c_int, *mut c_int, ...) -> P,
    client_close: unsafe extern "C" fn(P) -> c_int,
    port_register: unsafe extern "C" fn(P, *const c_char, *const c_char, u64, u64) -> P,
    set_process: unsafe extern "C" fn(P, unsafe extern "C" fn(u32, P) -> c_int, P) -> c_int,
    activate: unsafe extern "C" fn(P) -> c_int,
    deactivate: unsafe extern "C" fn(P) -> c_int,
    port_buffer: unsafe extern "C" fn(P, u32) -> *const f32,
    last_frame_time: unsafe extern "C" fn(P) -> u32,
    frames_to_time: unsafe extern "C" fn(P, u32) -> u64,
    get_time: unsafe extern "C" fn() -> u64,
    sample_rate: unsafe extern "C" fn(P) -> u32,
}

impl Jack {
    fn load() -> Result<Jack, String> {
        #[cfg(not(windows))]
        let name = "libjack.so.0";
        #[cfg(windows)]
        let name = "libjack64.dll";
        let lib = unsafe { libloading::Library::new(name) }.map_err(|e| format!("can't load {name}: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Jack {
            client_open: sym!("jack_client_open"),
            client_close: sym!("jack_client_close"),
            port_register: sym!("jack_port_register"),
            set_process: sym!("jack_set_process_callback"),
            activate: sym!("jack_activate"),
            deactivate: sym!("jack_deactivate"),
            port_buffer: sym!("jack_port_get_buffer"),
            last_frame_time: sym!("jack_last_frame_time"),
            frames_to_time: sym!("jack_frames_to_time"),
            get_time: sym!("jack_get_time"),
            sample_rate: sym!("jack_get_sample_rate"),
            _lib: lib,
        })
    }
}

/// What the JACK process callback fills: per cycle, the JACK time (µs) of its first sample and the mono mix.
struct Recorder {
    jack: *const Jack,
    client: P,
    ports: [P; 2],
    cycles: Mutex<Vec<(u64, Vec<i16>)>>,
}

unsafe extern "C" fn process(n: u32, arg: P) -> c_int {
    let r = &*(arg as *const Recorder);
    let j = &*r.jack;
    let (a, b) = ((j.port_buffer)(r.ports[0], n), (j.port_buffer)(r.ports[1], n));
    if a.is_null() || b.is_null() {
        return 0;
    }
    let (a, b) = (std::slice::from_raw_parts(a, n as usize), std::slice::from_raw_parts(b, n as usize));
    let mono: Vec<i16> = a.iter().zip(b).map(|(x, y)| (((x + y) / 2.0).clamp(-1.0, 1.0) * 32767.0) as i16).collect();
    if mono.iter().any(|&s| s != 0) {
        // what a cycle produces goes out once the cycle ends: its first sample plays at the next cycle's start
        // (with no hardware after it, the dummy driver's sound card has no further latency)
        let t = (j.frames_to_time)(r.client, (j.last_frame_time)(r.client).wrapping_add(n));
        r.cycles.lock().unwrap().push((t, mono));
    }
    0
}

/// A private jackd (dummy driver) and our recording client on it
struct Sound {
    server: Child,
    jack: Box<Jack>,
    rec: Box<Recorder>,
}

impl Sound {
    fn start(name: &str) -> Result<Sound, String> {
        let jack = Box::new(Jack::load()?);
        let server = Command::new("jackd")
            // no realtime scheduling, at most 16 ports (the default 2048 needs ~107 MB of /dev/shm, more than
            // Docker's 64 MB); the dummy driver: 44.1 kHz, 256-frame periods (5.8 ms), stereo playback only
            .args(["-n", name, "-r", "-p", "16", "-d", "dummy", "-r", &RATE.to_string(), "-p", "256", "-C", "0", "-P", "2"])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("can't start jackd: {e}"))?;
        let cname = CString::new("testsrc").unwrap();
        let sname = CString::new(name).unwrap();
        let mut client: P = std::ptr::null_mut();
        let t0 = Instant::now();
        while client.is_null() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
            let mut status = 0;
            // JackNoStartServer | JackServerName
            client = unsafe { (jack.client_open)(cname.as_ptr(), 0x01 | 0x04, &mut status, sname.as_ptr()) };
        }
        let mut sound = Sound {
            server, jack,
            rec: Box::new(Recorder { jack: std::ptr::null(), client, ports: [std::ptr::null_mut(); 2], cycles: Mutex::new(vec![]) }),
        };
        if client.is_null() {
            return Err("jackd didn't start".into());
        }
        sound.rec.jack = &*sound.jack;
        let ty = CString::new("32 bit float mono audio").unwrap();
        for (i, port) in ["in_1", "in_2"].iter().enumerate() {
            let pn = CString::new(*port).unwrap();
            sound.rec.ports[i] = unsafe { (sound.jack.port_register)(client, pn.as_ptr(), ty.as_ptr(), 0x1, 0) };
        }
        if sound.rec.ports.iter().any(|p| p.is_null()) {
            return Err("can't register JACK ports".into());
        }
        let arg = &*sound.rec as *const Recorder as P;
        unsafe {
            (sound.jack.set_process)(client, process, arg);
            (sound.jack.activate)(client);
        }
        if unsafe { (sound.jack.sample_rate)(client) } != RATE {
            return Err("jackd runs at another rate".into());
        }
        Ok(sound)
    }

    fn now(&self) -> f64 {
        unsafe { (self.jack.get_time)() as f64 / 1e6 }
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        if !self.rec.client.is_null() {
            unsafe {
                (self.jack.deactivate)(self.rec.client);
                (self.jack.client_close)(self.rec.client);
            }
        }
        // SIGTERM, so jackd frees its shared memory (~38 MB of /dev/shm); killed, it leaves it behind and the next
        // case's jackd doesn't fit in Docker's 64 MB
        #[cfg(unix)]
        let _ = Command::new("kill").args(["-TERM", &self.server.id().to_string()]).status();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if let Ok(Some(_)) = self.server.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

// ---------------------------------------------------------------- the player

pub struct Options {
    pub timeout: f64,
    pub width: u32,
}

#[derive(Clone, Copy)]
struct Ptr(P);
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

/// The update callback only says "there may be a frame": the render thread does the rest
struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

unsafe extern "C" fn on_update(arg: P) {
    let w = &*(arg as *const Wake);
    *w.flag.lock().unwrap() = true;
    w.cv.notify_one();
}

/// Whether mpv flips this file vertically (a mirrored display matrix is read as a 180-degree turn plus a vertical
/// flip). mpv doesn't expose the flip; with an output that can't flip it inserts its autovflip filter and logs it,
/// so open the file once with null outputs and watch the log.
unsafe fn mpv_flips(mpv: &Mpv, path: &CStr) -> bool {
    let h = (mpv.create)();
    for (k, v) in [("config", "no"), ("terminal", "no"), ("idle", "yes"), ("pause", "yes"), ("vo", "null"), ("ao", "null"),
                   ("ytdl", "no")] {
        let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
        (mpv.set_option)(h, k.as_ptr(), v.as_ptr());
    }
    (mpv.request_log)(h, c"v".as_ptr());
    if (mpv.initialize)(h) < 0 {
        (mpv.terminate)(h);
        return false;
    }
    let cmd = [c"loadfile".as_ptr(), path.as_ptr(), std::ptr::null()];
    (mpv.command)(h, cmd.as_ptr());
    let (t0, mut flips) = (Instant::now(), false);
    while t0.elapsed() < Duration::from_secs(5) {
        let ev = &*(mpv.wait_event)(h, 0.1);
        match ev.event_id {
            EVENT_LOG if !ev.data.is_null() => {
                let m = &*(ev.data as *const LogMessage);
                if CStr::from_ptr(m.prefix).to_bytes() == b"autovflip"
                    && CStr::from_ptr(m.text).to_string_lossy().contains("Inserting vflip") {
                    flips = true;
                }
            }
            EVENT_VIDEO_RECONFIG | EVENT_PLAYBACK_RESTART | EVENT_END_FILE | EVENT_SHUTDOWN => break,
            // no video, nothing to flip
            EVENT_FILE_LOADED if mpv.string(h, c"vid").as_deref() == Some("no") => break,
            _ => {}
        }
    }
    (mpv.terminate)(h);
    flips
}

pub fn play(path: &str, o: &Options) -> Vec<Value> {
    let mpv = match Mpv::load() {
        Ok(l) => l,
        Err(e) => return vec![json!({"t": "meta", "player": "mpv"}), json!({"t": "log", "level": "error", "msg": e})],
    };
    let mut logs: Vec<(String, String)> = vec![];
    let server = format!("testsrc-{}", std::process::id());
    let sound = match Sound::start(&server) {
        Ok(s) => Some(s),
        Err(e) => {
            logs.push(("info".into(), format!("audio not recorded: {e}")));
            None
        }
    };
    let t0 = Instant::now();

    let (mut duration, mut code, mut timed_out, mut ver) = (None::<f64>, Value::from(0), false, String::from("mpv"));
    let (frames, size, held, rotation): (Vec<Value>, _, usize, Option<String>);
    unsafe {
        let h = (mpv.create)();
        let opts: Vec<(&str, String)> = vec![
            ("config", "no".into()), ("terminal", "no".into()), ("idle", "yes".into()),
            // start paused and play once the first frame is drawn: the software renderer sets itself up on the first
            // frame (tens of ms), which a GPU output does before playback, and mpv would drop frames to catch up
            ("pause", "yes".into()),
            // as a player window does, keep the last frame on screen at the end (eof-reached says when)
            ("keep-open", "yes".into()),
            ("input-default-bindings", "no".into()), ("osd-level", "0".into()), ("ytdl", "no".into()),
            ("vo", "libmpv".into()), ("hwdec", "no".into()),
            ("ao", if sound.is_some() { "jack".into() } else { "null".into() }),
            ("jack-port", "testsrc:in_.*".into()), ("jack-autostart", "no".into()), ("jack-name", "mpv".into()),
            // the sound card is stereo: mpv downmixes, as it does for a stereo device (one JACK port per channel)
            ("audio-channels", "stereo".into()),
        ];
        if sound.is_some() {
            std::env::set_var("JACK_DEFAULT_SERVER", &server);
        }
        for (k, v) in &opts {
            let (k, v) = (CString::new(*k).unwrap(), CString::new(v.as_str()).unwrap());
            (mpv.set_option)(h, k.as_ptr(), v.as_ptr());
        }
        (mpv.request_log)(h, c"error".as_ptr());
        if (mpv.initialize)(h) < 0 {
            (mpv.terminate)(h);
            return vec![json!({"t": "meta", "player": "mpv"}), json!({"t": "log", "level": "error", "msg": "mpv_initialize failed"}),
                        json!({"t": "exit", "code": 1, "wall": 0.0})];
        }
        ver = mpv.string(h, c"mpv-version").map(|v| v.replace("mpv v", "mpv ")).unwrap_or(ver);

        let api = CString::new("sw").unwrap();
        let params = [Param { kind: RENDER_API_TYPE, data: api.as_ptr() as P }, Param { kind: 0, data: std::ptr::null_mut() }];
        let mut ctx: P = std::ptr::null_mut();
        (mpv.render_create)(&mut ctx, h, params.as_ptr());
        let wake = Arc::new(Wake { flag: Mutex::new(false), cv: Condvar::new() });
        (mpv.render_set_update)(ctx, Some(on_update), Arc::as_ptr(&wake) as P);

        // the render size, set when the video (re)configures: display aspect, rotation applied, at most --width
        let target = Arc::new(Mutex::new(None::<(usize, usize)>));
        let stop = Arc::new(AtomicBool::new(false));
        let drawn = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(f64, usize, usize, Vec<u8>)>();
        let reader = std::thread::spawn(move || {
            let (mut reader, mut out, mut last, mut held) = (Reader::new(), vec![], None, 0usize);
            for (n, (t, w, h, raw)) in rx.into_iter().enumerate() {
                let r = reader.read(&RgbFrame::with_layout(&raw, w, h, 4, w * 4, "rgbx"), n % 25 == 0);
                // mpv draws a frame again when it pauses on it (the last one, kept on screen): a held frame, not a
                // repeat, as with libvlc. Each frame is reported once, when it first appeared.
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
        let (mp, cx) = (Ptr(&mpv as *const Mpv as P), Ptr(ctx));
        let (wake_r, target_r, stop_r, drawn_r) = (wake.clone(), target.clone(), stop.clone(), drawn.clone());
        let snd = Ptr(sound.as_ref().map(|s| s as *const Sound as P).unwrap_or(std::ptr::null_mut()));
        let render = std::thread::spawn(move || {
            let (mp, cx, snd) = (mp, cx, snd);
            let mpv = &*(mp.0 as *const Mpv);
            let now = || if snd.0.is_null() { t0.elapsed().as_secs_f64() } else { (*(snd.0 as *const Sound)).now() };
            let mut buf: Vec<u8> = vec![];
            let fmt = CString::new("rgb0").unwrap();
            while !stop_r.load(Ordering::Relaxed) {
                {
                    let g = wake_r.flag.lock().unwrap();
                    let (mut g, _) = wake_r.cv.wait_timeout_while(g, Duration::from_millis(50), |f| !*f).unwrap();
                    *g = false;
                }
                if (mpv.render_update)(cx.0) & UPDATE_FRAME == 0 {
                    continue;
                }
                let Some((w, h)) = *target_r.lock().unwrap() else { continue };
                buf.resize(w * h * 4, 0);
                let mut sz = [w as c_int, h as c_int];
                let mut stride = w * 4;
                let params = [
                    Param { kind: RENDER_SW_SIZE, data: sz.as_mut_ptr() as P },
                    Param { kind: RENDER_SW_FORMAT, data: fmt.as_ptr() as P },
                    Param { kind: RENDER_SW_STRIDE, data: &mut stride as *mut usize as P },
                    Param { kind: RENDER_SW_POINTER, data: buf.as_mut_ptr() as P },
                    Param { kind: 0, data: std::ptr::null_mut() },
                ];
                // blocks until the frame's display time
                if (mpv.render)(cx.0, params.as_ptr()) >= 0 {
                    let t = now();
                    (mpv.report_swap)(cx.0);
                    let _ = tx.send((t, w, h, buf.clone()));
                    drawn_r.store(true, Ordering::Relaxed);
                }
            }
        });

        let file = CString::new(path).unwrap();
        let flips = mpv_flips(&mpv, &file);
        let cmd = [c"loadfile".as_ptr(), file.as_ptr(), std::ptr::null()];
        (mpv.command)(h, cmd.as_ptr());
        let mut eof_at = None::<Instant>;
        let (mut rotated, mut filter) = (false, None::<String>);
        let (mut playing, loaded) = (false, Instant::now());
        loop {
            // the end: once mpv has reached it, give the last frame time to render
            if eof_at.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
                break;
            }
            let ev = &*(mpv.wait_event)(h, 0.05);
            match ev.event_id {
                EVENT_VIDEO_RECONFIG => {
                    let (dw, dh) = (mpv.int(h, c"video-out-params/dw"), mpv.int(h, c"video-out-params/dh"));
                    let rot = mpv.int(h, c"video-out-params/rotate").unwrap_or(0).rem_euclid(360);
                    if (rot != 0 || flips) && !rotated {
                        // libmpv 0.41.0's software renderer takes the rotation and the flip and doesn't apply them
                        // (180, flips) or asserts (90, 270). Do what mpv does for an output that can't: turn and flip
                        // the frames with filters (the angle and the flip are mpv's own reading of the file), and
                        // leave the renderer 0 degrees.
                        rotated = true;
                        // the order of mpv's own chain: autovflip, then autorotate
                        let turn = match rot { 90 => "transpose=clock", 180 => "hflip,vflip", 270 => "transpose=cclock", _ => "" };
                        let f = [if flips { "vflip" } else { "" }, turn].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(",");
                        if !f.is_empty() {
                            let spec = CString::new(format!("@testsrc-rotate:lavfi=[{f}]")).unwrap();
                            let cmd = [c"vf".as_ptr(), c"add".as_ptr(), spec.as_ptr(), std::ptr::null()];
                            (mpv.command)(h, cmd.as_ptr());
                            let undo = CString::new(((360 - rot) % 360).to_string()).unwrap();
                            (mpv.set_property)(h, c"video-rotate".as_ptr(), undo.as_ptr());
                            filter = Some(format!("{}{rot} degrees by filter", if flips { "flipped and " } else { "" }));
                            continue; // the next reconfig has the turned size
                        }
                    }
                    if let (Some(mut dw), Some(mut dh)) = (dw, dh) {
                        if rot % 180 == 90 {
                            std::mem::swap(&mut dw, &mut dh);
                        }
                        let w = dw.min(o.width as i64).max(2);
                        let hh = (pyfmt::round(dh as f64 * w as f64 / dw as f64 / 2.0) * 2).max(2);
                        *target.lock().unwrap() = Some((w as usize, hh as usize));
                    }
                }
                EVENT_LOG if !ev.data.is_null() => {
                    let m = &*(ev.data as *const LogMessage);
                    let text = CStr::from_ptr(m.text).to_string_lossy().trim().to_string();
                    let prefix = CStr::from_ptr(m.prefix).to_string_lossy();
                    logs.push(("error".into(), format!("[{prefix}] {text}")));
                }
                EVENT_END_FILE if !ev.data.is_null() => {
                    let e = &*(ev.data as *const EndFile);
                    if e.reason == END_ERROR {
                        logs.push(("error".into(), CStr::from_ptr((mpv.error_string)(e.error)).to_string_lossy().into_owned()));
                        code = 1.into();
                    } else if e.reason != END_EOF {
                        code = e.reason.into();
                    }
                    break;
                }
                EVENT_SHUTDOWN => break,
                _ => {}
            }
            if duration.is_none() {
                duration = mpv.double(h, c"duration").filter(|d| *d > 0.0);
            }
            // play: once the first frame is drawn, at once for files without video, or after 3 s whatever happens
            if !playing && (drawn.load(Ordering::Relaxed) || mpv.string(h, c"vid").as_deref() == Some("no")
                            || loaded.elapsed() > Duration::from_secs(3)) {
                (mpv.set_property)(h, c"pause".as_ptr(), c"no".as_ptr());
                playing = true;
            }
            if playing && eof_at.is_none() && mpv.flag(h, c"eof-reached") == Some(true) {
                eof_at = Some(Instant::now());
            }
            if t0.elapsed().as_secs_f64() > o.timeout {
                timed_out = true;
                break;
            }
        }
        size = *target.lock().unwrap();
        rotation = filter;
        stop.store(true, Ordering::Relaxed);
        on_update(Arc::as_ptr(&wake) as P);
        let _ = render.join();
        (mpv.render_free)(ctx);
        (mpv.terminate)(h);
        (frames, held) = reader.join().unwrap_or_default();
    }
    let wall = t0.elapsed().as_secs_f64();
    let cap = format!("render API (software), RGB{}{}{}", size.map(|(w, h)| format!(" {w}x{h}")).unwrap_or_default(),
                      rotation.map(|r| format!(", rotation {r}")).unwrap_or_default(),
                      if sound.is_some() { ", audio through JACK (dummy driver)" } else { ", audio not recorded" });
    let mut events = vec![json!({"t": "meta", "player": ver, "capture": cap, "duration": duration, "sar": "1:1",
                                 "held_redisplays": held})];
    events.extend(frames);
    if let Some(s) = &sound {
        let cycles = std::mem::take(&mut *s.rec.cycles.lock().unwrap());
        if !cycles.is_empty() {
            let (mut mono, mut marks) = (Vec::<i16>::new(), vec![]);
            for (t, c) in &cycles {
                marks.push((mono.len(), *t as f64 / 1e6));
                mono.extend_from_slice(c);
            }
            let beeps: Vec<f64> = find_beeps(&mono, RATE, 0.0).iter().map(|b| {
                let idx = (b * RATE as f64) as usize;
                let (start, t) = marks.iter().rev().find(|m| m.0 <= idx).copied().unwrap_or((0, 0.0));
                pyfmt::round_to(t + (idx - start) as f64 / RATE as f64, 4)
            }).collect();
            events.push(json!({"t": "audio", "start": cycles[0].0 as f64 / 1e6, "beeps": beeps}));
        }
    }
    for (level, msg) in logs {
        events.push(json!({"t": "log", "level": level, "msg": msg}));
    }
    let code: Value = if timed_out { "timeout".into() } else { code };
    events.push(json!({"t": "exit", "code": code, "wall": pyfmt::round_to(wall, 2)}));
    drop(sound);
    events
}
