//! Local, read-mostly Git access through libgit2. Everything here is synchronous
//! and must run on a worker thread. Mutations that should honour hooks, filters
//! (LFS, autocrlf) and signing go through the Git CLI instead (see `cli.rs`).

use crate::highlight::{self, Highlighter};
use crate::patch;
use crate::util;
use anyhow::{Result, anyhow, bail};
use git2::{
    AttrCheckFlags, AttrValue, BranchType, Delta, DiffFindOptions, DiffFormat, DiffOptions, ErrorCode, FileMode, Oid,
    Repository, RepositoryState, Sort, StatusOptions,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Hard caps that keep a pathological diff from eating memory or the UI.
pub const MAX_DIFF_LINES: usize = 5_000;
const MAX_LINE_CHARS: usize = 800;
pub const LARGE_FILE_BYTES: u64 = 3 * 1024 * 1024;
const MAX_AHEAD_BEHIND_BRANCHES: usize = 60;

pub mod kind {
    pub const CONTEXT: i32 = 0;
    pub const ADD: i32 = 1;
    pub const DEL: i32 = 2;
    pub const HUNK: i32 = 3;
    pub const FILE: i32 = 4;
    pub const NOTE: i32 = 5;
    pub const MARKER: i32 = 6;
    pub const OURS: i32 = 7;
    pub const THEIRS: i32 = 8;
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: char,
    pub staged: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub root: PathBuf,
    pub name: String,
    pub branch: String,
    pub head: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub state: String,
    pub remote: Option<String>,
    pub github_slug: Option<String>,
    pub stash_count: usize,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub conflicted: usize,
}

impl Summary {
    /// Tracked changes that a checkout could carry or clobber (untracked files don't count).
    pub fn has_tracked_changes(&self) -> bool {
        !self.staged.is_empty() || self.unstaged.iter().any(|c| c.status != '?')
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: i32,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    /// File-header rows only: "added", "deleted", "renamed", ...
    pub tag: &'static str,
    /// `(hunk, line within hunk)` for context/`+`/`-` rows: the key into
    /// [`patch::FileDiff`] used by line staging.
    pub pos: Option<(u32, u32)>,
    /// Syntax-coloured runs with changed words marked; empty = plain text.
    pub segs: Vec<highlight::Seg>,
}

fn note(text: &str) -> Line {
    Line { kind: kind::NOTE, old_no: None, new_no: None, text: text.to_string(), tag: "", pos: None, segs: Vec::new() }
}

#[derive(Clone, Debug, Default)]
pub struct DiffView {
    pub lines: Vec<Line>,
    pub truncated: bool,
    pub too_large: Option<u64>,
    pub binary: bool,
    pub adds: usize,
    pub dels: usize,
    pub files: usize,
    pub max_chars: usize,
    /// Set when individual lines of this diff can be staged/unstaged; identifies
    /// the exact diff the user is selecting on.
    pub fingerprint: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub id: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
    pub refs: Vec<String>,
    pub unpushed: bool,
    pub merge: bool,
}

#[derive(Clone, Debug)]
pub struct CommitDetail {
    pub id: String,
    pub summary: String,
    pub body: String,
    pub author: String,
    pub email: String,
    pub time: i64,
    pub offset_minutes: i32,
    pub parents: Vec<String>,
    pub diff: DiffView,
}

#[derive(Clone, Debug)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub is_remote: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub time: i64,
    pub summary: String,
}

pub fn open(path: &Path) -> Result<Repository> {
    let repo = Repository::discover(path)
        .map_err(|e| anyhow!("{} is not inside a Git repository ({})", path.display(), e.message()))?;
    if repo.is_bare() {
        bail!("Bare repositories have no working tree and aren't supported.");
    }
    Ok(repo)
}

pub fn workdir(repo: &Repository) -> PathBuf {
    util::normalize_path(repo.workdir().unwrap_or_else(|| repo.path()))
}

pub fn short(oid: Oid) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn state_label(s: RepositoryState) -> &'static str {
    use RepositoryState::*;
    match s {
        Clean => "",
        Merge => "Merging",
        Revert | RevertSequence => "Reverting",
        CherryPick | CherryPickSequence => "Cherry-picking",
        Bisect => "Bisecting",
        Rebase | RebaseInteractive | RebaseMerge => "Rebasing",
        ApplyMailbox | ApplyMailboxOrRebase => "Applying patches",
    }
}

