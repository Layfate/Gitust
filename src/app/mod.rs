//! Application controller. Owns state, runs every blocking or network task on
//! the Tokio runtime, and pushes immutable snapshots to the Slint UI thread.
//!
//! Rules: the UI thread never touches git/network/disk; repository mutations
//! are serialised by `op_lock`; stale async results are dropped via sequence
//! numbers; nothing polls in the background.

use crate::cli::{self, Cancelled};
use crate::git::{self, FileChange};
use crate::github;
use crate::ops::{self, GitFailure, blocking, git_ok};
use crate::store::Store;
use crate::util;
use crate::watch::{self, RepoWatcher};
use crate::{
    AppState, BranchItem, ChangeRow, CommitItem, DiffLine, GhItem, GhRepoItem, GraphPath, MainWindow, PaletteItem,
    RepoItem, Seg,
};
use anyhow::{Result, anyhow};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Notify;

const HISTORY_PAGE: usize = 300;
const GH_STALE_SECS: i64 = 180;

// ───────────────────────────── state ─────────────────────────────

/// One diff row in thread-safe form; becomes a Slint `DiffLine` on the UI thread
/// (Slint models are single-threaded, so the payload can't hold them).
#[derive(Clone, Default)]
struct Row {
    kind: i32,
    old_no: SharedString,
    new_no: SharedString,
    text: SharedString,
    selectable: bool,
    segs: Vec<crate::highlight::Seg>,
}

impl Row {
    fn into_line(self) -> DiffLine {
        DiffLine {
            kind: self.kind,
            old_no: self.old_no,
            new_no: self.new_no,
            text: self.text,
            selectable: self.selectable,
            selected: false,
            segs: if self.segs.is_empty() {
                ModelRc::default()
            } else {
                ModelRc::new(VecModel::from(
                    self.segs
                        .into_iter()
                        .map(|g| Seg { text: g.text.into(), tok: g.tok as i32, emph: g.emph })
                        .collect::<Vec<_>>(),
                ))
            },
        }
    }
}

#[derive(Clone, Default)]
struct DiffPayload {
    lines: Vec<Row>,
    title: String,
    dir: String,
    status: String,
    adds: i32,
    dels: i32,
    files: i32,
    max_chars: i32,
    truncated: bool,
    too_large: String,
    binary: bool,
    // commit detail
    summary: String,
    body: String,
    meta: String,
    /// Line staging key: "staged:fingerprint:path", empty when unsupported.
    select_key: String,
}

