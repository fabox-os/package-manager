//! Version comparison: try semver, fall back to loose numeric comparison.
//! `v2.0.0` == `2.0.0`. Equal versions are never reported as updates.

use std::cmp::Ordering;

use semver::Version;

fn strip(raw: &str) -> &str {
    let t = raw.trim();
    t.strip_prefix(['v', 'V']).unwrap_or(t)
}

fn parse_semver(raw: &str) -> Option<Version> {
    let s = strip(raw);
    if let Ok(v) = Version::parse(s) {
        return Some(v);
    }
    // Pad "1.2" / "1" to "1.2.0" / "1.0.0".
    let (core, rest) = match s.find(['-', '+']) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.parse::<u64>().is_err()) {
        return None;
    }
    let mut padded = parts;
    while padded.len() < 3 {
        padded.push("0");
    }
    Version::parse(&format!("{}{rest}", padded.join("."))).ok()
}

/// Splits into numeric / alphabetic runs: "1.2.10-beta" -> [N(1), N(2), N(10), S("beta")].
fn loose_tokens(raw: &str) -> Vec<Result<u64, String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut numeric = false;
    let flush = |cur: &mut String, numeric: bool, out: &mut Vec<Result<u64, String>>| {
        if cur.is_empty() {
            return;
        }
        let tok = std::mem::take(cur);
        out.push(if numeric {
            tok.parse::<u64>().map_err(|_| tok)
        } else {
            Err(tok.to_lowercase())
        });
    };
    for c in strip(raw).chars() {
        if c == '~' {
            // Debian: `~` sorts before everything, even the end of the string.
            flush(&mut cur, numeric, &mut out);
            out.push(Err("~".to_owned()));
            numeric = false;
        } else if c.is_ascii_digit() {
            if !numeric {
                flush(&mut cur, numeric, &mut out);
            }
            numeric = true;
            cur.push(c);
        } else if c.is_alphabetic() {
            if numeric {
                flush(&mut cur, numeric, &mut out);
            }
            numeric = false;
            cur.push(c);
        } else {
            flush(&mut cur, numeric, &mut out);
        }
    }
    flush(&mut cur, numeric, &mut out);
    out
}

/// How the end of a version compares with a text segment.
fn end_vs_text(text: &str) -> Ordering {
    if text == "~" {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

pub fn compare(a: &str, b: &str) -> Ordering {
    if let (Some(x), Some(y)) = (parse_semver(a), parse_semver(b)) {
        return x.cmp(&y);
    }
    let (ta, tb) = (loose_tokens(a), loose_tokens(b));
    for i in 0..ta.len().max(tb.len()) {
        let ord = match (ta.get(i), tb.get(i)) {
            (Some(Ok(p)), Some(Ok(q))) => p.cmp(q),
            (Some(Ok(_)), Some(Err(_))) => Ordering::Greater,
            (Some(Err(_)), Some(Ok(_))) => Ordering::Less,
            (Some(Err(p)), Some(Err(q))) => p.cmp(q),
            // One side ended: a number counts as 0 ("1.0" == "1.0.0"), `~` sorts before the
            // end, any other text sorts after it ("1.15" < "1.15ubuntu1").
            (Some(Ok(p)), None) => p.cmp(&0),
            (None, Some(Ok(q))) => 0.cmp(q),
            (Some(Err(t)), None) => end_vs_text(t).reverse(),
            (None, Some(Err(t))) => end_vs_text(t),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// True only if `latest` is strictly newer than `installed` after normalization.
pub fn is_newer(installed: &str, latest: &str) -> bool {
    compare(latest, installed) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v_prefix_is_ignored() {
        assert_eq!(compare("v2.0.0", "2.0.0"), Ordering::Equal);
        assert!(!is_newer("2.0.0", "v2.0.0"));
    }

    #[test]
    fn detects_newer_semver() {
        assert!(is_newer("2.0.0", "2.1.0"));
        assert!(is_newer("2.9.0", "2.10.0"));
        assert!(!is_newer("2.1.0", "2.0.0"));
    }

    #[test]
    fn short_versions_are_padded() {
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert!(is_newer("1.2", "1.2.1"));
    }

    #[test]
    fn prerelease_is_older_than_release() {
        assert!(is_newer("2.0.0-beta.1", "2.0.0"));
    }

    #[test]
    fn debian_style_versions_fall_back_to_loose() {
        assert!(is_newer("1.2.3-1ubuntu1", "1.2.3-1ubuntu2"));
        assert!(is_newer("1:1.0", "1:1.1"));
        assert!(!is_newer("1.2.3-1ubuntu1", "1.2.3-1ubuntu1"));
    }

    #[test]
    fn real_ubuntu_update_versions_are_newer() {
        // From a real `apt list --upgradable`.
        assert!(is_newer("1.15", "1.15ubuntu0.1"));
        assert!(is_newer("2.5.33-1ubuntu1.1", "2.5.33-1ubuntu1.2"));
        assert!(is_newer(
            "131.0+build1-0ubuntu0.24.04.1",
            "132.0+build1-0ubuntu0.24.04.1"
        ));
        assert!(!is_newer("1.15ubuntu0.1", "1.15"));
    }

    #[test]
    fn tilde_sorts_before_release() {
        assert!(is_newer("1.0~rc1", "1.0"));
        assert!(!is_newer("1.0", "1.0~rc1"));
    }

    #[test]
    fn garbage_never_panics() {
        for s in ["", "abc", "....", "v", "99999999999999999999999"] {
            let _ = compare(s, "1.0.0");
            let _ = compare("1.0.0", s);
        }
    }
}
