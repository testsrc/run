//! The player matrix: several engines' results.json for one snapshot, as a Markdown dashboard (the matrix
//! repo's README). Engines across, case categories down, then every case that didn't pass, with the
//! checker's reason.

use std::collections::BTreeMap;

use serde_json::Value;

pub struct Options {
    /// Where "test your own player" points
    pub site: String,
    /// A case page URL with {id}, if case pages exist
    pub case_url: Option<String>,
}

struct Engine<'a> {
    short: String,
    doc: &'a Value,
}

fn status_of(r: &Value) -> &str {
    r["status"].as_str().unwrap_or("")
}

/// "libvlc 3.0.24 Vetinari (YUV callbacks)" -> "libvlc 3.0.24"; "ffmpeg 7.1.5-0+deb13u1" -> "ffmpeg 7.1.5"
fn short_name(player: &str) -> String {
    let mut it = player.split_whitespace();
    let name = it.next().unwrap_or("?");
    let ver: String = it.next().unwrap_or("").chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    if ver.is_empty() { name.to_string() } else { format!("{name} {ver}") }
}

fn category(id: &str) -> &str {
    id.split_once('/').map(|(c, _)| c).unwrap_or("other")
}

fn cell(pass: usize, total: usize) -> String {
    match (pass, total) {
        (_, 0) => "".into(),
        (p, t) if p == t => format!("✅ {p}/{t}"),
        (p, t) => format!("⚠️ {p}/{t}"),
    }
}