impl DiffPayload {
    fn from_view(v: git::DiffView) -> Self {
        let num = |n: Option<u32>| n.map(|n| SharedString::from(n.to_string())).unwrap_or_default();
        let selectable = v.fingerprint.is_some();
        Self {
            lines: v
                .lines
                .into_iter()
                .map(|l| Row {
                    kind: l.kind,
                    old_no: if l.kind == git::kind::FILE { l.tag.into() } else { num(l.old_no) },
                    new_no: num(l.new_no),
                    selectable: selectable && l.pos.is_some() && (l.kind == git::kind::ADD || l.kind == git::kind::DEL),
                    segs: l.segs,
                    text: l.text.into(),
                })
                .collect(),
            adds: v.adds as i32,
            dels: v.dels as i32,
            files: v.files as i32,
            max_chars: v.max_chars as i32,
            truncated: v.truncated,
            too_large: v.too_large.map(util::human_bytes).unwrap_or_default(),
            binary: v.binary,
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
enum PaletteMode {
    #[default]
    All,
    Branches,
}

enum Pending {
    Discard(String),
    DeleteBranch(String),
    Merge(String),
    Checkout { name: String, remote: bool },
    Revert(String),
    Amend,
    Trust,
    TakeSide { path: String, mine: bool },
    DiscardLines { key: String, rows: Vec<usize> },
}

fn select_key(change: &FileChange, fingerprint: u64) -> String {
    format!("{}:{fingerprint:016x}:{}", u8::from(change.staged), change.path)
}

fn parse_select_key(key: &str) -> Option<(bool, u64, &str)> {
    let (staged, rest) = key.split_once(':')?;
    let (fp, path) = rest.split_once(':')?;
    Some((staged == "1", u64::from_str_radix(fp, 16).ok()?, path))
}

/// Stable identity of a repository's "runs programs" findings, so trusting it
/// once doesn't cover settings added later.
fn trust_token(findings: &[String]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    findings.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Default)]
struct State {
    repo: Option<PathBuf>,
    summary: Option<git::Summary>,
    tab: i32,
    selected: Option<FileChange>,
    force_full_diff: bool,
    changes_diff: Option<DiffPayload>,
    commit_diff: Option<DiffPayload>,
    selected_commit: Option<String>,
    commits: Vec<git::CommitInfo>,
    commits_loading: bool,
    commits_exhausted: bool,
    history_gen: u64,
    history_query: String,
    branches: Vec<git::BranchInfo>,
    branch_query: String,
    watcher: Option<RepoWatcher>,
    /// Config/hooks that run programs, until the user trusts this repository.
    untrusted: Vec<String>,
    last_fetch: HashMap<PathBuf, i64>,
    recents: Vec<RepoItem>,
    // GitHub
    gh: Option<octocrab::Octocrab>,
    gh_login: String,
    gh_repos: Vec<github::Repo>,
    gh_local: HashMap<String, String>,
    gh_items: Vec<github::Item>,
    gh_query: String,
    gh_tab: i32,
    gh_this_repo: bool,
    gh_loaded: HashMap<String, i64>,
    gh_seq: u64,
    device_cancel: Option<Arc<Notify>>,
    // dialogs
    pending: Option<Pending>,
    palette_mode: PaletteMode,
    clone_root: PathBuf,
}

pub struct App {
    ui: Weak<MainWindow>,
    rt: tokio::runtime::Handle,
    store: Mutex<Store>,
    state: Mutex<State>,
    op_lock: tokio::sync::Mutex<()>,
    cancel: Mutex<Option<Arc<Notify>>>,
    file_seq: AtomicU64,
    commit_seq: AtomicU64,
    refreshing: AtomicBool,
    refresh_again: AtomicBool,
}

/// Replace a model's rows in place (keeps scroll position) or create it.
fn sync_model<T: Clone + PartialEq + 'static>(current: ModelRc<T>, new: Vec<T>) -> Option<ModelRc<T>> {
    if let Some(vm) = current.as_any().downcast_ref::<VecModel<T>>() {
        let same = vm.row_count() == new.len() && vm.iter().zip(new.iter()).all(|(a, b)| &a == b);
        if !same {
            vm.set_vec(new);
        }
        None
    } else {
        Some(ModelRc::new(VecModel::from(new)))
    }
}

macro_rules! set_model {
    ($st:expr, $get:ident, $set:ident, $v:expr) => {
        if let Some(m) = sync_model($st.$get(), $v) {
            $st.$set(m);
        }
    };
}

fn sh(v: impl Into<SharedString>) -> SharedString {
    v.into()
}

fn is_http_url(u: &str) -> bool {
    u.starts_with("https://") || u.starts_with("http://")
}

/// Extensions that would *run* rather than open — repositories are untrusted.
fn is_executable_like(path: &Path) -> bool {
    const RISKY: &[&str] = &[
        "exe",
        "bat",
        "cmd",
        "com",
        "ps1",
        "psm1",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "msi",
        "msp",
        "lnk",
        "scr",
        "hta",
        "cpl",
        "reg",
        "jar",
        "sh",
        "bash",
        "command",
        "app",
        "url",
        "pif",
        "appref-ms",
        "application",
    ];
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| RISKY.contains(&e.as_str()))
}

fn encode_branch(b: &str) -> String {
    b.split('/').map(util::percent_encode).collect::<Vec<_>>().join("/")
}