/// Preferred remote: `origin`, then `upstream`, then whatever comes first.
pub fn default_remote(repo: &Repository) -> Option<String> {
    let names = repo.remotes().ok()?;
    let names: Vec<String> = names.iter().flatten().flatten().map(str::to_string).collect();
    ["origin", "upstream"]
        .iter()
        .find(|n| names.iter().any(|x| x == *n))
        .map(|s| s.to_string())
        .or_else(|| names.first().cloned())
}

pub fn github_slug(repo: &Repository) -> Option<String> {
    let mut names: Vec<String> = repo.remotes().ok()?.iter().flatten().flatten().map(str::to_string).collect();
    names.sort_by_key(|n| match n.as_str() {
        "origin" => 0,
        "upstream" => 1,
        _ => 2,
    });
    names.iter().find_map(|n| util::github_slug(repo.find_remote(n).ok()?.url().ok()?))
}

pub fn summary(repo: &Repository) -> Result<Summary> {
    let root = workdir(repo);
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
    let mut s = Summary { root, name, ..Default::default() };

    match repo.head() {
        Ok(head) => {
            s.head = head.target().map(|o| o.to_string());
            if repo.head_detached().unwrap_or(false) {
                s.detached = true;
                s.branch = head.target().map(short).unwrap_or_else(|| "HEAD".into());
            } else {
                s.branch = head.shorthand().unwrap_or("HEAD").to_string();
                if let (Some(local), Ok(up)) =
                    (head.target(), repo.find_branch(&s.branch, BranchType::Local).and_then(|b| b.upstream()))
                {
                    s.upstream = up.name().ok().flatten().map(str::to_string);
                    if let Some(remote_oid) = up.get().target()
                        && let Ok((a, b)) = repo.graph_ahead_behind(local, remote_oid)
                    {
                        s.ahead = a;
                        s.behind = b;
                    }
                }
            }
        }
        Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => {
            s.unborn = true;
            s.branch = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| {
                    r.symbolic_target().ok().flatten().map(|t| t.trim_start_matches("refs/heads/").to_string())
                })
                .unwrap_or_else(|| "main".into());
        }
        Err(e) => return Err(e.into()),
    }

    s.state = state_label(repo.state()).to_string();
    s.remote = default_remote(repo);
    s.github_slug = github_slug(repo);
    s.stash_count = repo.reflog("refs/stash").map(|r| r.len()).unwrap_or(0);
    let (staged, unstaged, conflicted) = status(repo)?;
    s.staged = staged;
    s.unstaged = unstaged;
    s.conflicted = conflicted;
    Ok(s)
}

pub fn status(repo: &Repository) -> Result<(Vec<FileChange>, Vec<FileChange>, usize)> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .include_ignored(false)
        .exclude_submodules(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    let (mut staged, mut unstaged, mut conflicted) = (Vec::new(), Vec::new(), 0);

    for e in statuses.iter() {
        let st = e.status();
        let path = String::from_utf8_lossy(e.path_bytes()).into_owned();
        if st.is_conflicted() {
            conflicted += 1;
            unstaged.push(FileChange { path, old_path: None, status: 'U', staged: false });
            continue;
        }
        let index_code = if st.is_index_new() {
            Some('A')
        } else if st.is_index_modified() {
            Some('M')
        } else if st.is_index_deleted() {
            Some('D')
        } else if st.is_index_renamed() {
            Some('R')
        } else if st.is_index_typechange() {
            Some('T')
        } else {
            None
        };
        if let Some(code) = index_code {
            let (p, old) = match (code, e.head_to_index()) {
                ('R', Some(d)) => {
                    (d.new_file().path().map(lossy).unwrap_or_else(|| path.clone()), d.old_file().path().map(lossy))
                }
                _ => (path.clone(), None),
            };
            staged.push(FileChange { path: p, old_path: old, status: code, staged: true });
        }
        let wt_code = if st.is_wt_new() {
            Some('?')
        } else if st.is_wt_modified() {
            Some('M')
        } else if st.is_wt_deleted() {
            Some('D')
        } else if st.is_wt_typechange() {
            Some('T')
        } else if st.is_wt_renamed() {
            Some('R')
        } else {
            None
        };
        if let Some(code) = wt_code {
            unstaged.push(FileChange { path, old_path: None, status: code, staged: false });
        }
    }
    staged.sort_by(|a, b| a.path.cmp(&b.path));
    unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((staged, unstaged, conflicted))
}

