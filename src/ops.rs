//! Repository mutations through the Git CLI, kept free of UI so they can be
//! tested against real repositories. Paths always travel over stdin as
//! NUL-separated literal pathspecs, so names that start with `-`, contain
//! spaces, glob characters or `:(magic)` are only ever names.

use crate::cli::{self, Opts};
use crate::git::{self, FileChange};
use crate::patch::{self, Direction};
use anyhow::{Result, anyhow, bail};
use std::collections::HashSet;
use std::path::Path;

/// A git failure with a human summary for the status bar and raw output for details.
#[derive(Debug)]
pub struct GitFailure {
    pub summary: String,
    pub detail: String,
}
impl std::fmt::Display for GitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}
impl std::error::Error for GitFailure {}

pub async fn git_ok(root: Option<&Path>, args: &[&str], opts: Opts) -> Result<cli::Output> {
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = cli::git(root, &argv, opts).await?;
    if out.ok {
        Ok(out)
    } else {
        let raw = format!("$ git {}\n\n{}{}", args.join(" "), out.stdout, out.stderr);
        Err(GitFailure { summary: cli::explain(&format!("{}\n{}", out.stdout, out.stderr)), detail: raw }.into())
    }
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("Background task failed: {e}"))?
}

fn with_paths(mut opts: Opts, paths: &[impl AsRef<str>]) -> Opts {
    opts.stdin = Some(cli::nul_paths(paths));
    opts.literal_pathspecs = true;
    opts
}

const FROM_STDIN: [&str; 2] = ["--pathspec-from-file=-", "--pathspec-file-nul"];

pub async fn stage_paths(root: &Path, paths: &[String], opts: Opts) -> Result<()> {
    let args = [&["add", "-A"][..], &FROM_STDIN].concat();
    git_ok(Some(root), &args, with_paths(opts, paths)).await.map(drop)
}

/// `unborn`: no commit yet, so there is no HEAD to reset to.
pub async fn unstage_paths(root: &Path, paths: &[String], unborn: bool, opts: Opts) -> Result<()> {
    // `-f` with `--cached` only drops the index entry (the file on disk is untouched),
    // matching what `reset` does once there is a HEAD.
    let base: &[&str] = if unborn { &["rm", "--cached", "-f", "-r", "-q"] } else { &["reset", "-q"] };
    let args = [base, &FROM_STDIN].concat();
    git_ok(Some(root), &args, with_paths(opts, paths)).await.map(drop)
}

pub async fn stage_all(root: &Path, opts: Opts) -> Result<()> {
    git_ok(Some(root), &["add", "-A"], opts).await.map(drop)
}

pub async fn unstage_all(root: &Path, unborn: bool, opts: Opts) -> Result<()> {
    let args: &[&str] = if unborn { &["rm", "--cached", "-f", "-r", "-q", "--", "."] } else { &["reset", "-q"] };
    git_ok(Some(root), args, opts).await.map(drop)
}

/// Throws away unstaged edits to one file. Staged content is what it returns to.
pub async fn discard(root: &Path, path: &str, opts: Opts) -> Result<()> {
    let args = [&["restore", "--worktree"][..], &FROM_STDIN].concat();
    git_ok(Some(root), &args, with_paths(opts, &[path])).await.map(drop)
}

/// Applies a patch to the index only. The `-c` overrides stop a user's
/// `apply.whitespace=fix` from silently rewriting the staged lines and
/// `apply.ignoreWhitespace` from matching the wrong context.
pub async fn apply_cached(root: &Path, patch: Vec<u8>, reverse: bool, mut opts: Opts) -> Result<()> {
    opts.stdin = Some(patch);
    let mut args = vec!["-c", "apply.whitespace=nowarn", "-c", "apply.ignoreWhitespace=no", "apply", "--cached"];
    if reverse {
        args.push("--reverse");
    }
    args.push("-");
    git_ok(Some(root), &args, opts).await.map(drop).map_err(|e| match e.downcast::<GitFailure>() {
        Ok(g) => GitFailure {
            summary: "Git couldn't apply the selected lines — the file may have changed. Refresh and try again.".into(),
            detail: g.detail,
        }
        .into(),
        Err(e) => e,
    })
}

