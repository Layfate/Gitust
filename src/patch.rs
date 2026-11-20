//! Builds the patch for partial (hunk / line) staging. Pure: no I/O, no git.
//!
//! The input is one file's diff exactly as the UI showed it (index→worktree when
//! staging, HEAD→index when unstaging) plus the set of selected `+`/`-` lines.
//! The output is a unified diff for `git apply --cached` (staging) or
//! `git apply --cached --reverse` (unstaging).
//!
//! How unselected lines are rewritten:
//!
//! | line | staging (applied forward)   | unstaging (applied in reverse) |
//! |------|-----------------------------|--------------------------------|
//! | `-`  | becomes context (stays put) | dropped (stays absent)         |
//! | `+`  | dropped (not staged)        | becomes context (stays staged) |
//!
//! Hunk headers are recounted from what is actually emitted. A wrong count
//! makes `git apply` either reject the patch or misplace lines, so this module
//! is covered by randomized tests that apply the result with real git.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PLine {
    /// `b' '`, `b'+'` or `b'-'`.
    pub origin: u8,
    /// Raw bytes as stored in Git (after clean filters), including the trailing
    /// `\n` when the line has one.
    pub content: Vec<u8>,
}

impl PLine {
    fn has_newline(&self) -> bool {
        self.content.last() == Some(&b'\n')
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<PLine>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileDiff {
    pub hunks: Vec<PHunk>,
    /// The file is absent on the old side (untracked when staging, newly added when unstaging).
    pub old_absent: bool,
    /// The file is absent on the new side (deleted).
    pub new_absent: bool,
    /// Git file mode of the side that exists (e.g. `0o100644`).
    pub mode: u32,
}

impl FileDiff {
    /// Identity of the diff the user made a selection on. Any change to the
    /// file in between makes the selection meaningless, so we refuse to apply.
    pub fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }

    pub fn changed_lines(&self) -> usize {
        self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.origin != b' ').count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Stage,
    Unstage,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchError {
    /// A selected position doesn't exist or isn't a `+`/`-` line.
    BadSelection,
    /// The selection would put content after a line that has no trailing
    /// newline (only possible at the very end of a file).
    MissingNewline,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PatchError::BadSelection => "The selected lines no longer match the diff. Select them again.",
            PatchError::MissingNewline => {
                "The last line of this file has no line break, so it has to be staged together with the lines around it. Select the whole change at the end of the file."
            }
        })
    }
}

impl std::error::Error for PatchError {}