fn head_tree(repo: &Repository) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(h) => Ok(Some(h.peel_to_tree()?)),
        Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Diff of one file: HEAD→index when staged, index→worktree otherwise.
pub fn file_diff(repo: &Repository, change: &FileChange, force: bool) -> Result<DiffView> {
    Ok(file_diff_with_source(repo, change, force)?.0)
}

/// [`file_diff`] plus the raw hunks for line staging, when the file supports it
/// (then `view.fingerprint` is set and every `+`/`-` row carries its `pos`).
pub fn file_diff_with_source(
    repo: &Repository,
    change: &FileChange,
    force: bool,
) -> Result<(DiffView, Option<patch::FileDiff>)> {
    let root = workdir(repo);
    if change.status == 'U' {
        return Ok((conflict_view(&root.join(&change.path))?, None));
    }
    if !change.staged
        && !force
        && let Ok(md) = std::fs::metadata(root.join(&change.path))
        && md.is_file()
        && md.len() > LARGE_FILE_BYTES
    {
        return Ok((DiffView { too_large: Some(md.len()), ..Default::default() }, None));
    }
    let diff = change_diff(repo, change)?;
    let mut view = render(&diff, false)?;
    decorate(&mut view.lines, Some(&change.path));
    // Any doubt about the file means no line staging, never a wrong patch.
    let source = line_source(repo, change, &diff, &view).ok().flatten();
    view.fingerprint = source.as_ref().map(patch::FileDiff::fingerprint);
    Ok((view, source))
}

fn regular(mode: FileMode) -> bool {
    matches!(mode, FileMode::Blob | FileMode::BlobExecutable)
}

/// Attributes under which libgit2's view of the content may differ from what
/// `git apply`/`git add` would store (external filters, encodings, keyword expansion).
fn has_content_filters(repo: &Repository, path: &str) -> bool {
    ["filter", "working-tree-encoding", "ident"].iter().any(|name| {
        match repo.get_attr_bytes(Path::new(path), name, AttrCheckFlags::FILE_THEN_INDEX) {
            Ok(v) => !matches!(AttrValue::from_bytes(v), AttrValue::Unspecified | AttrValue::False),
            Err(_) => true,
        }
    })
}

/// Raw hunks for line staging, cross-checked row by row against the rendered view.
fn line_source(
    repo: &Repository,
    change: &FileChange,
    diff: &git2::Diff<'_>,
    view: &DiffView,
) -> Result<Option<patch::FileDiff>> {
    if !matches!(change.status, 'M' | 'A' | 'D' | '?')
        || change.old_path.is_some()
        || view.binary
        || view.truncated
        || view.too_large.is_some()
        || diff.deltas().len() != 1
        || has_content_filters(repo, &change.path)
    {
        return Ok(None);
    }
    let Some(p) = git2::Patch::from_diff(diff, 0)? else { return Ok(None) };
    let delta = p.delta();
    if delta.flags().is_binary() {
        return Ok(None);
    }
    let (old_absent, new_absent) = match delta.status() {
        Delta::Modified => (false, false),
        Delta::Added | Delta::Untracked => (true, false),
        Delta::Deleted => (false, true),
        _ => return Ok(None),
    };
    let (old_mode, new_mode) = (delta.old_file().mode(), delta.new_file().mode());
    if (!old_absent && !regular(old_mode)) || (!new_absent && !regular(new_mode)) {
        return Ok(None);
    }
    let mut fd = patch::FileDiff {
        hunks: Vec::with_capacity(p.num_hunks()),
        old_absent,
        new_absent,
        mode: u32::from(if new_absent { old_mode } else { new_mode }),
    };
    for h in 0..p.num_hunks() {
        let (hunk, n) = p.hunk(h)?;
        let mut lines = Vec::with_capacity(n);
        for l in 0..n {
            let line = p.line_in_hunk(h, l)?;
            if let o @ (' ' | '+' | '-') = line.origin() {
                lines.push(patch::PLine { origin: o as u8, content: line.content().to_vec() });
            }
        }
        fd.hunks.push(patch::PHunk {
            old_start: hunk.old_start(),
            old_lines: hunk.old_lines(),
            new_start: hunk.new_start(),
            new_lines: hunk.new_lines(),
            lines,
        });
    }
    if fd.hunks.is_empty() {
        return Ok(None);
    }
    // The UI maps a clicked row to `pos`; prove that mapping matches these hunks.
    let mut rows = 0;
    for row in &view.lines {
        let Some((h, l)) = row.pos else { continue };
        let Some(raw) = fd.hunks.get(h as usize).and_then(|x| x.lines.get(l as usize)) else { return Ok(None) };
        let kind = match raw.origin {
            b'+' => kind::ADD,
            b'-' => kind::DEL,
            _ => kind::CONTEXT,
        };
        if kind != row.kind || clean_line(&raw.content) != row.text {
            return Ok(None);
        }
        rows += 1;
    }
    if rows != fd.hunks.iter().map(|h| h.lines.len()).sum::<usize>() {
        return Ok(None);
    }
    Ok(Some(fd))
}