/// Stages (or, for a staged `change`, unstages) the diff rows the user
/// selected. `fingerprint` identifies the diff those rows were taken from; if
/// the file changed since, nothing is applied. Returns the number of lines.
pub async fn stage_lines(
    root: &Path,
    change: FileChange,
    fingerprint: u64,
    rows: Vec<usize>,
    unborn: bool,
    opts: Opts,
) -> Result<usize> {
    let (r, c) = (root.to_path_buf(), change.clone());
    let (view, source) = blocking(move || git::file_diff_with_source(&git::open(&r)?, &c, true)).await?;
    let Some(source) = source else {
        bail!("Line staging isn't available for {}. Stage or unstage the whole file instead.", change.path)
    };
    if source.fingerprint() != fingerprint {
        bail!("{} changed since you selected lines. Check the diff and select them again.", change.path);
    }
    let mut selected = HashSet::new();
    for row in rows {
        let pos = view.lines.get(row).and_then(|l| l.pos).ok_or(patch::PatchError::BadSelection)?;
        selected.insert((pos.0 as usize, pos.1 as usize));
    }
    let n = selected.len();
    if n == 0 {
        return Ok(0);
    }
    if n == source.changed_lines() {
        // Everything selected: the path-level command also carries the file's
        // creation, deletion or mode change, which a content patch can't.
        let paths = [change.path.clone()];
        if change.staged {
            unstage_paths(root, &paths, unborn, opts).await?;
        } else {
            stage_paths(root, &paths, opts).await?;
        }
        return Ok(n);
    }
    let dir = if change.staged { Direction::Unstage } else { Direction::Stage };
    let Some(bytes) = patch::build(&change.path, &source, &selected, dir)? else { return Ok(0) };
    apply_cached(root, bytes, change.staged, opts).await?;
    Ok(n)
}

/// Resolves a conflicted file by taking one side whole. `mine` is the user's
/// side: HEAD during a merge, cherry-pick or stash pop, but the commit being
/// replayed during a rebase (where Git's "ours" is the branch rebased onto).
pub async fn take_side(root: &Path, path: &str, mine: bool, rebasing: bool, opts: Opts) -> Result<()> {
    let ours = mine != rebasing;
    let (r, p) = (root.to_path_buf(), path.to_string());
    let (has_ours, has_theirs) = blocking(move || git::conflict_sides(&git::open(&r)?, &p)).await?;
    if (ours && has_ours) || (!ours && has_theirs) {
        let side = if ours { "--ours" } else { "--theirs" };
        let args = [&["checkout", side][..], &FROM_STDIN].concat();
        git_ok(Some(root), &args, with_paths(opts.clone(), &[path])).await?;
        stage_paths(root, &[path.to_string()], opts).await
    } else {
        // That side deleted the file.
        let args = [&["rm", "-q"][..], &FROM_STDIN].concat();
        git_ok(Some(root), &args, with_paths(opts, &[path])).await.map(drop)
    }
}

/// Commits the index with `message` (passed verbatim over stdin). Returns the short SHA.
pub async fn commit(root: &Path, message: String, amend: bool, mut opts: Opts) -> Result<String> {
    opts.stdin = Some(message.into_bytes());
    // `--cleanup=whitespace`: only trims blank lines, so a summary like "#42 fix" survives.
    let mut args = vec!["commit", "--cleanup=whitespace", "-F", "-"];
    if amend {
        args.push("--amend");
    }
    let out = git_ok(Some(root), &args, opts).await?;
    // "[main 1a2b3c4] Summary" → "1a2b3c4"
    let first = out.stdout.lines().next().unwrap_or("");
    Ok(first.split(']').next().and_then(|x| x.split_whitespace().last()).unwrap_or("").to_string())
}

/// Switches branch. `track`: create a local branch tracking this remote branch.
/// Uncommitted changes come along; Git refuses (and changes nothing) if they'd be overwritten.
pub async fn switch(root: &Path, local: &str, track: Option<&str>, opts: Opts) -> Result<()> {
    if !git::valid_branch_name(local) {
        bail!("'{local}' isn't a valid branch name");
    }
    let args: Vec<&str> = match track {
        Some(remote) => vec!["switch", "--track", remote],
        None => vec!["switch", local],
    };
    git_ok(Some(root), &args, opts).await.map(drop)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    /// Whatever the user's Git config says (fast-forward when possible).
    Default,
    Merge,
    Rebase,
}

pub async fn pull(root: &Path, mode: PullMode, opts: Opts) -> Result<cli::Output> {
    let mut args = vec!["pull", "--progress"];
    match mode {
        PullMode::Default => {}
        // `--ff` overrides a configured `pull.ff=only`: fast-forward if possible, else merge.
        PullMode::Merge => args.extend(["--no-rebase", "--ff", "--no-edit"]),
        PullMode::Rebase => args.push("--rebase"),
    }
    git_ok(Some(root), &args, opts).await
}

