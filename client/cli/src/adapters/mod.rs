//! Players. Each one turns "play this file" into events (see testsrc_core::check); the checker does the rest.

pub mod cmd;
pub mod ffmpeg;
pub mod gstreamer;
pub mod libvlc;
pub mod mpv;

use serde_json::{json, Value};

/// What to do when an engine isn't on the machine: the install line for Debian and Ubuntu (GitHub's Linux runners),
/// or where the runner looks on Windows, and the variable that points at your own build.
pub fn install_hint(engine: &str) -> &'static str {
    if cfg!(windows) {
        match engine {
            "libvlc" => "Install VLC 3.0 in C:\\Program Files\\VideoLAN\\VLC, or set VLC_DIR to the folder with libvlc.dll",
            "ffmpeg" => "Put ffmpeg.exe and ffprobe.exe (5.1 or later, the build your player links) on PATH",
            "gstreamer" => "Install GStreamer 1.x (MSVC x86_64); the runner reads GSTREAMER_1_0_ROOT_MSVC_X86_64",
            "mpv" => "Put libmpv-2.dll on PATH, or set LIBMPV to its path",
            _ => "Install JACK (libjack64.dll and jackd on PATH)",
        }
    } else {
        match engine {
            "libvlc" => "Install it: sudo apt-get install -y libvlc5 vlc-plugin-base (Debian, Ubuntu), or set LIBVLC to your build's libvlc.so.5",
            "ffmpeg" => "Put ffmpeg and ffprobe 5.1 or later on PATH, the build your player links (Debian, Ubuntu: sudo apt-get install -y ffmpeg)",
            "gstreamer" => "Install it: sudo apt-get install -y libgstreamer1.0-0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav (Debian, Ubuntu)",
            "mpv" => "Install it: sudo apt-get install -y libmpv2 (Debian 12: libmpv2; Ubuntu 22.04: libmpv1 is too old), or set LIBMPV to your build's libmpv.so.2",
            _ => "Install JACK: sudo apt-get install -y jackd2 (Debian, Ubuntu)",
        }
    }
}

/// The events of a player that couldn't start at all: its name, and the error with what to do, marked `setup` so
/// the runner says it once for the whole run rather than as each case's failure.
pub fn not_installed(player: &str, engine: &str, err: &str) -> Vec<Value> {
    vec![json!({"t": "meta", "player": player}),
         json!({"t": "log", "level": "error", "setup": true, "msg": format!("{}. {}", err.trim_end_matches('.'), install_hint(engine))})]
}

/// libvlc's error lines that say nothing about the file, left out once frames were shown (without frames they stay:
/// they may say why nothing was). VLC opens an audio output of its own before the callbacks make it amem, and
/// without a PulseAudio server (CI machines) that one fails. And libvlc 3.0.20 sets the video output up for a
/// hardware decoder's pictures (VA-API, VDPAU), can't convert them for the callbacks and falls back to software
/// decoding, even with --avcodec-hw=none: about 40 lines a file on Ubuntu, where libavcodec offers those formats.
const LIBVLC_NOISE: [&str; 6] = [
    "vlcpulse error: PulseAudio server connection failure",
    "main error: Failed to create video converter",
    "chain error: Too high level of recursion",
    "main error: video output creation failed",
    "main error: failed to create video output",
    "main error: Failed to adapt decoder format to display",
];

/// An adapter's events, made plain before they are written: a player that couldn't load (its meta names the bare
/// engine, its error says "can't load") becomes a `setup` error with what to install, and libvlc's noise goes.
pub fn tidy(engine: &str, events: &mut Vec<Value>) {
    let bare = events.iter().any(|e| e["t"] == "meta" && e["player"] == engine);
    for e in events.iter_mut() {
        let msg = e["msg"].as_str().unwrap_or("");
        if bare && e["t"] == "log" && e["level"] == "error" && e["setup"].is_null() && msg.starts_with("can't load ") {
            e["msg"] = format!("{}. {}", msg.trim_end_matches('.'), install_hint(engine)).into();
            e["setup"] = true.into();
        }
    }
    if engine == "libvlc" && events.iter().any(|e| e["t"] == "frame") {
        events.retain(|e| !(e["t"] == "log" && LIBVLC_NOISE.iter().any(|n| e["msg"].as_str().unwrap_or("").starts_with(n))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(msg: &str) -> Value {
        json!({"t": "log", "level": "error", "msg": msg})
    }

    #[test]
    fn an_engine_that_didnt_load_says_what_to_install() {
        let mut e = vec![json!({"t": "meta", "player": "libvlc"}), log("can't load libvlc: libvlc.so.5: cannot open shared object file.")];
        tidy("libvlc", &mut e);
        assert_eq!(e[1]["setup"], true);
        assert_eq!(e[1]["msg"], format!("can't load libvlc: libvlc.so.5: cannot open shared object file. {}", install_hint("libvlc")));
        // a loaded engine's "can't load" (a plugin, say) is the file's business
        let mut e = vec![json!({"t": "meta", "player": "gstreamer 1.24.2"}), log("can't load the subtitle font")];
        tidy("gstreamer", &mut e);
        assert!(e[1]["setup"].is_null());
    }

    #[test]
    fn libvlc_noise_goes_once_frames_were_shown() {
        let lines = || vec![json!({"t": "meta", "player": "libvlc 3.0.20"}), log("vlcpulse error: PulseAudio server connection failure: Connection refused"),
                            log("main error: Failed to create video converter"), log("chain error: Too high level of recursion (3)"),
                            log("freetype error: LoadFace: Error creating face")];
        let mut shown = lines();
        shown.push(json!({"t": "frame", "pts": 0.0}));
        tidy("libvlc", &mut shown);
        let msgs: Vec<&str> = shown.iter().filter_map(|e| e["msg"].as_str()).collect();
        assert_eq!(msgs, ["freetype error: LoadFace: Error creating face"]);
        let mut none = lines();
        tidy("libvlc", &mut none);
        assert_eq!(none.len(), 5, "nothing shown: every line stays");
        let mut other = lines();
        other.push(json!({"t": "frame", "pts": 0.0}));
        tidy("mpv", &mut other);
        assert_eq!(other.len(), 6, "only libvlc's");
    }

    #[test]
    fn each_engine_says_how_to_install_it() {
        for (engine, word) in [("libvlc", "libvlc5"), ("ffmpeg", "ffprobe"), ("gstreamer", "gstreamer1.0-libav"), ("mpv", "libmpv2"), ("jack", "jackd2")] {
            assert!(install_hint(engine).contains(word), "{engine}");
        }
        let e = not_installed("mpv", "mpv", "can't load libmpv.so.2: cannot open shared object file.");
        assert_eq!(e[0]["player"], "mpv");
        assert_eq!(e[1]["setup"], true);
        assert!(e[1]["msg"].as_str().unwrap().starts_with("can't load libmpv.so.2: cannot open shared object file. Install it: sudo apt-get"));
    }
}
