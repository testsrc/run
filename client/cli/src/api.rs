//! The testsrc API: catalog (public), facts and files (token), and the local cache, keyed by snapshot.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{die, VERSION};

/// What the API (or the network) said when a request failed. A run that meets one mid-way stops fetching but keeps
/// the results it has (main.rs), so a quota or an evaluation token's case limit doesn't throw away the run.
#[derive(Debug)]
pub struct ApiError {
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// How many times a request is tried again after a 429 rate_limited, and after a network error.
const RATE_RETRIES: u32 = 3;
const NET_RETRIES: u32 = 2;

pub struct Api {
    base: String,
    token: Option<String>,
    snapshot: Option<String>,
    agent: ureq::Agent,
}

impl Api {
    pub fn new(base: &str, token: Option<String>, snapshot: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new().user_agent(&format!("testsrc-runner/{VERSION}")).build();
        Api { base: base.trim_end_matches('/').to_string(), token, snapshot, agent }
    }

    fn get(&self, path: &str, auth: bool) -> Result<Box<dyn Read + Send + Sync>, ApiError> {
        let mut url = format!("{}{path}", self.base);
        if let Some(s) = &self.snapshot {
            url += &format!("{}snapshot={s}", if url.contains('?') { '&' } else { '?' });
        }
        let mut req = self.agent.get(&url);
        // The token goes on every request when there is one: the catalog lists every case only with a token
        // (without, just the baseline files and the edge cases public on testsrc.com/cases)
        match &self.token {
            Some(t) => req = req.set("authorization", &format!("Bearer {t}")),
            None if auth => die("TESTSRC_TOKEN is not set. Create a token at https://testsrc.com/account"),
            None => {}
        }
        // Transient refusals are retried: the per-minute limit (429 rate_limited) after its Retry-After, a dropped
        // connection after a moment. The quota, an evaluation token's case limit and everything else end the run.
        let mut tries = 0;
        loop {
            tries += 1;
            match req.clone().call() {
                Ok(r) => return Ok(r.into_reader()),
                Err(ureq::Error::Status(code, r)) => {
                    let wait = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(60).min(120);
                    let body = r.into_string().unwrap_or_default();
                    let j = serde_json::from_str::<Value>(&body).ok();
                    let msg = j.as_ref().and_then(|j| j["error"].as_str()).unwrap_or_default();
                    let err = ApiError { message: format!("{code} from {path}: {msg}") };
                    if code != 429 || j.as_ref().and_then(|j| j["code"].as_str()) != Some("rate_limited") || tries > RATE_RETRIES {
                        return Err(err);
                    }
                    eprintln!("testsrc: rate limited on {path}, waiting {wait} s (retry {tries} of {RATE_RETRIES})");
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                }
                Err(e) => {
                    if tries > NET_RETRIES {
                        return Err(ApiError { message: format!("{path}: {e}") });
                    }
                    eprintln!("testsrc: {path}: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_secs(tries as u64));
                }
            }
        }
    }

    pub fn json(&self, path: &str, auth: bool) -> Result<Value, ApiError> {
        serde_json::from_reader(self.get(path, auth)?).map_err(|e| ApiError { message: format!("{path}: bad JSON: {e}") })
    }

    /// Download into `dest` through a `.part` file, returning the SHA-256 of what arrived. A body cut off while it
    /// is read is Retry (fetch() tries the whole download again); an answer get() refused, after its own retries, is
    /// Final.
    fn download(&self, path: &str, dest: &Path) -> Result<String, Download> {
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let mut r = self.get(path, true).map_err(Download::Final)?;
        let mut w = fs::File::create(&part).unwrap_or_else(|e| die(&format!("{}: {e}", part.display())));
        let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
        loop {
            let n = match r.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    drop(w);
                    let _ = fs::remove_file(&part);
                    return Err(Download::Retry(ApiError { message: format!("{path}: {e}") }));
                }
            };
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            w.write_all(&buf[..n]).unwrap_or_else(|e| die(&format!("{}: {e}", part.display())));
        }
        drop(w);
        let got = hex(&h.finalize());
        fs::rename(&part, dest).unwrap_or_else(|e| die(&format!("{}: {e}", dest.display())));
        Ok(got)
    }
}