pub fn render(docs: &[Value], o: &Options) -> String {
    let engines: Vec<Engine> = docs.iter()
        .map(|d| Engine { short: short_name(d["player"].as_str().unwrap_or("?")), doc: d })
        .collect();
    let snapshots: Vec<&str> = {
        let mut s: Vec<&str> = docs.iter().filter_map(|d| d["snapshot"].as_str()).collect();
        s.dedup();
        s
    };
    let latest = docs.iter().filter_map(|d| d["started"].as_str()).max().unwrap_or("");
    let link = |id: &str| match &o.case_url {
        Some(u) => format!("[`{id}`]({})", u.replace("{id}", id)),
        None => format!("`{id}`"),
    };

    // category -> engine index -> (pass, played)
    let mut cats: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for (i, e) in engines.iter().enumerate() {
        for r in e.doc["results"].as_array().into_iter().flatten() {
            if status_of(r) == "skipped" {
                continue;
            }
            let c = cats.entry(category(r["id"].as_str().unwrap_or("")).to_string()).or_insert_with(|| vec![(0, 0); engines.len()]);
            c[i].1 += 1;
            c[i].0 += (status_of(r) == "pass") as usize;
        }
    }

    let mut md = String::from("# Player matrix\n\n");
    md += "The same test files, played through common media engines and checked frame by frame: every frame shown once \
           and in order, orientation, aspect, colour, timestamps and audio/video sync. Files and checker by \
           [testsrc](SITE).\n\n".replace("SITE", &o.site).as_str();
    md += &format!("Snapshot {}, last run {}.\n\n", snapshots.join(", "), latest.get(..10).unwrap_or(latest));

    md += "| Category |";
    for e in &engines {
        md += &format!(" {} |", e.short);
    }
    md += "\n| --- |";
    md += &" --- |".repeat(engines.len());
    md += "\n";
    let mut totals = vec![(0, 0); engines.len()];
    for (cat, per) in &cats {
        let n = per.iter().map(|p| p.1).max().unwrap_or(0);
        md += &format!("| {cat} ({n}) |");
        for (i, &(p, t)) in per.iter().enumerate() {
            md += &format!(" {} |", cell(p, t));
            totals[i].0 += p;
            totals[i].1 += t;
        }
        md += "\n";
    }
    md += "| **All** |";
    for &(p, t) in &totals {
        md += &format!(" **{}** |", cell(p, t));
    }
    md += "\n\n";

    let mut bad = vec![];
    for e in &engines {
        for r in e.doc["results"].as_array().into_iter().flatten() {
            let s = status_of(r);
            if s == "pass" || s == "skipped" {
                continue;
            }
            let why = r["checks"].as_array().into_iter().flatten()
                .find(|c| c[0] == "FAIL")
                .map(|c| format!("{}: {}", c[1].as_str().unwrap_or(""), c[2].as_str().unwrap_or("")))
                .unwrap_or_else(|| s.to_string());
            bad.push((r["id"].as_str().unwrap_or("").to_string(), e.short.clone(), s.to_string(), why));
        }
    }
    bad.sort();
    if !bad.is_empty() {
        md += "## What didn't pass\n\n| Case | Engine | Result | What the checker saw |\n| --- | --- | --- | --- |\n";
        for (id, eng, s, why) in &bad {
            md += &format!("| {} | {eng} | {s} | {} |\n", link(id), why.replace('|', "\\|"));
        }
        md += "\n";
    }

    md += "## Engines\n\n| Engine | Version and capture | Runner | OS |\n| --- | --- | --- | --- |\n";
    for e in &engines {
        md += &format!("| {} | {} | testsrc {} | {} {} |\n", e.short, e.doc["player"].as_str().unwrap_or(""),
                       e.doc["runner"].as_str().unwrap_or("?"), e.doc["os"].as_str().unwrap_or("?"), e.doc["arch"].as_str().unwrap_or(""));
    }
    md += &format!("\nSame files, same checker and the same rules for every engine. To see how your own player does on these files \
                    and the rest of the catalog, [start a 7-day trial on testsrc]({}).\n", o.site);
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(player: &str, results: Value) -> Value {
        json!({"runner": "0.1.0", "snapshot": "2026-10-29", "player": player, "started": "2026-10-31T10:00:00Z",
               "os": "linux", "arch": "x86_64", "results": results})
    }

    #[test]
    fn names() {
        assert_eq!(short_name("libvlc 3.0.24 Vetinari (YUV callbacks)"), "libvlc 3.0.24");
        assert_eq!(short_name("ffmpeg 7.1.5-0+deb13u1"), "ffmpeg 7.1.5");
        assert_eq!(short_name("ffmpeg"), "ffmpeg");
    }

    #[test]
    fn table_and_failures() {
        let vlc = doc("libvlc 3.0.24 Vetinari (YUV callbacks)", json!([
            {"id": "hls/ts.m3u8", "status": "fail", "checks": [["PASS", "run", "x"], ["FAIL", "frames", "179/180 frames shown, missing 179"]]},
            {"id": "hls/fmp4.m3u8", "status": "pass", "checks": []},
            {"id": "codec/h264-aac.mp4", "status": "pass", "checks": []},
            {"id": "subs/sidecar.srt", "status": "skipped", "checks": []},
        ]));
        let ff = doc("ffmpeg 7.1.5-0+deb13u1", json!([
            {"id": "hls/ts.m3u8", "status": "pass", "checks": []},
            {"id": "hls/fmp4.m3u8", "status": "crash", "checks": [["FAIL", "run", "player process ended with a signal"]]},
            {"id": "codec/h264-aac.mp4", "status": "pass", "checks": []},
        ]));
        let md = render(&[vlc, ff], &Options { site: "https://testsrc.com".into(), case_url: Some("https://testsrc.com/cases/{id}".into()) });
        assert!(md.contains("| Category | libvlc 3.0.24 | ffmpeg 7.1.5 |"));
        assert!(md.contains("| codec (1) | ✅ 1/1 | ✅ 1/1 |"));
        assert!(md.contains("| hls (2) | ⚠️ 1/2 | ⚠️ 1/2 |"));
        assert!(md.contains("| **All** | **⚠️ 2/3** | **⚠️ 2/3** |"));
        assert!(!md.contains("subs"));
        assert!(md.contains("| [`hls/ts.m3u8`](https://testsrc.com/cases/hls/ts.m3u8) | libvlc 3.0.24 | fail | frames: 179/180 frames shown, missing 179 |"));
        assert!(md.contains("| ffmpeg 7.1.5 | crash | run: player process ended with a signal |"));
        assert!(md.contains("Snapshot 2026-10-29, last run 2026-10-31."));
    }
}