fn change_diff<'r>(repo: &'r Repository, change: &FileChange) -> Result<git2::Diff<'r>> {
    let mut opts = DiffOptions::new();
    opts.pathspec(&change.path)
        .disable_pathspec_match(true)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .context_lines(3);
    if let Some(old) = &change.old_path {
        opts.pathspec(old);
    }

    let mut diff = if change.staged {
        let tree = head_tree(repo)?;
        repo.diff_tree_to_index(tree.as_ref(), None, Some(&mut opts))?
    } else {
        repo.diff_index_to_workdir(None, Some(&mut opts))?
    };
    if change.old_path.is_some() {
        diff.find_similar(Some(DiffFindOptions::new().renames(true)))?;
    }
    Ok(diff)
}

fn clean_line(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim_end_matches(['\n', '\r']);
    let mut out = String::with_capacity(s.len().min(MAX_LINE_CHARS + 8));
    let mut n = 0;
    for ch in s.chars() {
        if n >= MAX_LINE_CHARS {
            out.push('…');
            break;
        }
        if ch == '\t' {
            out.push_str("    ");
            n += 4;
        } else if ch.is_control() {
            out.push('·');
            n += 1;
        } else {
            out.push(ch);
            n += 1;
        }
    }
    out
}

fn render(diff: &git2::Diff<'_>, file_headers: bool) -> Result<DiffView> {
    let mut v = DiffView::default();
    if let Ok(stats) = diff.stats() {
        v.adds = stats.insertions();
        v.dels = stats.deletions();
        v.files = stats.files_changed();
    }
    let mut current_file: Option<String> = None;
    let (mut hunk_no, mut line_no) = (u32::MAX, 0u32);

    let result = diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        if v.lines.len() >= MAX_DIFF_LINES {
            v.truncated = true;
            return false;
        }
        if file_headers {
            let new_path = delta.new_file().path().map(lossy);
            let old_path = delta.old_file().path().map(lossy);
            let path = new_path.clone().or(old_path.clone()).unwrap_or_default();
            if current_file.as_deref() != Some(path.as_str()) {
                let (label, text) = match delta.status() {
                    Delta::Added | Delta::Untracked => ("added", path.clone()),
                    Delta::Deleted => ("deleted", path.clone()),
                    Delta::Renamed => ("renamed", format!("{} → {}", old_path.unwrap_or_default(), path)),
                    Delta::Copied => ("copied", path.clone()),
                    Delta::Typechange => ("type changed", path.clone()),
                    _ => ("modified", path.clone()),
                };
                v.lines.push(Line {
                    kind: kind::FILE,
                    old_no: None,
                    new_no: None,
                    text,
                    tag: label,
                    pos: None,
                    segs: Vec::new(),
                });
                current_file = Some(path);
                hunk_no = u32::MAX;
            }
        }
        let k = match line.origin() {
            '+' => kind::ADD,
            '-' => kind::DEL,
            ' ' => kind::CONTEXT,
            'H' => {
                hunk_no = hunk_no.wrapping_add(1);
                line_no = 0;
                kind::HUNK
            }
            'B' => {
                v.binary = true;
                v.lines.push(note("Binary file — no text diff"));
                return true;
            }
            '=' | '>' | '<' => {
                v.lines.push(note("No newline at end of file"));
                return true;
            }
            _ => return true,
        };
        let text = clean_line(line.content());
        v.max_chars = v.max_chars.max(text.chars().count());
        let pos = (k != kind::HUNK && hunk_no != u32::MAX).then(|| {
            line_no += 1;
            (hunk_no, line_no - 1)
        });
        v.lines.push(Line {
            kind: k,
            old_no: line.old_lineno(),
            new_no: line.new_lineno(),
            text,
            tag: "",
            pos,
            segs: Vec::new(),
        });
        true
    });
    match result {
        Ok(()) => {}
        Err(_) if v.truncated => {}
        Err(e) => return Err(e.into()),
    }
    // Single-file diffs that are binary: show the empty-state instead of a lone note.
    if !file_headers && v.binary {
        v.lines.clear();
    }
    Ok(v)
}

