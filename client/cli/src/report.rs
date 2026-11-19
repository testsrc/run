//! Results: one line per case on the console, results.json, JUnit XML, and a table in the GitHub Actions
//! run summary when $GITHUB_STEP_SUMMARY is set.

use serde_json::{json, Value};
use testsrc_core::Line;

pub struct CaseResult {
    pub id: String,
    pub status: &'static str,
    pub wall: f64,
    pub player: Option<String>,
    pub checks: Vec<Line>,
}

impl CaseResult {
    /// The first failing check, or for a pass the first warning ("warning: ..."): what the console line shows.
    pub fn first_failure(&self) -> String {
        self.checks.iter().find(|l| l.status.name() == "FAIL").map(|l| format!("{}: {}", l.what, l.detail))
            .or_else(|| self.warnings().first().map(|w| format!("warning: {w}")))
            .unwrap_or_default()
    }

    /// The checks that warn ("what: detail"): a case passes with them, and the reports name it (partial playback of
    /// a file a player may refuse, error lines in the log).
    pub fn warnings(&self) -> Vec<String> {
        self.checks.iter().filter(|l| l.status.name() == "WARN").map(|l| format!("{}: {}", l.what, l.detail)).collect()
    }

    pub fn to_json(&self) -> Value {
        let checks: Vec<Value> = self.checks.iter().map(|l| json!([l.status.name(), l.what, l.detail])).collect();
        let mut v = json!({"id": self.id, "status": self.status, "wall": self.wall, "checks": checks});
        if let Some(p) = &self.player {
            v["player"] = p.clone().into();
        }
        let w = self.warnings();
        if !w.is_empty() {
            v["warnings"] = w.into();
        }
        v
    }
}

/// The cases that passed with warnings.
pub fn warned(results: &[CaseResult]) -> Vec<&CaseResult> {
    results.iter().filter(|r| r.status == "pass" && !r.warnings().is_empty()).collect()
}

pub const STATUSES: [&str; 5] = ["pass", "fail", "crash", "hang", "skipped"];

