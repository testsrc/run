//! `testsrc.lock`'s list of expected failures: cases that still run and are reported, but whose failure doesn't
//! fail the run. The file is TOML; the runner reads only this key (the action reads `snapshot`):
//!
//!   snapshot = "2026-11-06"
//!   expected = [
//!     "edge/rotation-90-display-matrix.mp4",   # our player ignores the display matrix (#412)
//!     "hls/",                                  # a prefix, as for --exclude
//!   ]
//!
//! It lives in the repository, next to the code it describes, so the same commit always runs the same way.

/// The `expected` list: case ids or prefixes. A file without the key has none; a malformed list is an error that
/// says which line.
pub fn expected(text: &str) -> Result<Vec<String>, String> {
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, strip_comment(l)));
    let mut out = vec![];
    while let Some((n, line)) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("expected") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('=') else { continue };
        let mut rest = rest.trim().strip_prefix('[')
            .ok_or_else(|| format!("line {n}: expected must be a list, like expected = [\"hls/\", \"codec/av1-opus.webm\"]"))?
            .to_string();
        let mut at = n;
        loop {
            if let Some(end) = rest.find(']') {
                strings(&rest[..end], at, &mut out)?;
                return Ok(out);
            }
            strings(&rest, at, &mut out)?;
            let Some((n, next)) = lines.next() else {
                return Err(format!("line {at}: the expected list has no closing ]"));
            };
            at = n;
            rest = next;
        }
    }
    Ok(out)
}

/// The line without its comment (a # outside quotes)
fn strip_comment(line: &str) -> String {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (c, Some(q)) if c == q => quote = None,
            ('#', None) => return line[..i].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

/// The quoted strings of one line of the list, separated by commas
fn strings(part: &str, line: usize, out: &mut Vec<String>) -> Result<(), String> {
    for item in part.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let inner = item.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
            .or_else(|| item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .ok_or_else(|| format!("line {line}: {item} in the expected list isn't quoted, like \"hls/\""))?;
        if inner.is_empty() {
            return Err(format!("line {line}: an empty case id in the expected list"));
        }
        out.push(inner.to_string());
    }
    Ok(())
}

/// Whether a case id is on the list (an id, or a prefix of it)
pub fn matches(list: &[String], id: &str) -> bool {
    list.iter().any(|e| id.starts_with(e.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_is_an_empty_list() {
        assert_eq!(expected("# written by testsrc/run\nsnapshot = \"2026-11-06\"\n").unwrap(), Vec::<String>::new());
        assert_eq!(expected("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_list_over_several_lines_with_comments() {
        let t = "snapshot = \"2026-11-06\"\n\n# don't fail the job on these\nexpected = [\n  \"edge/rotation-90-display-matrix.mp4\",  # ignores the matrix (#412)\n  'hls/', \"a#b.mp4\"\n\n]\nother = 1\n";
        assert_eq!(expected(t).unwrap(), ["edge/rotation-90-display-matrix.mp4", "hls/", "a#b.mp4"]);
    }

    #[test]
    fn a_list_on_one_line() {
        assert_eq!(expected("expected = [\"hls/\", \"codec/x.mp4\"]  # two").unwrap(), ["hls/", "codec/x.mp4"]);
        assert_eq!(expected("expected=[]").unwrap(), Vec::<String>::new());
        assert_eq!(expected("expected = [\n\"hls/\"]").unwrap(), ["hls/"]);
    }

    #[test]
    fn other_keys_starting_with_expected_are_not_it() {
        assert_eq!(expected("expected_by = \"x\"\n# expected = [\"a\"]\n").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn malformed_lists_say_which_line() {
        assert!(expected("\nexpected = \"hls/\"").unwrap_err().starts_with("line 2: expected must be a list"));
        assert!(expected("expected = [\n  \"hls/\",\n").unwrap_err().contains("no closing ]"));
        assert!(expected("expected = [\n  hls/\n]").unwrap_err().starts_with("line 2: hls/ in the expected list isn't quoted"));
        assert!(expected("expected = [\"\"]").unwrap_err().contains("empty case id"));
    }

    #[test]
    fn ids_and_prefixes_match() {
        let l = vec!["hls/".to_string(), "codec/a.mp4".to_string()];
        assert!(matches(&l, "hls/ts.m3u8") && matches(&l, "codec/a.mp4"));
        assert!(!matches(&l, "codec/b.mp4") && !matches(&[], "hls/ts.m3u8"));
    }
}