impl App {
    pub fn new(ui: &MainWindow, rt: tokio::runtime::Handle, store: Store) -> Arc<Self> {
        let clone_root = store.get("clone_root").map(PathBuf::from).unwrap_or_else(|| {
            dirs::document_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir).join("GitHub")
        });
        Arc::new(Self {
            ui: ui.as_weak(),
            rt,
            store: Mutex::new(store),
            state: Mutex::new(State { clone_root, gh_this_repo: true, ..Default::default() }),
            op_lock: tokio::sync::Mutex::new(()),
            cancel: Mutex::new(None),
            file_seq: AtomicU64::new(0),
            commit_seq: AtomicU64::new(0),
            refreshing: AtomicBool::new(false),
            refresh_again: AtomicBool::new(false),
        })
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn db(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn repo(&self) -> Option<PathBuf> {
        self.st().repo.clone()
    }

    /// Run `f` on the UI thread with the window and its AppState global.
    fn ui(&self, f: impl FnOnce(&MainWindow, &AppState<'_>) + Send + 'static) {
        let w = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                f(&ui, &st);
            }
        });
    }

    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(f);
    }

    fn info(&self, msg: impl Into<String>) {
        let m = msg.into();
        self.ui(move |_, st| {
            st.set_status_text(m.into());
            st.set_status_error(false);
        });
    }

    fn fail(&self, e: anyhow::Error) {
        let (summary, detail) = match e.downcast_ref::<GitFailure>() {
            Some(g) => (g.summary.clone(), g.detail.clone()),
            None => (format!("{e:#}"), format!("{e:?}")),
        };
        crate::log::error(&format!("{summary}\n{detail}"));
        self.ui(move |_, st| {
            st.set_status_text(summary.into());
            st.set_error_detail(detail.into());
            st.set_status_error(true);
        });
    }

    // ───────────────────────── startup / wiring ─────────────────────────

    /// `initial` (from the command line) wins over the last opened repository.
    pub fn restore(self: &Arc<Self>, ui: &MainWindow, initial: Option<PathBuf>) {
        let st = ui.global::<AppState>();
        let (theme, last_repo, w, h, maximized) = {
            let db = self.db();
            (
                db.get("theme").and_then(|v| v.parse().ok()).unwrap_or(0),
                db.get("last_repo"),
                db.get("win_w").and_then(|v| v.parse::<f32>().ok()),
                db.get("win_h").and_then(|v| v.parse::<f32>().ok()),
                db.get("win_max").as_deref() == Some("1"),
            )
        };
        st.set_theme_mode(theme);
        {
            let db = self.db();
            let width = |key: &str| db.get(key).and_then(|v| v.parse::<f32>().ok()).map(|w| w.clamp(260.0, 720.0));
            if let Some(w) = width("changes_width") {
                st.set_changes_width(w);
            }
            if let Some(w) = width("history_width") {
                st.set_history_width(w);
            }
        }
        st.set_version(env!("CARGO_PKG_VERSION").into());
        st.set_clone_root(self.st().clone_root.display().to_string().into());
        st.set_device_flow_available(github::device_client_id().is_some());
        st.set_status_text("Ready".into());
        if let (Some(w), Some(h)) = (w, h)
            && w >= 900.0
            && h >= 560.0
        {
            ui.window().set_size(slint::LogicalSize::new(w, h));
        }
        if maximized {
            ui.window().set_maximized(true);
        }
        ui.global::<crate::T>().set_mono(
            if cfg!(windows) {
                "Consolas"
            } else if cfg!(target_os = "macos") {
                "Menlo"
            } else {
                "DejaVu Sans Mono"
            }
            .into(),
        );

        self.push_recents();
        match initial {
            Some(p) => self.open_repo(p, true),
            None => {
                if let Some(p) = last_repo {
                    self.open_repo(PathBuf::from(p), false);
                }
            }
        }
        self.gh_restore_session();
        let (enabled, last) = {
            let db = self.db();
            (
                db.get("update_check").as_deref() != Some("0"),
                db.get("update_last").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
            )
        };
        st.set_update_check(enabled);
        if enabled && util::now() - last > 86_400 {
            self.check_updates(false);
        }
    }

    /// Asks GitHub for the latest release (at most daily unless `manual`).
    /// Only shows a link; nothing is downloaded or installed.
    fn check_updates(self: &Arc<Self>, manual: bool) {
        let app = self.clone();
        self.spawn(async move {
            match github::latest_release().await {
                Ok(found) => {
                    let _ = app.db().set("update_last", &util::now().to_string());
                    match found.filter(|(tag, _)| util::is_newer_version(tag, env!("CARGO_PKG_VERSION"))) {
                        Some((tag, url)) => {
                            let version = tag.trim_start_matches('v').to_string();
                            crate::log::info(&format!("update available: {version}"));
                            app.ui(move |_, st| {
                                st.set_update_version(version.into());
                                st.set_update_url(url.into());
                            });
                        }
                        None if manual => {
                            app.info(format!("Gitust {} is the latest version", env!("CARGO_PKG_VERSION")))
                        }
                        None => {}
                    }
                }
                Err(e) if manual => app.fail(e.context("Couldn't check for updates")),
                Err(e) => crate::log::info(&format!("update check skipped: {e:#}")),
            }
        });
    }

    pub fn save_window(&self, ui: &MainWindow) {
        let win = ui.window();
        let db = self.db();
        let maximized = win.is_maximized();
        let _ = db.set("win_max", if maximized { "1" } else { "0" });
        if !maximized {
            let size = win.size().to_logical(win.scale_factor());
            let _ = db.set("win_w", &size.width.round().to_string());
            let _ = db.set("win_h", &size.height.round().to_string());
        }
    }

    pub fn bind(self: &Arc<Self>, ui: &MainWindow) {
        let st = ui.global::<AppState>();
        macro_rules! on {
            ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
                let a = self.clone();
                st.$name(move |$($arg: $ty),*| {
                    let $app = &a;
                    $body
                });
            }};
        }

        on!(on_open_repo_dialog, |app| app.pick_repo());
        on!(on_open_repo, |app, p: SharedString| app.open_repo(PathBuf::from(p.as_str()), true));
        on!(on_remove_recent, |app, p: SharedString| {
            let _ = app.db().remove_recent(&p);
            app.push_recents();
        });
        on!(on_toggle_pin, |app, p: SharedString| {
            let _ = app.db().toggle_pin(&p);
            app.push_recents();
        });
        on!(on_refresh, |app| app.schedule_refresh());
        on!(on_select_change, |app, p: SharedString, staged: bool| app.select_change(&p, staged));
        on!(on_stage, |app, p: SharedString| app.stage(vec![p.to_string()], true));
        on!(on_unstage, |app, p: SharedString| app.stage(vec![p.to_string()], false));
        on!(on_stage_all, |app| app.stage_all(true));
        on!(on_unstage_all, |app| app.stage_all(false));
        on!(on_discard, |app, p: SharedString| app.confirm(
            Pending::Discard(p.to_string()),
            "Discard changes?",
            &format!("Your unstaged edits to {p} will be permanently lost. Staged changes are kept."),
            "Discard changes",
            true
        ));
        on!(on_load_full_diff, |app| {
            let sel = {
                let mut s = app.st();
                s.force_full_diff = true;
                s.selected.clone()
            };
            if let Some(c) = sel {
                app.load_file_diff(c, true);
            }
        });
        on!(on_commit, |app| app.commit(false));
        on!(on_amend_toggled, |app, on: bool| app.amend_toggled(on));
        on!(on_stash, |app| app.stash());
        on!(on_stash_pop, |app| app.stash_pop());
        on!(on_select_commit, |app, id: SharedString| app.select_commit(id.to_string()));
        on!(on_revert_commit, |app, id: SharedString| app.confirm(
            Pending::Revert(id.to_string()),
            "Revert this commit?",
            &format!("Creates a new commit that undoes {}. History isn't rewritten.", &id[..7.min(id.len())]),
            "Revert",
            false
        ));
        on!(on_open_commit_on_github, |app, id: SharedString| {
            if let Some(slug) = app.slug() {
                app.open_url(&format!("https://github.com/{slug}/commit/{id}"));
            }
        });
        on!(on_load_more_commits, |app| app.load_history(false));
        on!(on_history_filter, |app, q: SharedString| {
            app.st().history_query = q.to_string();
            app.push_commits();
        });
        on!(on_checkout, |app, name: SharedString, remote: bool| app.request_checkout(name.to_string(), remote));
        on!(on_create_branch, |app, name: SharedString| app.create_branch(name.trim().to_string()));
        on!(on_delete_branch, |app, name: SharedString| app.confirm(
            Pending::DeleteBranch(name.to_string()),
            "Delete branch?",
            &format!("Deletes the local branch {name}. Git refuses if it has unmerged commits, so no work is lost."),
            "Delete",
            true
        ));
        on!(on_merge_branch, |app, name: SharedString| {
            let into = app.st().summary.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
            app.confirm(
                Pending::Merge(name.to_string()),
                "Merge branch?",
                &format!("Merge {name} into {into}. If there are conflicts you can resolve them in the Changes tab."),
                "Merge",
                false,
            )
        });
        on!(on_branch_filter, |app, q: SharedString| {
            app.st().branch_query = q.to_string();
            app.push_branches();
        });
        on!(on_fetch, |app| app.fetch());
        on!(on_pull, |app| app.pull());
        on!(on_push, |app| app.push());
        on!(on_cancel_op, |app| {
            if let Some(n) = app.cancel.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                n.notify_one();
            }
        });
        on!(on_show_clone, |app, url: SharedString| app.show_clone(url.to_string()));
        on!(on_clone_url_edited, |app, url: SharedString| app.suggest_clone_dest(&url));
        on!(on_browse_clone_dest, |app| app.browse_folder(true));
        on!(on_browse_clone_root, |app| app.browse_folder(false));
        on!(on_start_clone, |app| app.start_clone());
        on!(on_gh_sign_in_cli, |app| app.gh_sign_in_cli());
        on!(on_gh_sign_in_token, |app, t: SharedString| app.gh_sign_in_token(t.trim().to_string()));
        on!(on_gh_sign_in_device, |app| app.gh_sign_in_device());
        on!(on_gh_cancel_sign_in, |app| {
            if let Some(n) = app.st().device_cancel.take() {
                n.notify_one();
            }
        });
        on!(on_gh_sign_out, |app| app.gh_sign_out());
        on!(on_gh_switch_account, |app, login: SharedString| {
            if !login.eq_ignore_ascii_case(&app.st().gh_login) {
                app.gh_activate(login.to_string());
            }
        });
        on!(on_gh_add_account, |app| app.ui(|_, st| {
            st.set_gh_error("".into());
            st.set_gh_adding_account(true);
        }));
        on!(on_gh_cancel_add_account, |app| {
            if let Some(n) = app.st().device_cancel.take() {
                n.notify_one();
            }
            app.ui(|_, st| st.set_gh_adding_account(false));
        });
        on!(on_gh_pin_account, |app, on: bool| {
            let (root, login) = {
                let s = app.st();
                (s.repo.clone(), s.gh_login.clone())
            };
            if let Some(root) = root {
                let key = format!("gh_account:{}", root.display());
                let _ = if on { app.db().set(&key, &login) } else { app.db().remove_setting(&key) };
            }
            app.push_account_pin();
        });
        on!(on_gh_refresh, |app| app.gh_load(true));
        on!(on_gh_tab_changed, |app, t: i32| {
            app.st().gh_tab = t;
            app.push_gh_items();
            app.gh_load(false);
        });
        on!(on_gh_filter, |app, q: SharedString| {
            app.st().gh_query = q.to_string();
            app.push_gh_repos();
            app.push_gh_items();
        });
        on!(on_gh_scope_changed, |app, this_repo: bool| {
            app.st().gh_this_repo = this_repo;
            app.gh_load(false);
        });
        on!(on_open_url, |app, u: SharedString| app.open_url(&u));
        on!(on_open_in_explorer, |app| {
            if let Some(r) = app.repo() {
                let _ = open::that_detached(r);
            }
        });
        on!(on_open_terminal, |app| app.open_terminal());
        on!(on_open_log, |_app| {
            if let Some(p) = crate::log::path() {
                reveal(p);
            }
        });
        on!(on_open_file, |app, p: SharedString| app.open_file(&p));
        on!(on_open_on_github, |app| {
            let (slug, branch) = {
                let s = app.st();
                (app_slug(&s), s.summary.as_ref().filter(|x| !x.detached && !x.unborn).map(|x| x.branch.clone()))
            };
            if let Some(slug) = slug {
                let url = match branch {
                    Some(b) => format!("https://github.com/{slug}/tree/{}", encode_branch(&b)),
                    None => format!("https://github.com/{slug}"),
                };
                app.open_url(&url);
            }
        });
        on!(on_create_pr, |app| app.create_pr());
        on!(on_palette_query, |app, q: SharedString| app.palette_query(&q));
        on!(on_palette_run, |app, id: SharedString| app.palette_run(&id));
        on!(on_confirm_accept, |app| app.confirm_accept());
        on!(on_set_theme, |app, m: i32| {
            let _ = app.db().set("theme", &m.to_string());
            app.ui(move |_, st| st.set_theme_mode(m));
        });
        on!(on_tab_changed, |app, t: i32| app.tab_changed(t));
        on!(on_show_dialog, |app, name: SharedString| app.show_dialog(&name));
        on!(on_toggle_diff_line, |app, row: i32, shift: bool| app.toggle_diff_line(row, shift));
        on!(on_stage_hunk, |app, row: i32| app.stage_hunk(row));
        on!(on_stage_selected_lines, |app| app.stage_selected_lines());
        on!(on_discard_selected_lines, |app| {
            let (key, rows) = app.picked_rows();
            app.confirm_discard_rows(key, rows);
        });
        on!(on_discard_hunk, |app, row: i32| {
            let (key, rows) = app.hunk_rows(row);
            app.confirm_discard_rows(key, rows);
        });
        on!(on_clear_line_selection, |app| app.with_diff_model(|st, model| {
            for i in 0..model.row_count() {
                if let Some(mut r) = model.row_data(i).filter(|r| r.selected) {
                    r.selected = false;
                    model.set_row_data(i, r);
                }
            }
            st.set_diff_selected_count(0);
            st.set_diff_anchor(-1);
        }));
        on!(on_take_side, |app, path: SharedString, mine: bool| {
            let rebasing = app.st().summary.as_ref().is_some_and(|s| s.state == "Rebasing");
            let whose = match (mine, rebasing) {
                (true, false) => "your version (the branch you're on)",
                (true, true) => "your version (the commit being rebased)",
                (false, false) => "the incoming version",
                (false, true) => "the version you're rebasing onto",
            };
            app.confirm(
                Pending::TakeSide { path: path.to_string(), mine },
                if mine { "Use your version?" } else { "Use their version?" },
                &format!(
                    "{path} becomes {whose} and is marked resolved. Edits you made to it by hand are lost. If that side deleted the file, it's deleted."
                ),
                if mine { "Use mine" } else { "Use theirs" },
                true,
            )
        });
        on!(on_pull_with, |app, mode: i32| {
            app.ui(|_, st| st.set_dialog("".into()));
            app.pull_mode(if mode == 2 { ops::PullMode::Rebase } else { ops::PullMode::Merge });
        });
        on!(on_review_trust, |app| {
            let list = app.st().untrusted.clone();
            if list.is_empty() {
                return;
            }
            let items: String = list.iter().map(|l| format!("\n  • {l}")).collect();
            app.confirm(
                Pending::Trust,
                "Trust this repository?",
                &format!(
                    "Its own Git settings make Git run these programs when you stage, commit, pull or push:{items}\n\nRepositories you cloned yourself rarely have these; ones you were sent as a folder or zip can. Only trust it if you know where it came from. Gitust asks again if the list changes."
                ),
                "Trust repository",
                true,
            )
        });
        {
            let a = self.clone();
            st.on_step_change(move |delta| a.step_change(delta));
        }
        {
            let a = self.clone();
            st.on_step_commit(move |delta| a.step_commit(delta));
        }
        on!(on_check_updates, |app| app.check_updates(true));
        on!(on_set_update_check, |app, on: bool| {
            let _ = app.db().set("update_check", if on { "1" } else { "0" });
            app.ui(move |_, st| st.set_update_check(on));
        });
        on!(on_panel_resized, |app| {
            if let Some(ui) = app.ui.upgrade() {
                let st = ui.global::<AppState>();
                let db = app.db();
                let _ = db.set("changes_width", &st.get_changes_width().round().to_string());
                let _ = db.set("history_width", &st.get_history_width().round().to_string());
            }
        });

        // Persist geometry when the window closes.
        let a = self.clone();
        let weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                a.save_window(&ui);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    fn slug(&self) -> Option<String> {
        app_slug(&self.st())
    }

    // ───────────────────────── dialogs & navigation ─────────────────────────

    fn show_dialog(self: &Arc<Self>, name: &str) {
        match name {
            "palette" | "switch-branch" => {
                if name == "switch-branch" && self.repo().is_none() {
                    return;
                }
                self.st().palette_mode = if name == "palette" { PaletteMode::All } else { PaletteMode::Branches };
                self.palette_query("");
                self.ui(|_, st| {
                    st.set_palette_index(0);
                    st.set_dialog("palette".into());
                });
            }
            "new-branch" => {
                if self.repo().is_none() {
                    return;
                }
                self.ui(|_, st| {
                    st.set_new_branch_name("".into());
                    st.set_dialog("new-branch".into());
                });
            }
            other => {
                let other = other.to_string();
                self.ui(move |_, st| st.set_dialog(other.into()));
            }
        }
    }

    fn confirm(self: &Arc<Self>, pending: Pending, title: &str, text: &str, button: &str, danger: bool) {
        self.st().pending = Some(pending);
        let (title, text, button) = (title.to_string(), text.to_string(), button.to_string());
        self.ui(move |_, st| {
            st.set_confirm_title(title.into());
            st.set_confirm_text(text.into());
            st.set_confirm_button(button.into());
            st.set_confirm_danger(danger);
            st.set_dialog("confirm".into());
        });
    }

    fn confirm_accept(self: &Arc<Self>) {
        let pending = self.st().pending.take();
        self.ui(|_, st| st.set_dialog("".into()));
        match pending {
            Some(Pending::Discard(p)) => self.discard(p),
            Some(Pending::DeleteBranch(b)) => {
                self.simple_op("Deleting branch", vec!["branch", "-d", &b], "Branch deleted")
            }
            Some(Pending::Merge(b)) => self.simple_op("Merging", vec!["merge", "--no-edit", &b], "Merged"),
            Some(Pending::Checkout { name, remote }) => self.checkout(name, remote),
            Some(Pending::Revert(id)) => {
                self.simple_op("Reverting", vec!["revert", "--no-edit", &id], "Commit reverted")
            }
            Some(Pending::Amend) => self.commit(true),
            Some(Pending::Trust) => {
                let (root, list) = {
                    let mut s = self.st();
                    (s.repo.clone(), std::mem::take(&mut s.untrusted))
                };
                if let Some(root) = root {
                    let _ = self.db().set(&format!("trust:{}", root.display()), &trust_token(&list));
                }
                self.ui(|_, st| st.set_untrusted_summary("".into()));
                self.info("Repository trusted");
            }
            Some(Pending::DiscardLines { key, rows }) => self.discard_rows(key, rows),
            Some(Pending::TakeSide { path, mine }) => {
                let rebasing = self.st().summary.as_ref().is_some_and(|s| s.state == "Rebasing");
                self.op("Resolving", true, true, move |_, root, opts| async move {
                    let root = root.ok_or_else(|| anyhow!("No repository"))?;
                    ops::take_side(&root, &path, mine, rebasing, opts).await?;
                    Ok(Some(format!("Resolved {path} with {} version", if mine { "your" } else { "their" })))
                });
            }
            None => {}
        }
    }

    fn tab_changed(self: &Arc<Self>, t: i32) {
        let payload = {
            let mut s = self.st();
            s.tab = t;
            match t {
                0 => Some(s.changes_diff.clone().unwrap_or_default()),
                1 => Some(s.commit_diff.clone().unwrap_or_default()),
                _ => None,
            }
        };
        if let Some(p) = payload {
            self.push_diff(p, false);
        }
        if t == 3 {
            self.gh_load(false);
        }
    }
}

