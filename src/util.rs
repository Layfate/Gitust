//! Small, dependency-free helpers: time formatting, path/URL handling, fuzzy matching.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// "just now", "5m ago", "3h ago", "2d ago", else "2026-01-05".
pub fn relative_time(epoch: i64, now: i64) -> String {
    let d = now - epoch;
    if d < 45 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", (d + 30) / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else if d < 86_400 * 30 {
        format!("{}d ago", d / 86_400)
    } else {
        date(epoch)
    }
}

/// UTC calendar date (proleptic Gregorian) from a Unix timestamp.
pub fn date(epoch: i64) -> String {
    let (y, m, d) = civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn datetime(epoch: i64, offset_minutes: i32) -> String {
    let local = epoch + offset_minutes as i64 * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let off = offset_minutes.abs();
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}", secs / 3600, (secs % 3600) / 60, off / 60, off % 60)
}

// Howard Hinnant's algorithms.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses GitHub's `2024-05-01T12:34:56Z` timestamps.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    Some(days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se)
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}

/// libgit2 hands back `C:/x/y/`; turn it into a native path without a trailing separator.
pub fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.trim_end_matches(['/', '\\']);
    #[cfg(windows)]
    let s = s.replace('/', "\\");
    PathBuf::from(if s.is_empty() { "/".to_string() } else { s.to_string() })
}

/// `owner/name` if `url` points at github.com (https, ssh or scp-style).
pub fn github_slug(url: &str) -> Option<String> {
    let u = url.trim();
    let path = if let Some(rest) = u.strip_prefix("git@github.com:") {
        rest
    } else {
        let (_, after) = u.split_once("://")?;
        let (authority, path) = after.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
            return None;
        }
        path
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// Expands `owner/name` shorthand to a GitHub HTTPS URL; leaves real URLs alone.
pub fn expand_clone_url(input: &str) -> String {
    let t = input.trim();
    let looks_like_slug = !t.contains("://")
        && !t.contains('@')
        && !t.contains('\\')
        && !t.starts_with('.')
        && !t.starts_with('/')
        && t.matches('/').count() == 1
        && !t.contains(':');
    if looks_like_slug { format!("https://github.com/{}.git", t.trim_end_matches(".git")) } else { t.to_string() }
}

/// Folder name git would choose for a clone URL.
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let t = url.trim().trim_end_matches(['/', '\\']);
    let last = t.rsplit(['/', ':', '\\']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    let ok = !name.is_empty() && name != "." && name != ".." && !name.contains(['<', '>', '"', '|', '?', '*']);
    ok.then(|| name.to_string())
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Subsequence fuzzy match with bonuses for word starts and consecutive runs.
/// Returns `None` when `query` is not a subsequence of `text`.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.chars().collect();
    let mut score = 0;
    let mut qi = 0;
    let mut prev_match = false;
    let mut first_match = None;
    for (i, ch) in t.iter().enumerate() {
        if qi == q.len() {
            break;
        }
        if ch.to_lowercase().next() == Some(q[qi]) {
            let word_start = i == 0 || matches!(t[i - 1], ' ' | '/' | '-' | '_' | '.' | ':');
            score += 1 + if prev_match { 5 } else { 0 } + if word_start { 8 } else { 0 };
            first_match.get_or_insert(i);
            qi += 1;
            prev_match = true;
        } else {
            prev_match = false;
        }
    }
    if qi < q.len() {
        return None;
    }
    Some(score * 4 - first_match.unwrap_or(0) as i32 - t.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(github_slug("https://github.com/rust-lang/rust.git").as_deref(), Some("rust-lang/rust"));
        assert_eq!(github_slug("git@github.com:o/r.git").as_deref(), Some("o/r"));
        assert_eq!(github_slug("ssh://git@github.com/o/r").as_deref(), Some("o/r"));
        assert_eq!(github_slug("https://user:tok@github.com/o/r/").as_deref(), Some("o/r"));
        assert_eq!(github_slug("https://gitlab.com/o/r.git"), None);
        assert_eq!(github_slug("https://github.com/o"), None);
    }

    #[test]
    fn clone_urls() {
        assert_eq!(expand_clone_url("rust-lang/rust"), "https://github.com/rust-lang/rust.git");
        assert_eq!(expand_clone_url("git@host:o/r.git"), "git@host:o/r.git");
        assert_eq!(repo_name_from_url("https://github.com/o/r.git").as_deref(), Some("r"));
        assert_eq!(repo_name_from_url("git@github.com:o/my-repo").as_deref(), Some("my-repo"));
        assert_eq!(repo_name_from_url("https://x/.."), None);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(parse_iso8601("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(date(1_709_208_000), "2024-02-29");
        assert_eq!(relative_time(100, 110), "just now");
        assert_eq!(relative_time(0, 7200), "2h ago");
        assert_eq!(datetime(0, 90), "1970-01-01 01:30 +0130");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("psh", "Push").is_some());
        assert!(fuzzy_score("xyz", "Push").is_none());
        assert!(fuzzy_score("fe", "Fetch").unwrap() > fuzzy_score("fe", "Safe edit").unwrap());
        assert_eq!(compact_count(1500), "1.5k");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