/// Adds syntax colouring and word-level change emphasis to rendered diff rows.
/// `path` is the file of a single-file diff; multi-file diffs carry it in their
/// FILE rows. Files in languages we don't know stay plain.
fn decorate(lines: &mut [Line], path: Option<&str>) {
    let mut lang = path.and_then(highlight::lang_for);
    let (mut old_h, mut new_h) = (lang.map(Highlighter::new), lang.map(Highlighter::new));
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            kind::FILE => {
                let p = lines[i].text.rsplit(" → ").next().unwrap_or(&lines[i].text).to_string();
                lang = highlight::lang_for(&p);
                old_h = lang.map(Highlighter::new);
                new_h = lang.map(Highlighter::new);
                i += 1;
            }
            kind::HUNK => {
                // A hunk may start anywhere; assume we're not inside a comment.
                for h in [&mut old_h, &mut new_h].into_iter().flatten() {
                    h.reset();
                }
                i += 1;
            }
            kind::CONTEXT => {
                if let (Some(nh), Some(oh)) = (new_h.as_mut(), old_h.as_mut()) {
                    let toks = nh.line(&lines[i].text);
                    oh.line(&lines[i].text);
                    lines[i].segs = highlight::segments(&lines[i].text, &toks, &[]);
                }
                i += 1;
            }
            kind::DEL | kind::ADD => {
                // A run of deletions followed by the additions that replace them.
                let del_start = i;
                while i < lines.len() && lines[i].kind == kind::DEL {
                    i += 1;
                }
                let add_start = i;
                while i < lines.len() && lines[i].kind == kind::ADD {
                    i += 1;
                }
                let (dels, adds) = (add_start - del_start, i - add_start);
                let mut emph: Vec<Vec<(usize, usize)>> = vec![Vec::new(); i - del_start];
                for k in 0..dels.min(adds) {
                    let (d, a) = (del_start + k, add_start + k);
                    if let Some((de, ae)) = highlight::changed_ranges(&lines[d].text, &lines[a].text) {
                        emph[d - del_start] = de;
                        emph[a - del_start] = ae;
                    }
                }
                for (j, line) in lines[del_start..i].iter_mut().enumerate() {
                    let h = if line.kind == kind::DEL { old_h.as_mut() } else { new_h.as_mut() };
                    let toks = h.map(|h| h.line(&line.text)).unwrap_or_default();
                    if !toks.is_empty() || !emph[j].is_empty() {
                        line.segs = highlight::segments(&line.text, &toks, &emph[j]);
                    }
                }
            }
            _ => i += 1,
        }
    }
}

/// Working-tree view of a conflicted file with ours/theirs regions tinted.
fn conflict_view(path: &Path) -> Result<DiffView> {
    let md = std::fs::metadata(path)?;
    if md.len() > LARGE_FILE_BYTES {
        return Ok(DiffView { too_large: Some(md.len()), ..Default::default() });
    }
    let bytes = std::fs::read(path)?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return Ok(DiffView { binary: true, ..Default::default() });
    }
    let mut v = DiffView::default();
    let mut region = kind::CONTEXT;
    let mut in_conflict = false;
    for (i, raw) in bytes.split(|b| *b == b'\n').enumerate() {
        if v.lines.len() >= MAX_DIFF_LINES {
            v.truncated = true;
            break;
        }
        let text = clean_line(raw);
        let k = if text.starts_with("<<<<<<<") {
            in_conflict = true;
            region = kind::OURS;
            kind::MARKER
        } else if in_conflict && text.starts_with("|||||||") {
            region = kind::CONTEXT; // diff3 base section
            kind::MARKER
        } else if in_conflict && text.starts_with("=======") {
            region = kind::THEIRS;
            kind::MARKER
        } else if in_conflict && text.starts_with(">>>>>>>") {
            in_conflict = false;
            region = kind::CONTEXT;
            kind::MARKER
        } else {
            region
        };
        v.max_chars = v.max_chars.max(text.chars().count());
        v.lines.push(Line {
            kind: k,
            old_no: None,
            new_no: Some(i as u32 + 1),
            text,
            tag: "",
            pos: None,
            segs: Vec::new(),
        });
    }
    Ok(v)
}