// ───────────────────────────── free helpers ─────────────────────────────

fn app_slug(s: &State) -> Option<String> {
    s.summary.as_ref().and_then(|x| x.github_slug.clone())
}

fn split_path(p: &str) -> (String, String) {
    match p.rsplit_once('/') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (p.to_string(), String::new()),
    }
}

// Feature modules: each adds methods to `App`. Declared after the macros above
// so `set_model!` is in scope there.
mod actions;
mod changes;
mod history;
mod hub;
mod palette;
mod remote;
mod repo;

#[allow(unused_imports)]
use self::{changes::*, history::*, palette::*, remote::*, repo::*};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_guard() {
        assert!(is_executable_like(Path::new("x/run.BAT")));
        assert!(is_executable_like(Path::new("install.ps1")));
        assert!(!is_executable_like(Path::new("README.md")));
        assert!(!is_executable_like(Path::new("Makefile")));
    }

    #[test]
    fn branch_url_encoding() {
        assert_eq!(encode_branch("feature/a b#1"), "feature/a%20b%231");
    }

    #[test]
    fn palette_ranks_and_filters() {
        let st = State::default();
        let all = build_palette(&st, "");
        assert!(all.iter().any(|i| i.id == "cmd:open"));
        assert!(!all.iter().any(|i| i.id == "cmd:fetch"), "repo commands hidden without a repo");
        let clone = build_palette(&st, "clone");
        assert_eq!(clone[0].id, "cmd:clone");
    }

    #[test]
    fn rows_group_staged_first() {
        let sum = git::Summary {
            staged: vec![FileChange { path: "src/a.rs".into(), old_path: None, status: 'M', staged: true }],
            unstaged: vec![FileChange { path: "b.txt".into(), old_path: None, status: '?', staged: false }],
            ..Default::default()
        };
        let rows = change_rows(&sum);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].header && rows[0].staged);
        assert_eq!(rows[1].name, "a.rs");
        assert_eq!(rows[1].dir, "src");
        assert!(rows[2].header && !rows[2].staged);
    }
}