/// Whether a failed pull stopped because the branches diverged and Git wants a strategy.
pub fn is_divergence(e: &anyhow::Error) -> bool {
    e.downcast_ref::<GitFailure>().is_some_and(|g| {
        let d = g.detail.to_lowercase();
        d.contains("divergent branches") || d.contains("not possible to fast-forward")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{FileDiff, PLine};
    use std::fs;
    use std::path::PathBuf;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repository whose behaviour doesn't depend on the machine's global config.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        run(&root, &["init", "-q", "-b", "main"]);
        for (k, v) in [
            ("user.name", "Test"),
            ("user.email", "t@example.com"),
            ("core.autocrlf", "false"),
            ("core.safecrlf", "false"),
            ("commit.gpgsign", "false"),
            // Hostile-ish defaults our commands must override.
            ("apply.whitespace", "fix"),
            ("apply.ignoreWhitespace", "change"),
        ] {
            run(&root, &["config", k, v]);
        }
        (dir, root)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn index_blob(root: &Path, path: &str) -> Option<Vec<u8>> {
        let repo = git2::Repository::open(root).unwrap();
        let index = repo.index().unwrap();
        let entry = index.get_path(Path::new(path), 0)?;
        Some(repo.find_blob(entry.id).unwrap().content().to_vec())
    }

    /// Sets the index entry for `path` to exactly `bytes` (no filters).
    fn set_index(root: &Path, path: &str, bytes: &[u8]) {
        let repo = git2::Repository::open(root).unwrap();
        let mut index = repo.index().unwrap();
        let id = repo.blob(bytes).unwrap();
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: bytes.len() as u32,
            id,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        };
        index.add(&entry).unwrap();
        index.write().unwrap();
    }

    fn commit_index(root: &Path, msg: &str) {
        let repo = git2::Repository::open(root).unwrap();
        let mut index = repo.index().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "t@example.com").unwrap();
        let parents: Vec<git2::Commit> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &refs).unwrap();
    }

    fn change(root: &Path, path: &str, staged: bool) -> FileChange {
        let s = git::summary(&git::open(root).unwrap()).unwrap();
        let list = if staged { s.staged } else { s.unstaged };
        list.into_iter().find(|c| c.path == path).unwrap_or_else(|| panic!("{path} not in status"))
    }

    fn rows_of(view: &git::DiffView, pred: impl Fn(&str) -> bool) -> Vec<usize> {
        view.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| (l.kind == git::kind::ADD || l.kind == git::kind::DEL) && pred(&l.text))
            .map(|(i, _)| i)
            .collect()
    }

    fn view(root: &Path, c: &FileChange) -> (git::DiffView, FileDiff) {
        let (v, s) = git::file_diff_with_source(&git::open(root).unwrap(), c, true).unwrap();
        (v, s.expect("line staging available"))
    }

    // ───────────── randomized: patch + real `git apply` vs an oracle ─────────────

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.next() % 100 < pct
        }
    }

    const WORDS: &[&str] = &[
        "a",
        "b",
        "c",
        "d",
        "e",
        "",
        "-x",
        "+y",
        "@@ -1 +1 @@",
        "\\ No newline",
        "--- a/f",
        "+++ b/f",
        " lead",
        "trail ",
        "\ttab",
        "ünï ✓",
        "a\r",
        "diff --git a/x b/x",
    ];

    fn gen_lines(rng: &mut Rng, max: usize) -> Vec<String> {
        (0..rng.below(max + 1)).map(|_| WORDS[rng.below(WORDS.len())].to_string()).collect()
    }

    fn mutate(rng: &mut Rng, old: &[String]) -> Vec<String> {
        let mut new = Vec::new();
        for l in old {
            match rng.below(10) {
                0 | 1 => {}                                          // delete
                2 => new.push(WORDS[rng.below(WORDS.len())].into()), // replace
                3 => {
                    new.push(WORDS[rng.below(WORDS.len())].into()); // insert before
                    new.push(l.clone());
                }
                _ => new.push(l.clone()),
            }
        }
        if rng.chance(40) {
            new.extend(gen_lines(rng, 3));
        }
        new
    }

    fn join(lines: &[String], final_newline: bool) -> Vec<u8> {
        let mut s = lines.join("\n");
        if final_newline && !lines.is_empty() {
            s.push('\n');
        }
        s.into_bytes()
    }

    fn split_keep(b: &[u8]) -> Vec<Vec<u8>> {
        b.split_inclusive(|c| *c == b'\n').map(<[u8]>::to_vec).collect()
    }

    /// What the index should hold after applying `sel`, computed straight from
    /// the hunks without any patch formatting. `None`: the selection would glue
    /// a line onto one that has no trailing newline.
    fn oracle(base: &[u8], fd: &FileDiff, sel: &HashSet<(usize, usize)>, dir: Direction) -> Option<Vec<u8>> {
        let base_lines = split_keep(base);
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut cursor = 0;
        for (hi, h) in fd.hunks.iter().enumerate() {
            let (start, count) = match dir {
                Direction::Stage => (h.old_start, h.old_lines),
                Direction::Unstage => (h.new_start, h.new_lines),
            };
            let first = if count == 0 { start as usize } else { start as usize - 1 };
            out.extend(base_lines[cursor..first].iter().cloned());
            // The hunk must describe the base it claims to.
            let side: Vec<&PLine> = h
                .lines
                .iter()
                .filter(|l| l.origin == b' ' || l.origin == if dir == Direction::Stage { b'-' } else { b'+' })
                .collect();
            assert_eq!(side.len(), count as usize);
            for (k, l) in side.iter().enumerate() {
                assert_eq!(l.content, base_lines[first + k]);
            }
            for (li, l) in h.lines.iter().enumerate() {
                let s = sel.contains(&(hi, li));
                let keep = match (l.origin, dir) {
                    (b' ', _) => true,
                    (b'-', Direction::Stage) | (b'+', Direction::Unstage) => !s,
                    (b'+', Direction::Stage) | (b'-', Direction::Unstage) => s,
                    _ => unreachable!(),
                };
                if keep {
                    out.push(l.content.clone());
                }
            }
            cursor = first + count as usize;
        }
        out.extend(base_lines[cursor..].iter().cloned());
        let glued = out.iter().rev().skip(1).any(|l| l.last() != Some(&b'\n'));
        (!glued).then(|| out.concat())
    }

    /// Returns what the round exercised, so the caller can check coverage.
    fn random_round(seed: u64, dir: Direction) -> &'static str {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (_d, root) = repo();
        let old_lines = if rng.chance(12) { Vec::new() } else { gen_lines(&mut rng, 14) };
        let new_lines = mutate(&mut rng, &old_lines);
        let old = join(&old_lines, rng.chance(80));
        let new = join(&new_lines, rng.chance(80));
        if old == new {
            return "same";
        }
        let path = "dir/f ü.txt";
        fs::create_dir_all(root.join("dir")).unwrap();
        // Brand-new files (empty old side) exercise the new-file patch paths.
        let fresh = old.is_empty() && rng.chance(50);
        let base = match dir {
            Direction::Stage => {
                if !fresh {
                    set_index(&root, path, &old);
                }
                fs::write(root.join(path), &new).unwrap();
                old.clone()
            }
            Direction::Unstage => {
                if !fresh {
                    set_index(&root, path, &old);
                    commit_index(&root, "base");
                }
                set_index(&root, path, &new);
                // The worktree differs from both, to prove only the index changes.
                fs::write(root.join(path), b"worktree only\n").unwrap();
                new.clone()
            }
        };
        let staged = dir == Direction::Unstage;
        let c = change(&root, path, staged);
        let (v, fd) = view(&root, &c);
        let rows = rows_of(&v, |_| true);
        let picked: Vec<usize> = rows.iter().copied().filter(|_| rng.chance(50)).collect();
        let sel: HashSet<(usize, usize)> =
            picked.iter().map(|&r| v.lines[r].pos.map(|(h, l)| (h as usize, l as usize)).unwrap()).collect();
        let expected = if sel.len() == fd.changed_lines() {
            // Whole-file path: the result is the other side verbatim.
            Some(if staged { if fresh { Vec::new() } else { old.clone() } } else { new.clone() })
        } else {
            oracle(&base, &fd, &sel, dir)
        };
        let before = index_blob(&root, path);
        let unborn = staged && fresh;
        let res = rt().block_on(stage_lines(
            &root,
            c.clone(),
            v.fingerprint.unwrap(),
            picked.clone(),
            unborn,
            Opts::default(),
        ));
        let ctx = format!("seed {seed} {dir:?}\nold {old:?}\nnew {new:?}\npicked {picked:?}");
        let outcome = match (res, expected) {
            (Ok(_), Some(want)) => {
                let got = index_blob(&root, path);
                let whole = sel.len() == fd.changed_lines();
                if staged && fresh && whole {
                    assert_eq!(got, None, "{ctx}");
                } else if sel.is_empty() {
                    assert_eq!(got, before, "{ctx}");
                } else {
                    assert_eq!(got.as_deref(), Some(&want[..]), "{ctx}");
                }
                if sel.is_empty() {
                    "empty"
                } else if whole {
                    "whole"
                } else if fresh {
                    "partial-new-file"
                } else {
                    "partial"
                }
            }
            (Err(e), None) => {
                assert!(matches!(e.downcast_ref(), Some(patch::PatchError::MissingNewline)), "{ctx}\n{e:#}");
                assert_eq!(index_blob(&root, path), before, "index untouched on refusal\n{ctx}");
                "refused"
            }
            (r, want) => panic!("{ctx}\nresult {r:?}\nexpected {want:?}"),
        };
        // Whatever happened, the worktree file is never touched.
        let wt = fs::read(root.join(path)).unwrap();
        assert_eq!(wt, if staged { b"worktree only\n".to_vec() } else { new }, "{ctx}");
        outcome
    }

    fn random_rounds(seeds: std::ops::RangeInclusive<u64>, dir: Direction) {
        let mut seen: std::collections::BTreeMap<&str, usize> = Default::default();
        for seed in seeds {
            *seen.entry(random_round(seed, dir)).or_default() += 1;
        }
        // Guard against a generator that quietly stops producing interesting cases.
        assert!(seen.get("partial").copied().unwrap_or(0) >= 30, "{seen:?}");
        for kind in ["whole", "refused", "partial-new-file"] {
            assert!(seen.contains_key(kind), "{kind} never exercised: {seen:?}");
        }
        eprintln!("{dir:?}: {seen:?}");
    }

    #[test]
    fn random_partial_stage_matches_oracle() {
        random_rounds(1..=120, Direction::Stage);
    }

    #[test]
    fn random_partial_unstage_matches_oracle() {
        random_rounds(1001..=1120, Direction::Unstage);
    }

    // ───────────── targeted cases ─────────────

    #[test]
    fn stage_and_unstage_single_lines() {
        let (_d, root) = repo();
        fs::write(root.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        run(&root, &["add", "f.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("f.txt"), "one\nA\ntwo\nthree\nB\n").unwrap();

        let c = change(&root, "f.txt", false);
        let (v, _) = view(&root, &c);
        let rt = rt();
        rt.block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows_of(&v, |t| t == "B"), false, Opts::default()))
            .unwrap();
        assert_eq!(index_blob(&root, "f.txt").unwrap(), b"one\ntwo\nthree\nB\n");
        assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"one\nA\ntwo\nthree\nB\n");

        // Stage A too, then unstage B again.
        let c = change(&root, "f.txt", false);
        let (v, _) = view(&root, &c);
        rt.block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows_of(&v, |t| t == "A"), false, Opts::default()))
            .unwrap();
        let c = change(&root, "f.txt", true);
        let (v, _) = view(&root, &c);
        rt.block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows_of(&v, |t| t == "B"), false, Opts::default()))
            .unwrap();
        assert_eq!(index_blob(&root, "f.txt").unwrap(), b"one\nA\ntwo\nthree\n");
    }

    #[test]
    fn stale_selection_is_refused() {
        let (_d, root) = repo();
        fs::write(root.join("f.txt"), "a\n").unwrap();
        run(&root, &["add", "f.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("f.txt"), "a\nb\nc\n").unwrap();
        let c = change(&root, "f.txt", false);
        let (v, _) = view(&root, &c);
        let rows = rows_of(&v, |t| t == "b");
        // The file changes after the user selected lines.
        fs::write(root.join("f.txt"), "a\nX\nb\nc\n").unwrap();
        let err =
            rt().block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows, false, Opts::default())).unwrap_err();
        assert!(err.to_string().contains("changed since you selected"), "{err}");
        assert_eq!(index_blob(&root, "f.txt").unwrap(), b"a\n");
    }

    #[test]
    fn whitespace_is_staged_byte_for_byte() {
        // The repo config says apply.whitespace=fix; trailing spaces and tabs must survive anyway.
        let (_d, root) = repo();
        fs::write(root.join("w.txt"), "x\n").unwrap();
        run(&root, &["add", "w.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("w.txt"), "x\ntrailing  \n\tindented\nmore \t\n").unwrap();
        let c = change(&root, "w.txt", false);
        let (v, _) = view(&root, &c);
        let rows = rows_of(&v, |t| t.starts_with("trailing") || t.starts_with("more"));
        rt().block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows, false, Opts::default())).unwrap();
        assert_eq!(index_blob(&root, "w.txt").unwrap(), b"x\ntrailing  \nmore \t\n");
    }

    #[test]
    fn crlf_worktree_with_autocrlf() {
        let (_d, root) = repo();
        run(&root, &["config", "core.autocrlf", "true"]);
        fs::write(root.join("c.txt"), "a\r\nb\r\n").unwrap();
        run(&root, &["add", "c.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        assert_eq!(index_blob(&root, "c.txt").unwrap(), b"a\nb\n");
        fs::write(root.join("c.txt"), "a\r\nNEW1\r\nb\r\nNEW2\r\n").unwrap();
        let c = change(&root, "c.txt", false);
        let (v, _) = view(&root, &c);
        let rows = rows_of(&v, |t| t.contains("NEW2"));
        assert_eq!(rows.len(), 1);
        rt().block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows, false, Opts::default())).unwrap();
        // Same normalisation `git add` would have done.
        assert_eq!(index_blob(&root, "c.txt").unwrap(), b"a\nb\nNEW2\n");
    }

    #[test]
    fn whole_selection_stages_deletion_not_empty_file() {
        let (_d, root) = repo();
        fs::write(root.join("gone.txt"), "a\nb\n").unwrap();
        run(&root, &["add", "gone.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::remove_file(root.join("gone.txt")).unwrap();
        let c = change(&root, "gone.txt", false);
        let (v, _) = view(&root, &c);
        let rt = rt();
        // One of two lines: still a (shorter) file in the index.
        rt.block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows_of(&v, |t| t == "a"), false, Opts::default()))
            .unwrap();
        assert_eq!(index_blob(&root, "gone.txt").unwrap(), b"b\n");
        // The rest: a real deletion.
        let c = change(&root, "gone.txt", false);
        let (v, _) = view(&root, &c);
        rt.block_on(stage_lines(&root, c, v.fingerprint.unwrap(), rows_of(&v, |_| true), false, Opts::default()))
            .unwrap();
        assert_eq!(index_blob(&root, "gone.txt"), None);
    }

    #[test]
    fn line_staging_is_not_offered_for_risky_files() {
        let (_d, root) = repo();
        // An undefined driver, so the test doesn't depend on git-lfs being installed.
        fs::write(root.join(".gitattributes"), "*.lfs filter=custom\n").unwrap();
        fs::write(root.join("bin.dat"), [0u8, 1, 2]).unwrap();
        fs::write(root.join("old.txt"), "same content here\nline two\nline three\n").unwrap();
        fs::write(root.join("x.lfs"), "pointer\n").unwrap();
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("bin.dat"), [0u8, 9, 9]).unwrap();
        fs::write(root.join("x.lfs"), "pointer\nmore\n").unwrap();
        run(&root, &["mv", "old.txt", "new.txt"]);
        let repo = git::open(&root).unwrap();
        for (path, staged) in [("bin.dat", false), ("x.lfs", false), ("new.txt", true)] {
            let c = change(&root, path, staged);
            let (v, s) = git::file_diff_with_source(&repo, &c, true).unwrap();
            assert!(s.is_none() && v.fingerprint.is_none(), "{path}");
        }
    }

    // ───────────── odd file names ─────────────

    fn odd_names() -> Vec<&'static str> {
        let mut v = vec![
            "-leading-dash.txt",
            "--force",
            "with space.txt",
            "ünïcødé ✓ 日本.txt",
            "[ab].txt",
            "quote'apos.txt",
            "dir with space/-x.txt",
        ];
        // Characters Windows doesn't allow in file names.
        if cfg!(unix) {
            v.extend([
                "*.txt",
                ":(exclude)magic.txt",
                "tab\tname.txt",
                "back\\slash.txt",
                "dq\"uote.txt",
                "new\nline.txt",
            ]);
        }
        v
    }

    #[test]
    fn odd_names_stage_unstage_discard_and_lines() {
        let (_d, root) = repo();
        let rt = rt();
        // Decoys that a glob or magic pathspec would also match.
        for decoy in ["a.txt", "b.txt", "magic.txt"] {
            fs::write(root.join(decoy), "decoy\n").unwrap();
        }
        for name in odd_names() {
            if let Some(dir) = Path::new(name).parent() {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            fs::write(root.join(name), "1\n2\n3\n").unwrap();
        }
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-qm", "base"]);
        for decoy in ["a.txt", "b.txt", "magic.txt"] {
            fs::write(root.join(decoy), "decoy changed\n").unwrap();
        }

        for name in odd_names() {
            let p = root.join(name);
            fs::write(&p, "1\nX\n2\n3\nY\n").unwrap();
            let paths = [name.to_string()];

            rt.block_on(stage_paths(&root, &paths, Opts::default())).unwrap();
            assert_eq!(index_blob(&root, name).unwrap(), b"1\nX\n2\n3\nY\n", "{name}");
            rt.block_on(unstage_paths(&root, &paths, false, Opts::default())).unwrap();
            assert_eq!(index_blob(&root, name).unwrap(), b"1\n2\n3\n", "{name}");

            let c = change(&root, name, false);
            let (v, _) = view(&root, &c);
            rt.block_on(stage_lines(
                &root,
                c,
                v.fingerprint.unwrap(),
                rows_of(&v, |t| t == "Y"),
                false,
                Opts::default(),
            ))
            .unwrap();
            assert_eq!(index_blob(&root, name).unwrap(), b"1\n2\n3\nY\n", "{name}");

            rt.block_on(discard(&root, name, Opts::default())).unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"1\n2\n3\nY\n", "discard returns to the staged content: {name}");
            rt.block_on(unstage_paths(&root, &paths, false, Opts::default())).unwrap();
            rt.block_on(discard(&root, name, Opts::default())).unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"1\n2\n3\n", "{name}");
        }
        // No decoy was staged or discarded along the way.
        for decoy in ["a.txt", "b.txt", "magic.txt"] {
            assert_eq!(fs::read(root.join(decoy)).unwrap(), b"decoy changed\n", "{decoy}");
            assert_eq!(index_blob(&root, decoy).unwrap(), b"decoy\n", "{decoy}");
        }
    }

    // ───────────── destructive actions ─────────────

    #[test]
    fn discard_keeps_staged_and_untracked_work() {
        let (_d, root) = repo();
        fs::write(root.join("f.txt"), "v1\n").unwrap();
        fs::write(root.join("other.txt"), "o1\n").unwrap();
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("f.txt"), "v2\n").unwrap();
        run(&root, &["add", "f.txt"]);
        fs::write(root.join("f.txt"), "v3\n").unwrap();
        fs::write(root.join("other.txt"), "o2\n").unwrap();
        fs::write(root.join("new.txt"), "untracked\n").unwrap();

        rt().block_on(discard(&root, "f.txt", Opts::default())).unwrap();
        assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"v2\n");
        assert_eq!(index_blob(&root, "f.txt").unwrap(), b"v2\n");
        assert_eq!(fs::read(root.join("other.txt")).unwrap(), b"o2\n");
        assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"untracked\n");
    }

    #[test]
    fn switching_with_conflicting_changes_loses_nothing() {
        let (_d, root) = repo();
        fs::write(root.join("f.txt"), "base\n").unwrap();
        run(&root, &["add", "f.txt"]);
        run(&root, &["commit", "-qm", "base"]);
        run(&root, &["switch", "-qc", "other"]);
        fs::write(root.join("f.txt"), "other\n").unwrap();
        run(&root, &["commit", "-qam", "other"]);
        run(&root, &["switch", "-q", "main"]);
        // A branch named like a file must still mean the branch.
        fs::write(root.join("other"), "a file named like the branch\n").unwrap();
        fs::write(root.join("f.txt"), "my unsaved work\n").unwrap();

        let rt = rt();
        let err = rt.block_on(switch(&root, "other", None, Opts::default())).unwrap_err();
        assert!(err.to_string().contains("overwritten"), "{err}");
        assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"my unsaved work\n");
        assert_eq!(fs::read(root.join("other")).unwrap(), b"a file named like the branch\n");
        assert_eq!(run(&root, &["branch", "--show-current"]).trim(), "main");
        assert!(rt.block_on(switch(&root, "-f", None, Opts::default())).is_err(), "option-like names are refused");
    }

    #[test]
    fn commit_and_amend_keep_message_verbatim() {
        let (_d, root) = repo();
        fs::write(root.join("f.txt"), "1\n").unwrap();
        run(&root, &["add", "f.txt"]);
        let rt = rt();
        let msg = "#42 fix: --amend isn't an option here\n\n# not a comment\n-- signature-like line";
        rt.block_on(commit(&root, msg.into(), false, Opts::default())).unwrap();
        assert_eq!(run(&root, &["log", "-1", "--format=%B"]).trim_end(), msg);

        fs::write(root.join("f.txt"), "2\n").unwrap();
        run(&root, &["add", "f.txt"]);
        let first = run(&root, &["rev-parse", "HEAD"]);
        rt.block_on(commit(&root, "Reworded".into(), true, Opts::default())).unwrap();
        assert_ne!(run(&root, &["rev-parse", "HEAD"]), first);
        assert_eq!(run(&root, &["rev-list", "--count", "HEAD"]).trim(), "1", "amend replaces, never stacks");
        assert_eq!(run(&root, &["show", "HEAD:f.txt"]), "2\n");
        assert_eq!(run(&root, &["log", "-1", "--format=%s"]).trim(), "Reworded");
    }

    // ───────────── conflicts ─────────────

    fn conflicted(root: &Path, mine: &str, theirs: Option<&str>) {
        fs::write(root.join("c.txt"), "base\n").unwrap();
        run(root, &["add", "c.txt"]);
        run(root, &["commit", "-qm", "base"]);
        run(root, &["switch", "-qc", "theirs"]);
        match theirs {
            Some(t) => fs::write(root.join("c.txt"), t).unwrap(),
            None => fs::remove_file(root.join("c.txt")).unwrap(),
        }
        run(root, &["commit", "-qam", "theirs"]);
        run(root, &["switch", "-q", "main"]);
        fs::write(root.join("c.txt"), mine).unwrap();
        run(root, &["commit", "-qam", "mine"]);
    }

    #[test]
    fn take_mine_or_theirs_during_merge() {
        for mine in [true, false] {
            let (_d, root) = repo();
            conflicted(&root, "mine\n", Some("theirs\n"));
            let out = std::process::Command::new("git").args(["merge", "theirs"]).current_dir(&root).output().unwrap();
            assert!(!out.status.success());
            rt().block_on(take_side(&root, "c.txt", mine, false, Opts::default())).unwrap();
            let want: &[u8] = if mine { b"mine\n" } else { b"theirs\n" };
            assert_eq!(fs::read(root.join("c.txt")).unwrap(), want);
            assert_eq!(index_blob(&root, "c.txt").unwrap(), want);
            assert!(git::summary(&git::open(&root).unwrap()).unwrap().conflicted == 0);
        }
    }

    #[test]
    fn take_side_during_rebase_means_your_commit() {
        let (_d, root) = repo();
        conflicted(&root, "mine\n", Some("theirs\n"));
        // Rebase main (mine) onto theirs: Git's --theirs is *my* commit here.
        let out = std::process::Command::new("git").args(["rebase", "theirs"]).current_dir(&root).output().unwrap();
        assert!(!out.status.success());
        rt().block_on(take_side(&root, "c.txt", true, true, Opts::default())).unwrap();
        assert_eq!(fs::read(root.join("c.txt")).unwrap(), b"mine\n");
    }

    #[test]
    fn take_side_that_deleted_the_file() {
        let (_d, root) = repo();
        conflicted(&root, "mine\n", None);
        let out = std::process::Command::new("git").args(["merge", "theirs"]).current_dir(&root).output().unwrap();
        assert!(!out.status.success());
        rt().block_on(take_side(&root, "c.txt", false, false, Opts::default())).unwrap();
        assert!(!root.join("c.txt").exists());
        assert_eq!(index_blob(&root, "c.txt"), None);
        assert_eq!(git::summary(&git::open(&root).unwrap()).unwrap().conflicted, 0);
    }

    #[test]
    fn unstage_edited_new_file_before_first_commit() {
        // Regression: `rm --cached` refused when the staged copy differed from the file.
        let (_d, root) = repo();
        fs::write(root.join("n.txt"), "staged\n").unwrap();
        run(&root, &["add", "n.txt"]);
        fs::write(root.join("n.txt"), "edited after staging\n").unwrap();
        let rt = rt();
        rt.block_on(unstage_paths(&root, &["n.txt".into()], true, Opts::default())).unwrap();
        assert_eq!(index_blob(&root, "n.txt"), None);
        assert_eq!(fs::read(root.join("n.txt")).unwrap(), b"edited after staging\n");
        run(&root, &["add", "n.txt"]);
        rt.block_on(unstage_all(&root, true, Opts::default())).unwrap();
        assert_eq!(index_blob(&root, "n.txt"), None);
        assert!(root.join("n.txt").exists());
    }

    // ───────────── repositories that run programs ─────────────

    #[test]
    fn detects_config_and_hooks_that_run_programs() {
        let (_d, root) = repo();
        let open = || git::open(&root).unwrap();
        // A fresh repository only has *.sample hooks and nothing risky.
        assert_eq!(git::programs_run_by_repo(&open()), Vec::<String>::new());
        run(&root, &["config", "core.fsmonitor", "true"]);
        assert!(git::programs_run_by_repo(&open()).is_empty(), "built-in fsmonitor is fine");

        for (k, v) in [
            ("core.fsmonitor", "touch pwned"),
            ("core.sshCommand", "evil"),
            ("filter.Smudgy.clean", "evil"),
            ("diff.x.textconv", "evil"),
            ("core.hooksPath", ".husky"),
            ("include.path", "../elsewhere"),
            ("protocol.ext.allow", "always"),
        ] {
            run(&root, &["config", k, v]);
        }
        fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        let found = git::programs_run_by_repo(&open());
        let has = |needle: &str| found.iter().any(|f| f.starts_with(needle));
        for needle in [
            "core.fsmonitor = touch pwned",
            "core.sshcommand",
            "filter.Smudgy.clean",
            "diff.x.textconv",
            "core.hookspath",
            "include.path",
            "protocol.ext.allow",
            "hook: pre-commit",
        ] {
            assert!(has(needle), "{needle} missing from {found:?}");
        }
        // Ordinary settings are not flagged.
        assert!(!has("user.name") && !has("apply.whitespace"), "{found:?}");
    }

    // ───────────── pulls ─────────────

    #[test]
    fn diverged_pull_can_merge_or_rebase() {
        for mode in [PullMode::Merge, PullMode::Rebase] {
            let (_up, upstream) = repo();
            fs::write(upstream.join("u.txt"), "1\n").unwrap();
            run(&upstream, &["add", "u.txt"]);
            run(&upstream, &["commit", "-qm", "base"]);
            let dir = tempfile::tempdir().unwrap();
            let local = dir.path().join("clone");
            run(dir.path(), &["clone", "-q", "-c", "core.autocrlf=false", upstream.to_str().unwrap(), "clone"]);
            for (k, v) in [("user.name", "T"), ("user.email", "t@e.x"), ("pull.rebase", "false"), ("pull.ff", "only")] {
                run(&local, &["config", k, v]);
            }
            fs::write(upstream.join("u.txt"), "2\n").unwrap();
            run(&upstream, &["commit", "-qam", "theirs"]);
            fs::write(local.join("l.txt"), "mine\n").unwrap();
            run(&local, &["add", "l.txt"]);
            run(&local, &["commit", "-qm", "mine"]);

            let rt = rt();
            let err = rt.block_on(pull(&local, PullMode::Default, Opts::default())).unwrap_err();
            assert!(is_divergence(&err), "{err:?}");
            rt.block_on(pull(&local, mode, Opts::default())).unwrap();
            assert_eq!(fs::read(local.join("u.txt")).unwrap(), b"2\n");
            assert_eq!(fs::read(local.join("l.txt")).unwrap(), b"mine\n");
            let parents = run(&local, &["log", "-1", "--format=%P"]);
            assert_eq!(parents.split_whitespace().count(), if mode == PullMode::Merge { 2 } else { 1 });
        }
    }
}
