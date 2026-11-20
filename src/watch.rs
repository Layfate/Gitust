//! Debounced working-tree watcher. Filters out ignored paths (build output,
//! node_modules, …) and Git's internal churn so idle CPU stays ~0%.

use anyhow::Result;
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub type RepoWatcher = Debouncer<RecommendedWatcher>;

/// Files under `.git/` whose change means the visible state changed.
const GIT_SIGNALS: &[&str] = &[
    "HEAD",
    "index",
    "refs",
    "packed-refs",
    "MERGE_HEAD",
    "FETCH_HEAD",
    "ORIG_HEAD",
    "REBASE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "rebase-apply",
    "logs", // stash reflog
];

pub fn watch(root: PathBuf, on_change: impl Fn() + Send + 'static) -> Result<RepoWatcher> {
    let mut repo = git2::Repository::open(&root).ok();
    let mut ignored_tops: HashSet<OsString> = HashSet::new();
    let base = root.clone();
    let mut debouncer = new_debouncer(Duration::from_millis(250), move |res: DebounceEventResult| {
        let Ok(events) = res else { return };
        let relevant = events.iter().any(|e| is_relevant(&base, &e.path, repo.as_mut(), &mut ignored_tops));
        if relevant {
            on_change();
        }
    })?;
    debouncer.watcher().watch(&root, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

fn is_relevant(
    root: &Path,
    path: &Path,
    repo: Option<&mut git2::Repository>,
    ignored_tops: &mut HashSet<OsString>,
) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return true };
    let mut comps = rel.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_os_string()),
        _ => None,
    });
    let Some(top) = comps.next() else { return true };
    if top == ".git" {
        let next = comps.next();
        return next.is_some_and(|n| {
            let n = n.to_string_lossy();
            !n.ends_with(".lock") && GIT_SIGNALS.contains(&n.as_ref())
        });
    }
    if ignored_tops.contains(&top) {
        return false;
    }
    let Some(repo) = repo else { return true };
    // Git can't re-include files inside an ignored directory, so remembering
    // ignored top-level directories is safe and makes build storms nearly free.
    let top_path = root.join(&top);
    if top_path.is_dir() && repo.is_path_ignored(Path::new(&top)).unwrap_or(false) {
        ignored_tops.insert(top);
        return false;
    }
    !repo.is_path_ignored(rel).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_noise() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        let mut tops = HashSet::new();
        let mut check = |p: &str| is_relevant(root, &root.join(p), Some(&mut repo), &mut tops);
        assert!(check("src/main.rs"));
        assert!(!check("target/debug/app.exe"));
        assert!(!check("build.log"));
        assert!(check(".git/HEAD"));
        assert!(check(".git/refs/heads/main"));
        assert!(!check(".git/index.lock"));
        assert!(!check(".git/objects/ab/cdef"));
        assert!(tops.contains(&OsString::from("target")));
    }
}