enum Download {
    Retry(ApiError),
    Final(ApiError),
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_file(p: &Path) -> Option<String> {
    let mut f = fs::File::open(p).ok()?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(hex(&h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// A case id or a prefix (`hls/`, `audio-only/aac`); no pattern means every case.
pub fn select<'a>(cases: &'a [Value], patterns: &[String], exclude: &[String]) -> Vec<&'a Value> {
    cases.iter()
        .filter(|c| {
            let id = c["id"].as_str().unwrap_or("");
            (patterns.is_empty() || patterns.iter().any(|p| id == p || id.starts_with(p.as_str())))
                && !exclude.iter().any(|x| id.starts_with(x.as_str()))
        })
        .collect()
}

/// The case's facts, then its files unless the cached copies match. Returns the main file's path.
pub fn fetch(api: &Api, case: &Value, cache: &Path) -> Result<PathBuf, ApiError> {
    let id = case["id"].as_str().unwrap_or_default();
    let facts_path = cache.join(format!("{id}.facts.json"));
    if !facts_path.exists() {
        let facts = api.json(&format!("/cases/{id}/facts"), true)?;
        fs::create_dir_all(facts_path.parent().unwrap()).unwrap_or_else(|e| die(&e.to_string()));
        fs::write(&facts_path, facts.to_string()).unwrap_or_else(|e| die(&e.to_string()));
    }
    let facts: Value = serde_json::from_slice(&fs::read(&facts_path).unwrap()).unwrap_or_default();
    let dir = id.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    for name in case["files"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
        let dest = cache.join(&rel);
        let want = facts["sha256"][name].as_str().unwrap_or("");
        if dest.exists() && sha256_file(&dest).as_deref() == Some(want) {
            continue;
        }
        fs::create_dir_all(dest.parent().unwrap()).unwrap_or_else(|e| die(&e.to_string()));
        // A body cut off mid-download, or bytes that don't match the facts, are fetched again, as often as get()
        // retries a network error; what get() itself refused (the quota, a 404, ...) ends here.
        let mut tries = 0;
        loop {
            tries += 1;
            let problem = match api.download(&format!("/files/{rel}"), &dest) {
                Ok(got) if got == want => break,
                Ok(got) => {
                    let _ = fs::remove_file(&dest);
                    ApiError { message: format!("checksum mismatch for {rel}: got {got}") }
                }
                Err(Download::Retry(e)) => e,
                Err(Download::Final(e)) => return Err(e),
            };
            if tries > NET_RETRIES {
                return Err(problem);
            }
            eprintln!("testsrc: {problem}; downloading it again");
            std::thread::sleep(std::time::Duration::from_secs(tries as u64));
        }
    }
    Ok(cache.join(id))
}

/// The case's own files in the cache (not its facts), for --discard.
pub fn files_of(case: &Value, cache: &Path) -> Vec<PathBuf> {
    let id = case["id"].as_str().unwrap_or_default();
    let dir = id.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    case["files"].as_array().into_iter().flatten().filter_map(Value::as_str)
        .map(|name| if dir.is_empty() { cache.join(name) } else { cache.join(dir).join(name) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_by_id_prefix_and_exclude() {
        let cases: Vec<Value> = ["hls/ts.m3u8", "hls/fmp4.m3u8", "duration/30m.mp4", "codec/h264-aac.mp4"]
            .iter().map(|id| serde_json::json!({"id": id})).collect();
        let ids = |v: Vec<&Value>| v.iter().map(|c| c["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(select(&cases, &[], &[])).len(), 4);
        assert_eq!(ids(select(&cases, &["hls/".into()], &[])), ["hls/ts.m3u8", "hls/fmp4.m3u8"]);
        assert_eq!(ids(select(&cases, &["codec/h264-aac.mp4".into()], &[])), ["codec/h264-aac.mp4"]);
        assert_eq!(ids(select(&cases, &[], &["duration/".into(), "hls/f".into()])), ["hls/ts.m3u8", "codec/h264-aac.mp4"]);
    }

    #[test]
    fn sha256_of_a_file() {
        let p = std::env::temp_dir().join(format!("testsrc-sha-{}", std::process::id()));
        fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        fs::remove_file(&p).unwrap();
        assert_eq!(sha256_file(Path::new("/no/such/file")), None);
    }
}