/// Branch/tag names pointing at each commit, for history decorations.
fn decorations(repo: &Repository) -> HashMap<Oid, Vec<String>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    let Ok(refs) = repo.references() else { return map };
    for r in refs.flatten() {
        if !(r.is_branch() || r.is_remote() || r.is_tag()) {
            continue;
        }
        let Ok(name) = r.shorthand() else { continue };
        if r.is_remote() && name.ends_with("/HEAD") {
            continue;
        }
        let label = if r.is_tag() { format!("tag {name}") } else { name.to_string() };
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id()).or_default().push(label);
        }
    }
    map
}

/// Commits reachable from HEAD but not from its upstream (capped).
fn unpushed(repo: &Repository) -> HashSet<Oid> {
    let mut set = HashSet::new();
    let Ok(head) = repo.head() else { return set };
    let Ok(branch) = head.shorthand().map(str::to_string) else { return set };
    let Ok(up) = repo.find_branch(&branch, BranchType::Local).and_then(|b| b.upstream()) else { return set };
    let (Some(local), Some(remote)) = (head.target(), up.get().target()) else { return set };
    let Ok(mut walk) = repo.revwalk() else { return set };
    if walk.push(local).is_err() || walk.hide(remote).is_err() {
        return set;
    }
    set.extend(walk.flatten().take(2000));
    set
}

pub fn history(repo: &Repository, skip: usize, limit: usize) -> Result<Vec<CommitInfo>> {
    let mut walk = repo.revwalk()?;
    if walk.push_head().is_err() {
        return Ok(Vec::new()); // unborn branch
    }
    // TIME (not TOPOLOGICAL) keeps the walk incremental on huge repositories.
    walk.set_sorting(Sort::TIME)?;
    let decorations = decorations(repo);
    let unpushed = unpushed(repo);
    let mut out = Vec::with_capacity(limit);
    for oid in walk.skip(skip).take(limit) {
        let oid = oid?;
        let c = repo.find_commit(oid)?;
        out.push(CommitInfo {
            id: oid.to_string(),
            summary: c.summary().ok().flatten().unwrap_or("(no message)").to_string(),
            author: c.author().name().unwrap_or("unknown").to_string(),
            // Author date, like GitHub shows (committer date changes on rebase/amend).
            time: c.author().when().seconds(),
            refs: decorations.get(&oid).cloned().unwrap_or_default(),
            unpushed: unpushed.contains(&oid),
            merge: c.parent_count() > 1,
        });
    }
    Ok(out)
}

pub fn commit_detail(repo: &Repository, id: &str) -> Result<CommitDetail> {
    let oid = Oid::from_str(id)?;
    let c = repo.find_commit(oid)?;
    let tree = c.tree()?;
    let parent_tree = if c.parent_count() > 0 { Some(c.parent(0)?.tree()?) } else { None };
    let mut opts = DiffOptions::new();
    opts.context_lines(3);
    let mut diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
    diff.find_similar(Some(DiffFindOptions::new().renames(true)))?;
    let message = c.message().unwrap_or("");
    let summary = c.summary().ok().flatten().unwrap_or("(no message)").to_string();
    let body = message.trim_start().split_once('\n').map(|x| x.1.trim().to_string()).unwrap_or_default();
    let author = c.author();
    Ok(CommitDetail {
        id: oid.to_string(),
        summary,
        body,
        author: author.name().unwrap_or("unknown").to_string(),
        email: author.email().unwrap_or("").to_string(),
        time: author.when().seconds(),
        offset_minutes: author.when().offset_minutes(),
        parents: c.parent_ids().map(short).collect(),
        diff: {
            let mut v = render(&diff, true)?;
            decorate(&mut v.lines, None);
            v
        },
    })
}

pub fn head_message(repo: &Repository) -> Option<String> {
    let c = repo.head().ok()?.peel_to_commit().ok()?;
    c.message().ok().map(str::to_string)
}