/// Builds the patch, or `Ok(None)` if the selection contains no changed line.
/// `selected` holds `(hunk index, line index within hunk)` pairs.
pub fn build(
    path: &str,
    diff: &FileDiff,
    selected: &HashSet<(usize, usize)>,
    dir: Direction,
) -> Result<Option<Vec<u8>>, PatchError> {
    for &(h, l) in selected {
        let ok = diff.hunks.get(h).and_then(|hunk| hunk.lines.get(l)).is_some_and(|line| line.origin != b' ');
        if !ok {
            return Err(PatchError::BadSelection);
        }
    }

    let mut body = Vec::new();
    let mut delta: i64 = 0; // emitted new_count - old_count so far
    let mut any = false;

    for (hi, hunk) in diff.hunks.iter().enumerate() {
        let mut out: Vec<(u8, &PLine)> = Vec::with_capacity(hunk.lines.len());
        let mut changes = 0;
        for (li, line) in hunk.lines.iter().enumerate() {
            let sel = selected.contains(&(hi, li));
            let origin = match (line.origin, sel, dir) {
                (b' ', _, _) => Some(b' '),
                (o, true, _) => Some(o),
                (b'-', false, Direction::Stage) => Some(b' '),
                (b'+', false, Direction::Stage) => None,
                (b'+', false, Direction::Unstage) => Some(b' '),
                (b'-', false, Direction::Unstage) => None,
                _ => return Err(PatchError::BadSelection),
            };
            if let Some(o) = origin {
                if o != b' ' {
                    changes += 1;
                }
                out.push((o, line));
            }
        }
        if changes == 0 {
            continue;
        }
        check_newlines(&out)?;

        let old_count = out.iter().filter(|(o, _)| *o != b'+').count() as i64;
        let new_count = out.iter().filter(|(o, _)| *o != b'-').count() as i64;
        // Unified diff convention: with a count of 0 the start is the line *before*.
        let first = |start: u32, lines: u32| if lines == 0 { start as i64 + 1 } else { start as i64 };
        let (old_first, new_first) = match dir {
            // Applied to the old side (the index) — anchor on its real positions.
            Direction::Stage => {
                let o = first(hunk.old_start, hunk.old_lines);
                (o, o + delta)
            }
            // Applied in reverse to the new side (the index) — anchor there.
            Direction::Unstage => {
                let n = first(hunk.new_start, hunk.new_lines);
                (n - delta, n)
            }
        };
        let shown = |first: i64, count: i64| if count == 0 { first - 1 } else { first };
        let header = format!(
            "@@ -{},{} +{},{} @@\n",
            shown(old_first, old_count).max(0),
            old_count,
            shown(new_first, new_count).max(0),
            new_count
        );
        body.extend_from_slice(header.as_bytes());
        for (o, line) in out {
            body.push(o);
            body.extend_from_slice(&line.content);
            if !line.has_newline() {
                body.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
        delta += new_count - old_count;
        any = true;
    }

    if !any {
        return Ok(None);
    }

    let a = quote(&format!("a/{path}"));
    let b = quote(&format!("b/{path}"));
    let mut patch = format!("diff --git {a} {b}\n");
    let mode = format!("{:06o}", diff.mode);
    // A partial patch never creates or deletes the file on the side it is
    // applied to, except: staging part of an untracked file creates it in the
    // index, and unstaging part of a staged deletion recreates it there.
    match dir {
        Direction::Stage if diff.old_absent => {
            let _ = write!(patch, "new file mode {mode}\n--- /dev/null\n+++ {b}\n");
        }
        Direction::Unstage if diff.new_absent => {
            let _ = write!(patch, "deleted file mode {mode}\n--- {a}\n+++ /dev/null\n");
        }
        _ => {
            let _ = write!(patch, "--- {a}\n+++ {b}\n");
        }
    }
    let mut bytes = patch.into_bytes();
    bytes.extend_from_slice(&body);
    Ok(Some(bytes))
}

/// A line without a trailing newline must be the last line on its side.
fn check_newlines(out: &[(u8, &PLine)]) -> Result<(), PatchError> {
    for side_excludes in *b"+-" {
        let side: Vec<&PLine> = out.iter().filter(|(o, _)| *o != side_excludes).map(|(_, l)| *l).collect();
        if let Some(pos) = side.iter().position(|l| !l.has_newline())
            && pos + 1 != side.len()
        {
            return Err(PatchError::MissingNewline);
        }
    }
    Ok(())
}

/// Git's C-style quoting, applied unconditionally so names with spaces, tabs,
/// quotes, backslashes, newlines or non-ASCII bytes round-trip exactly.
pub fn quote(name: &str) -> String {
    let mut q = String::with_capacity(name.len() + 2);
    q.push('"');
    for &b in name.as_bytes() {
        match b {
            b'"' => q.push_str("\\\""),
            b'\\' => q.push_str("\\\\"),
            b'\t' => q.push_str("\\t"),
            b'\n' => q.push_str("\\n"),
            0x20..=0x7e => q.push(b as char),
            _ => {
                let _ = write!(q, "\\{b:03o}");
            }
        }
    }
    q.push('"');
    q
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn line(origin: u8, s: &str) -> PLine {
        PLine { origin, content: s.as_bytes().to_vec() }
    }

    fn hunk(os: u32, ol: u32, ns: u32, nl: u32, lines: Vec<PLine>) -> PHunk {
        PHunk { old_start: os, old_lines: ol, new_start: ns, new_lines: nl, lines }
    }

    fn sel(v: &[(usize, usize)]) -> HashSet<(usize, usize)> {
        v.iter().copied().collect()
    }

    fn text(p: Option<Vec<u8>>) -> String {
        String::from_utf8(p.unwrap()).unwrap()
    }

    fn modified(hunks: Vec<PHunk>) -> FileDiff {
        FileDiff { hunks, mode: 0o100644, ..Default::default() }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a/plain.txt"), "\"a/plain.txt\"");
        assert_eq!(quote("a/sp ace\"q\\"), "\"a/sp ace\\\"q\\\\\"");
        assert_eq!(quote("a/é"), "\"a/\\303\\251\"");
        assert_eq!(quote("a/t\tn\n"), "\"a/t\\tn\\n\"");
    }

    #[test]
    fn stage_one_of_two_additions() {
        // a b c  →  a X b Y c
        let d = modified(vec![hunk(
            1,
            3,
            1,
            5,
            vec![line(b' ', "a\n"), line(b'+', "X\n"), line(b' ', "b\n"), line(b'+', "Y\n"), line(b' ', "c\n")],
        )]);
        let p = text(build("f", &d, &sel(&[(0, 3)]), Direction::Stage).unwrap());
        assert_eq!(p, "diff --git \"a/f\" \"b/f\"\n--- \"a/f\"\n+++ \"b/f\"\n@@ -1,3 +1,4 @@\n a\n b\n+Y\n c\n");
    }

    #[test]
    fn stage_turns_unselected_deletions_into_context() {
        let d = modified(vec![hunk(4, 2, 4, 0, vec![line(b'-', "x\n"), line(b'-', "y\n")])]);
        let p = text(build("f", &d, &sel(&[(0, 1)]), Direction::Stage).unwrap());
        assert!(p.ends_with("@@ -4,2 +4,1 @@\n x\n-y\n"), "{p}");
    }

    #[test]
    fn later_hunks_shift_by_earlier_deltas() {
        let d = modified(vec![
            hunk(1, 1, 1, 3, vec![line(b' ', "a\n"), line(b'+', "1\n"), line(b'+', "2\n")]),
            hunk(10, 1, 12, 2, vec![line(b' ', "z\n"), line(b'+', "3\n")]),
        ]);
        // Only one of the two lines in hunk 0, so hunk 1 moves by +1, not +2.
        let p = text(build("f", &d, &sel(&[(0, 1), (1, 1)]), Direction::Stage).unwrap());
        assert!(p.contains("@@ -1,1 +1,2 @@\n a\n+1\n@@ -10,1 +11,2 @@\n z\n+3\n"), "{p}");
    }

    #[test]
    fn unstage_keeps_unselected_additions_as_context() {
        // HEAD: a   index: a X Y
        let d = modified(vec![hunk(1, 1, 1, 3, vec![line(b' ', "a\n"), line(b'+', "X\n"), line(b'+', "Y\n")])]);
        let p = text(build("f", &d, &sel(&[(0, 2)]), Direction::Unstage).unwrap());
        assert!(p.ends_with("@@ -1,2 +1,3 @@\n a\n X\n+Y\n"), "{p}");
    }

    #[test]
    fn untracked_and_deleted_headers() {
        let new = FileDiff {
            hunks: vec![hunk(0, 0, 1, 2, vec![line(b'+', "a\n"), line(b'+', "b\n")])],
            old_absent: true,
            mode: 0o100755,
            ..Default::default()
        };
        let p = text(build("x y", &new, &sel(&[(0, 1)]), Direction::Stage).unwrap());
        assert_eq!(
            p,
            "diff --git \"a/x y\" \"b/x y\"\nnew file mode 100755\n--- /dev/null\n+++ \"b/x y\"\n@@ -0,0 +1,1 @@\n+b\n"
        );

        let gone = FileDiff {
            hunks: vec![hunk(1, 2, 0, 0, vec![line(b'-', "a\n"), line(b'-', "b\n")])],
            new_absent: true,
            mode: 0o100644,
            ..Default::default()
        };
        let p = text(build("g", &gone, &sel(&[(0, 0)]), Direction::Unstage).unwrap());
        assert!(p.contains("deleted file mode 100644\n--- \"a/g\"\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-a\n"), "{p}");
    }

    #[test]
    fn no_newline_markers() {
        // "a" (no newline) → "a\nb" (no newline)
        let d = modified(vec![hunk(1, 1, 1, 2, vec![line(b'-', "a"), line(b'+', "a\n"), line(b'+', "b")])]);
        let all = text(build("f", &d, &sel(&[(0, 0), (0, 1), (0, 2)]), Direction::Stage).unwrap());
        assert!(all.ends_with("-a\n\\ No newline at end of file\n+a\n+b\n\\ No newline at end of file\n"), "{all}");
        // Staging only "b" would glue it onto "a".
        assert_eq!(build("f", &d, &sel(&[(0, 2)]), Direction::Stage), Err(PatchError::MissingNewline));
        // Fixing the newline on its own is fine.
        assert!(build("f", &d, &sel(&[(0, 0), (0, 1)]), Direction::Stage).unwrap().is_some());
    }

    #[test]
    fn rejects_bad_selection_and_empty() {
        let d = modified(vec![hunk(1, 1, 1, 2, vec![line(b' ', "a\n"), line(b'+', "b\n")])]);
        assert_eq!(build("f", &d, &sel(&[(0, 0)]), Direction::Stage), Err(PatchError::BadSelection));
        assert_eq!(build("f", &d, &sel(&[(3, 0)]), Direction::Stage), Err(PatchError::BadSelection));
        assert_eq!(build("f", &d, &sel(&[]), Direction::Stage), Ok(None));
    }

    #[test]
    fn content_that_looks_like_patch_syntax() {
        let d = modified(vec![hunk(
            1,
            1,
            1,
            4,
            vec![line(b' ', "--- a\n"), line(b'+', "+++ b\n"), line(b'+', "@@ -1 +1 @@\n"), line(b'+', "\\ x\n")],
        )]);
        let p = text(build("f", &d, &sel(&[(0, 1), (0, 3)]), Direction::Stage).unwrap());
        assert!(p.ends_with("@@ -1,1 +1,3 @@\n --- a\n++++ b\n+\\ x\n"), "{p}");
    }
}