/// How many cases ended each way, then how many of the passes have warnings ("with warnings"): the console's
/// closing line and results.json's counts.
pub fn counts(results: &[CaseResult]) -> Vec<(&'static str, usize)> {
    let mut c: Vec<_> = STATUSES.iter().map(|&s| (s, results.iter().filter(|r| r.status == s).count())).collect();
    c.push(("with warnings", warned(results).len()));
    c
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn junit(results: &[CaseResult], suite: &str) -> String {
    let fails = results.iter().filter(|r| r.status != "pass" && r.status != "skipped").count();
    let mut out = format!("<testsuite name=\"{}\" tests=\"{}\" failures=\"{fails}\">\n", xml(suite), results.len());
    for r in results {
        out += &format!("  <testcase classname=\"{}\" name=\"{}\" time=\"{}\">\n", xml(suite), xml(&r.id), r.wall);
        let text: String = r.checks.iter().map(|l| format!("{:4}  {:12} {}", l.status.name(), l.what, l.detail)).collect::<Vec<_>>().join("\n");
        match r.status {
            "skipped" => out += &format!("    <skipped message=\"{}\"/>\n", xml(&r.checks[0].detail)),
            "pass" => out += &format!("    <system-out>{}</system-out>\n", xml(&text)),
            s => {
                let first = r.checks.iter().find(|l| l.status.name() == "FAIL").map(|l| l.detail.as_str()).unwrap_or(s);
                out += &format!("    <failure message=\"{}\">{}</failure>\n", xml(&format!("{s}: {first}")), xml(&text));
            }
        }
        out += "  </testcase>\n";
    }
    out + "</testsuite>\n"
}

/// Markdown for the GitHub Actions run summary: counts, then what didn't pass with its first failure.
pub fn github_summary(results: &[CaseResult], player: &str, snapshot: &str) -> String {
    let c = counts(results);
    let line: Vec<String> = c.iter().filter(|(_, n)| *n > 0).map(|(s, n)| format!("{n} {s}")).collect();
    let mut md = format!("### testsrc: {player}, snapshot {snapshot}\n\n{}\n\n", line.join(", "));
    let w = warned(results);
    if !w.is_empty() {
        md += &format!("**{} passed with warnings**\n\n| Case | Warning |\n| --- | --- |\n", w.len());
        for r in w {
            md += &format!("| `{}` | {} |\n", r.id, r.warnings().join("; ").replace('|', "\\|"));
        }
        md += "\n";
    }
    let bad: Vec<&CaseResult> = results.iter().filter(|r| r.status != "pass" && r.status != "skipped").collect();
    if !bad.is_empty() {
        md += "| Case | Result | First failure |\n| --- | --- | --- |\n";
        for r in bad {
            md += &format!("| `{}` | {} | {} |\n", r.id, r.status, r.first_failure().replace('|', "\\|"));
        }
        md += "\n";
    }
    let passed: Vec<String> = results.iter().filter(|r| r.status == "pass").map(|r| format!("`{}`", r.id)).collect();
    if !passed.is_empty() {
        md += &format!("<details><summary>{} passed</summary>\n\n{}\n</details>\n", passed.len(), passed.join(", "));
    }
    md
}

/// UTC time as 2026-10-30T12:34:56Z, without a date library.
pub fn utc_now() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use testsrc_core::Status;

    fn r(id: &str, status: &'static str, checks: Vec<(Status, &str, &str)>) -> CaseResult {
        CaseResult { id: id.into(), status, wall: 6.2, player: Some("p".into()),
                     checks: checks.into_iter().map(|(s, w, d)| Line { status: s, what: w.into(), detail: d.into() }).collect() }
    }

    fn sample() -> Vec<CaseResult> {
        vec![
            r("hls/ts.m3u8", "fail", vec![(Status::Pass, "run", "exit code 0"), (Status::Fail, "frames", "179/180 frames shown, missing 179")]),
            r("codec/h264-aac.mp4", "pass", vec![(Status::Pass, "run", "exit code 0")]),
            r("subs/sidecar.srt", "skipped", vec![(Status::Info, "run", "subtitle sidecar: not played on its own")]),
            r("x/<a&b>.mp4", "crash", vec![(Status::Fail, "run", "player process ended with a signal: |x|")]),
            r("damage/x-truncated-10.mp4", "pass", vec![(Status::Pass, "run", "exit code 0"),
              (Status::Warn, "frames", "partial playback, ended cleanly: 90/180 frames played, missing 90-179")]),
        ]
    }

    #[test]
    fn warnings_are_in_the_json_the_console_and_the_summary() {
        let s = sample();
        let j = s[4].to_json();
        assert_eq!(j["status"], "pass");
        assert_eq!(j["warnings"][0], "frames: partial playback, ended cleanly: 90/180 frames played, missing 90-179");
        assert!(s[1].to_json().get("warnings").is_none());
        // the console: the case's line shows its warning, the closing line counts them
        assert_eq!(s[4].first_failure(), "warning: frames: partial playback, ended cleanly: 90/180 frames played, missing 90-179");
        assert_eq!(s[1].first_failure(), "");
        assert_eq!(counts(&s).last(), Some(&("with warnings", 1)));
        assert_eq!(counts(&s[..2]).last(), Some(&("with warnings", 0)));
        let md = github_summary(&s, "mpv 0.41.0", "2026-11-11");
        assert!(md.contains("**1 passed with warnings**\n\n| Case | Warning |"));
        assert!(md.contains("| `damage/x-truncated-10.mp4` | frames: partial playback, ended cleanly: 90/180 frames played, missing 90-179 |"));
    }

    #[test]
    fn junit_report() {
        let x = junit(&sample(), "testsrc libvlc");
        assert!(x.starts_with("<testsuite name=\"testsrc libvlc\" tests=\"5\" failures=\"2\">\n"));
        assert!(x.contains("<failure message=\"fail: 179/180 frames shown, missing 179\">"));
        assert!(x.contains("<skipped message=\"subtitle sidecar: not played on its own\"/>"));
        assert!(x.contains("name=\"x/&lt;a&amp;b&gt;.mp4\""));
        assert!(x.ends_with("</testsuite>\n"));
    }

    #[test]
    fn summary_lists_what_did_not_pass() {
        let md = github_summary(&sample(), "libvlc 3.0.24", "2026-10-29");
        assert!(md.starts_with("### testsrc: libvlc 3.0.24, snapshot 2026-10-29\n\n2 pass, 1 fail, 1 crash, 1 skipped, 1 with warnings\n"));
        assert!(md.contains("| `hls/ts.m3u8` | fail | frames: 179/180 frames shown, missing 179 |"));
        assert!(md.contains(r"ended with a signal: \|x\|")); // pipes escaped inside the table
        assert!(md.contains("<details><summary>2 passed</summary>"));
    }

    #[test]
    fn counts_in_order() {
        assert_eq!(counts(&sample()), vec![("pass", 2), ("fail", 1), ("crash", 1), ("hang", 0), ("skipped", 1), ("with warnings", 1)]);
    }

    #[test]
    fn utc_format() {
        let t = utc_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z') && &t[10..11] == "T");
    }
}