pub fn branches(repo: &Repository) -> Result<Vec<BranchInfo>> {
    let mut out = Vec::new();
    let mut local_count = 0;
    for item in repo.branches(None)? {
        let (b, ty) = item?;
        let reference = b.get();
        if matches!(reference.symbolic_target(), Ok(Some(_))) {
            continue; // origin/HEAD
        }
        let Some(name) = b.name().ok().flatten().map(str::to_string) else { continue };
        let is_remote = ty == BranchType::Remote;
        let commit = reference.peel_to_commit().ok();
        let mut info = BranchInfo {
            name,
            is_head: b.is_head(),
            is_remote,
            upstream: None,
            ahead: 0,
            behind: 0,
            time: commit.as_ref().map(|c| c.time().seconds()).unwrap_or(0),
            summary: commit.as_ref().and_then(|c| c.summary().ok().flatten().map(str::to_string)).unwrap_or_default(),
        };
        if !is_remote {
            local_count += 1;
            if let Ok(up) = b.upstream() {
                info.upstream = up.name().ok().flatten().map(str::to_string);
                if local_count <= MAX_AHEAD_BEHIND_BRANCHES
                    && let (Some(l), Some(r)) = (reference.target(), up.get().target())
                    && let Ok((a, bh)) = repo.graph_ahead_behind(l, r)
                {
                    info.ahead = a;
                    info.behind = bh;
                }
            }
        }
        out.push(info);
    }
    out.sort_by(|a, b| {
        (a.is_remote, !a.is_head, std::cmp::Reverse(a.time)).cmp(&(b.is_remote, !b.is_head, std::cmp::Reverse(b.time)))
    });
    Ok(out)
}

pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && git2::Branch::name_is_valid(name).unwrap_or(false)
}

/// Which sides of a conflicted path exist: `(ours, theirs)`. A missing side
/// means that side deleted the file.
pub fn conflict_sides(repo: &Repository, path: &str) -> Result<(bool, bool)> {
    let index = repo.index()?;
    for c in index.conflicts()? {
        let c = c?;
        let entry = c.our.as_ref().or(c.their.as_ref()).or(c.ancestor.as_ref());
        if entry.is_some_and(|e| e.path == path.as_bytes()) {
            return Ok((c.our.is_some(), c.their.is_some()));
        }
    }
    bail!("{path} is no longer conflicted")
}

/// Repository-local settings and hooks that make Git run a program. A repository
/// handed over as a zip can carry these, and they would run as soon as Gitust
/// stages, commits, pulls or pushes. Libgit2 (used for every read) runs none of them.
pub fn programs_run_by_repo(repo: &Repository) -> Vec<String> {
    let mut found = Vec::new();
    for file in [repo.commondir().join("config"), repo.path().join("config.worktree")] {
        if !file.is_file() {
            continue;
        }
        let Ok(cfg) = git2::Config::open(&file) else {
            found.push(format!("{} could not be read", file.display()));
            continue;
        };
        let Ok(mut entries) = cfg.entries(None) else { continue };
        while let Some(Ok(e)) = entries.next() {
            let (Ok(name), value) = (e.name(), e.value().unwrap_or("")) else { continue };
            if config_runs_program(name, value) {
                let v: String = value.chars().take(100).collect();
                let more = if value.chars().count() > 100 { "…" } else { "" };
                found.push(format!("{name} = {v}{more}"));
            }
        }
    }
    if let Ok(dir) = std::fs::read_dir(repo.commondir().join("hooks")) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|t| t.is_file()) && !name.ends_with(".sample") {
                found.push(format!("hook: {name}"));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn config_runs_program(name: &str, value: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let Some((section, rest)) = n.split_once('.') else { return false };
    let sub = rest.contains('.');
    let key = rest.rsplit('.').next().unwrap_or(rest);
    let is_bool = matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
    );
    match (section, key, sub) {
        // `true` means Git's built-in daemon; anything else is a command.
        ("core", "fsmonitor", false) => !is_bool,
        (
            "core",
            "hookspath" | "sshcommand" | "gitproxy" | "askpass" | "editor" | "pager" | "alternaterefscommand",
            false,
        ) => true,
        ("sequence", "editor", false) | ("diff", "external", false) | ("include", "path", false) => true,
        ("uploadpack", "packobjectshook", false) => true,
        ("credential", "helper", _) | ("gpg", "program", _) => true,
        ("filter", "clean" | "smudge" | "process", true) => true,
        ("diff", "textconv" | "command", true) | ("merge", "driver", true) => true,
        ("remote", "uploadpack" | "receivepack", true) | ("includeif", "path", true) => true,
        // `ext::` URLs run arbitrary commands.
        ("protocol", "allow", true) => rest.starts_with("ext.") && !value.eq_ignore_ascii_case("never"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sig() -> git2::Signature<'static> {
        git2::Signature::now("Test", "test@example.com").unwrap()
    }

    fn commit_all(repo: &Repository, msg: &str) -> Oid {
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parents: Vec<git2::Commit> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig(), &sig(), msg, &tree, &refs).unwrap()
    }

    #[test]
    fn unborn_repo_summary() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let s = summary(&repo).unwrap();
        assert!(s.unborn);
        assert_eq!(s.unstaged.len(), 1);
        assert_eq!(s.unstaged[0].status, '?');
        assert!(!s.has_tracked_changes());
        assert!(history(&repo, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn status_diff_history() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        commit_all(&repo, "first\n\nbody text");
        fs::write(dir.path().join("a.txt"), "one\n2\nthree\n").unwrap();
        fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();

        let s = summary(&repo).unwrap();
        assert!(!s.unborn);
        assert!(s.has_tracked_changes());
        let modified = s.unstaged.iter().find(|c| c.path == "a.txt").unwrap();
        assert_eq!(modified.status, 'M');

        let d = file_diff(&repo, modified, false).unwrap();
        assert_eq!((d.adds, d.dels), (1, 1));
        assert!(d.lines.iter().any(|l| l.kind == kind::ADD && l.text == "2"));
        assert!(d.lines.iter().any(|l| l.kind == kind::HUNK));

        let bin = s.unstaged.iter().find(|c| c.path == "b.bin").unwrap();
        let bd = file_diff(&repo, bin, false).unwrap();
        assert!(bd.binary && bd.lines.is_empty());

        commit_all(&repo, "second");
        let h = history(&repo, 0, 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].summary, "second");
        let detail = commit_detail(&repo, &h[1].id).unwrap();
        assert_eq!(detail.body, "body text");
        assert!(detail.diff.lines.iter().any(|l| l.kind == kind::FILE));
        assert_eq!(head_message(&repo).as_deref(), Some("second"));
    }

    #[test]
    fn large_files_are_gated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("big.txt"), "x\n".repeat((LARGE_FILE_BYTES as usize) / 2 + 10)).unwrap();
        let s = summary(&repo).unwrap();
        let c = &s.unstaged[0];
        assert!(file_diff(&repo, c, false).unwrap().too_large.is_some());
        let forced = file_diff(&repo, c, true).unwrap();
        assert!(forced.truncated && forced.lines.len() == MAX_DIFF_LINES);
    }

    #[test]
    fn conflict_regions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.txt");
        fs::write(&p, "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> other\nz\n").unwrap();
        let v = conflict_view(&p).unwrap();
        let kinds: Vec<i32> = v.lines.iter().map(|l| l.kind).collect();
        assert_eq!(&kinds[..7], &[0, 6, 7, 6, 8, 6, 0]);
    }

    /// `GITUST_BENCH_REPO=<path> cargo test --release -- --ignored --nocapture bench_repo`
    #[test]
    #[ignore]
    fn bench_repo() {
        let Some(path) = std::env::var_os("GITUST_BENCH_REPO") else { return };
        let t = std::time::Instant::now();
        let repo = open(Path::new(&path)).unwrap();
        let open_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let s = summary(&repo).unwrap();
        let status_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let h = history(&repo, 0, 300).unwrap();
        let history_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let b = branches(&repo).unwrap();
        let branches_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let d = s.unstaged.first().map(|c| file_diff(&repo, c, false).unwrap());
        let diff_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let detail = commit_detail(&repo, &h[0].id).unwrap();
        let detail_ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "open {open_ms:.1} ms | status ({} changed) {status_ms:.1} ms | history (first {}) {history_ms:.1} ms | branches ({}) {branches_ms:.1} ms | file diff ({} lines) {diff_ms:.1} ms | commit detail ({} lines) {detail_ms:.1} ms",
            s.staged.len() + s.unstaged.len(),
            h.len(),
            b.len(),
            d.map(|d| d.lines.len()).unwrap_or(0),
            detail.diff.lines.len()
        );
    }

    #[test]
    fn branch_names() {
        assert!(valid_branch_name("feature/x"));
        assert!(!valid_branch_name("-x"));
        assert!(!valid_branch_name("a..b"));
        assert!(!valid_branch_name("has space"));
    }
}
