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
    AppState, BranchItem, ChangeRow, CommitItem, DiffLine, GhItem, GhRepoItem, MainWindow, PaletteItem, RepoItem,
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

#[derive(Clone, Default)]
struct DiffPayload {
    lines: Vec<DiffLine>,
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
                .map(|l| DiffLine {
                    kind: l.kind,
                    old_no: if l.kind == git::kind::FILE { l.tag.into() } else { num(l.old_no) },
                    new_no: num(l.new_no),
                    selectable: selectable && l.pos.is_some() && (l.kind == git::kind::ADD || l.kind == git::kind::DEL),
                    selected: false,
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

    // ───────────────────────── repositories ─────────────────────────

    fn pick_repo(self: &Arc<Self>) {
        let app = self.clone();
        self.spawn(async move {
            if let Some(dir) = rfd::AsyncFileDialog::new().set_title("Open a Git repository").pick_folder().await {
                app.open_repo(dir.path().to_path_buf(), true);
            }
        });
    }

    fn push_recents(self: &Arc<Self>) {
        let app = self.clone();
        self.spawn(async move {
            let current = app.repo().map(|p| p.display().to_string());
            let recents = app.db().recents().unwrap_or_default();
            let items = blocking(move || {
                Ok(recents
                    .into_iter()
                    .map(|r| RepoItem {
                        missing: !Path::new(&r.path).exists(),
                        current: current.as_deref() == Some(r.path.as_str()),
                        name: r.name.into(),
                        path: r.path.into(),
                        pinned: r.pinned,
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap_or_default();
            app.st().recents = items.clone();
            app.ui(move |_, st| set_model!(st, get_recent, set_recent, items));
        });
    }

    pub fn open_repo(self: &Arc<Self>, path: PathBuf, report: bool) {
        let app = self.clone();
        self.spawn(async move {
            app.ui(|_, st| st.set_repo_loading(true));
            let p = path.clone();
            let res = blocking(move || read_repo(&p)).await;
            app.ui(|_, st| st.set_repo_loading(false));
            let (sum, findings) = match res {
                Ok(s) => s,
                Err(e) => {
                    if report {
                        app.fail(e);
                    }
                    return;
                }
            };
            let root = sum.root.clone();
            let root_str = root.display().to_string();
            {
                let db = app.db();
                let _ = db.touch_recent(&root_str, &sum.name);
                let _ = db.set("last_repo", &root_str);
            }
            let weak = Arc::downgrade(&app);
            let watcher = watch::watch(root.clone(), move || {
                if let Some(a) = weak.upgrade() {
                    a.schedule_refresh();
                }
            })
            .ok();
            {
                let mut s = app.st();
                s.repo = Some(root.clone());
                s.summary = None;
                s.selected = None;
                s.force_full_diff = false;
                s.changes_diff = None;
                s.commit_diff = None;
                s.selected_commit = None;
                s.commits.clear();
                s.commits_exhausted = false;
                s.branches.clear();
                s.history_query.clear();
                s.branch_query.clear();
                s.watcher = watcher; // drops the previous repo's watcher
                if s.tab == 3 && !s.gh_this_repo {
                    s.gh_this_repo = true;
                }
            }
            let name = sum.name.clone();
            app.ui(move |_, st| {
                st.set_has_repo(true);
                st.set_selected_path("".into());
                st.set_selected_commit("".into());
                st.set_commit_summary("".into());
                st.set_commit_body("".into());
                st.set_amend(false);
                st.set_history_query("".into());
                st.set_branch_query("".into());
                if st.get_tab() == 3 {
                    // stay on GitHub
                } else {
                    st.set_tab(0);
                }
                st.set_status_text(format!("Opened {name}").into());
                st.set_status_error(false);
            });
            app.push_diff(DiffPayload::default(), false);
            app.apply_trust(&root, findings);
            app.apply_summary(sum, true);
            app.push_recents();
            app.load_history(true);
            app.load_branches();
            if app.st().gh.is_some() {
                app.st().gh_loaded.retain(|k, _| k == "repos");
            }
        });
    }

    pub fn schedule_refresh(self: &Arc<Self>) {
        self.refresh_again.store(true, Ordering::SeqCst);
        if self.refreshing.swap(true, Ordering::SeqCst) {
            return; // the running loop will pick it up
        }
        let app = self.clone();
        self.spawn(async move {
            loop {
                app.refresh_again.store(false, Ordering::SeqCst);
                app.refresh_now().await;
                if app.refresh_again.load(Ordering::SeqCst) {
                    continue;
                }
                app.refreshing.store(false, Ordering::SeqCst);
                if app.refresh_again.load(Ordering::SeqCst) && !app.refreshing.swap(true, Ordering::SeqCst) {
                    continue;
                }
                break;
            }
        });
    }

    async fn refresh_now(self: &Arc<Self>) {
        let Some(root) = self.repo() else { return };
        let r = root.clone();
        match blocking(move || read_repo(&r)).await {
            Ok((sum, findings)) if self.repo().as_ref() == Some(&root) => {
                self.apply_trust(&root, findings);
                self.apply_summary(sum, false)
            }
            Ok(_) => {}
            Err(e) => self.fail(e),
        }
    }

    /// Records which programs this repository would make Git run, unless the
    /// user already trusted exactly this set.
    fn apply_trust(self: &Arc<Self>, root: &Path, findings: Vec<String>) {
        let trusted = findings.is_empty()
            || self.db().get(&format!("trust:{}", root.display())).as_deref() == Some(trust_token(&findings).as_str());
        let list = if trusted { Vec::new() } else { findings };
        let summary = {
            let mut s = self.st();
            if s.untrusted == list {
                return;
            }
            s.untrusted = list.clone();
            let mut names: Vec<String> =
                list.iter().map(|l| l.split(" = ").next().unwrap_or(l).to_string()).take(4).collect();
            if list.len() > 4 {
                names.push(format!("+{} more", list.len() - 4));
            }
            names.join(", ")
        };
        self.ui(move |_, st| st.set_untrusted_summary(summary.into()));
    }

    fn apply_summary(self: &Arc<Self>, sum: git::Summary, opened: bool) {
        let (head_changed, reselect, last_fetch) = {
            let mut s = self.st();
            let prev = s.summary.as_ref().map(|p| (p.head.clone(), p.branch.clone(), p.stash_count));
            let head_changed =
                !opened && prev.is_some() && prev != Some((sum.head.clone(), sum.branch.clone(), sum.stash_count));
            let reselect = s.selected.as_ref().and_then(|sel| {
                sum.staged
                    .iter()
                    .chain(sum.unstaged.iter())
                    .find(|c| c.path == sel.path && c.staged == sel.staged)
                    .cloned()
            });
            if reselect.is_none() {
                s.selected = None;
                s.changes_diff = None;
            } else {
                s.selected = reselect.clone();
            }
            let lf = s.last_fetch.get(&sum.root).copied();
            s.summary = Some(sum.clone());
            (head_changed, reselect, lf)
        };

        let rows = change_rows(&sum);
        let now = util::now();
        self.ui(move |_, st| {
            st.set_repo_name(sum.name.clone().into());
            st.set_repo_path(sum.root.display().to_string().into());
            st.set_branch(sum.branch.clone().into());
            st.set_detached(sum.detached);
            st.set_unborn(sum.unborn);
            st.set_has_upstream(sum.upstream.is_some());
            st.set_upstream(sum.upstream.clone().unwrap_or_default().into());
            st.set_remote(sum.remote.clone().unwrap_or_default().into());
            st.set_ahead(sum.ahead as i32);
            st.set_behind(sum.behind as i32);
            st.set_repo_state(sum.state.clone().into());
            st.set_github_slug(sum.github_slug.clone().unwrap_or_default().into());
            st.set_stash_count(sum.stash_count as i32);
            st.set_staged_count(sum.staged.len() as i32);
            st.set_unstaged_count(sum.unstaged.len() as i32);
            st.set_conflicted_count(sum.conflicted as i32);
            st.set_last_fetched(last_fetch.map(|t| util::relative_time(t, now)).unwrap_or_default().into());
            set_model!(st, get_changes, set_changes, rows);
        });

        match reselect {
            Some(c) => self.load_file_diff(c, false),
            None => {
                self.ui(|_, st| st.set_selected_path("".into()));
                if self.st().tab == 0 {
                    self.push_diff(DiffPayload::default(), false);
                }
            }
        }
        if head_changed {
            self.load_history(true);
            self.load_branches();
        }
    }

    // ───────────────────────── diffs ─────────────────────────

    fn push_diff(&self, p: DiffPayload, loading: bool) {
        self.ui(move |_, st| {
            st.set_diff_loading(loading);
            st.set_diff_title(p.title.into());
            st.set_diff_dir(p.dir.into());
            st.set_diff_status(p.status.into());
            st.set_diff_adds(p.adds);
            st.set_diff_dels(p.dels);
            st.set_diff_files(p.files);
            st.set_diff_max_chars(p.max_chars);
            st.set_diff_truncated(p.truncated);
            st.set_diff_too_large(p.too_large.into());
            st.set_diff_binary(p.binary);
            st.set_detail_summary(p.summary.into());
            st.set_detail_body(p.body.into());
            st.set_detail_meta(p.meta.into());
            // The same key means the very same diff (a refresh after unrelated
            // changes): keep the rows, so the scroll position and picked lines stay.
            let same = !p.select_key.is_empty()
                && st.get_diff_select_key() == p.select_key.as_str()
                && st.get_diff().row_count() == p.lines.len();
            if !same {
                st.set_diff_select_key(p.select_key.as_str().into());
                st.set_diff_can_select(!p.select_key.is_empty());
                st.set_diff_selected_count(0);
                st.set_diff_anchor(-1);
                // Fresh model per diff: scroll resets to the top, which is what you want for a new file.
                st.set_diff(ModelRc::new(VecModel::from(p.lines)));
            }
        });
    }

    /// Runs `f` on the UI thread's current diff model. Call from the UI thread.
    fn with_diff_model(&self, f: impl FnOnce(&AppState<'_>, &ModelRc<DiffLine>)) {
        if let Some(ui) = self.ui.upgrade() {
            let st = ui.global::<AppState>();
            let model = st.get_diff();
            f(&st, &model);
        }
    }

    /// Click on a +/- row: toggle it; with Shift, set the range from the last
    /// clicked row to the state that row has.
    fn toggle_diff_line(&self, row: i32, shift: bool) {
        self.with_diff_model(|st, model| {
            let Some(clicked) = usize::try_from(row).ok().and_then(|i| model.row_data(i)) else { return };
            if !clicked.selectable {
                return;
            }
            let idx = row as usize;
            let anchor = usize::try_from(st.get_diff_anchor()).ok().filter(|&a| a < model.row_count());
            let (range, value) = match anchor {
                Some(a) if shift => (a.min(idx)..=a.max(idx), model.row_data(a).is_some_and(|r| r.selected)),
                _ => {
                    st.set_diff_anchor(row);
                    (idx..=idx, !clicked.selected)
                }
            };
            for i in range {
                if let Some(mut r) = model.row_data(i).filter(|r| r.selectable && r.selected != value) {
                    r.selected = value;
                    model.set_row_data(i, r);
                }
            }
            let count = model.iter().filter(|r| r.selected).count();
            st.set_diff_selected_count(count as i32);
        });
    }

    /// The selectable rows of the hunk whose header is at `header_row`, with the diff key.
    fn hunk_rows(&self, header_row: i32) -> (String, Vec<usize>) {
        let mut rows = Vec::new();
        let mut key = String::new();
        self.with_diff_model(|st, model| {
            key = st.get_diff_select_key().to_string();
            let start = usize::try_from(header_row).unwrap_or(usize::MAX).saturating_add(1);
            for i in start..model.row_count() {
                let Some(r) = model.row_data(i) else { break };
                if r.kind == git::kind::HUNK || r.kind == git::kind::FILE {
                    break;
                }
                if r.selectable {
                    rows.push(i);
                }
            }
        });
        (key, rows)
    }

    /// The ticked rows, with the diff key.
    fn picked_rows(&self) -> (String, Vec<usize>) {
        let mut rows = Vec::new();
        let mut key = String::new();
        self.with_diff_model(|st, model| {
            key = st.get_diff_select_key().to_string();
            rows = model.iter().enumerate().filter(|(_, r)| r.selected).map(|(i, _)| i).collect();
        });
        (key, rows)
    }

    fn stage_hunk(self: &Arc<Self>, header_row: i32) {
        let (key, rows) = self.hunk_rows(header_row);
        self.stage_rows(key, rows);
    }

    fn stage_selected_lines(self: &Arc<Self>) {
        let (key, rows) = self.picked_rows();
        self.stage_rows(key, rows);
    }

    /// Asks first: discarded lines can't be recovered.
    fn confirm_discard_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
        let Some((false, _, path)) = parse_select_key(&key) else { return };
        if rows.is_empty() {
            return;
        }
        let n = rows.len();
        let text = format!(
            "The {n} selected line{} in {path} go back to their staged version; other edits in the file stay. This can't be undone.",
            if n == 1 { "" } else { "s" }
        );
        self.confirm(Pending::DiscardLines { key, rows }, "Discard selected lines?", &text, "Discard lines", true);
    }

    fn discard_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
        let Some((false, fingerprint, path)) = parse_select_key(&key) else { return };
        let change = self.st().summary.as_ref().and_then(|x| x.unstaged.iter().find(|c| c.path == path).cloned());
        let Some(change) = change else {
            self.info(format!("{path} changed; select the lines again"));
            return;
        };
        self.op("Discarding lines", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            let n = ops::discard_lines(&root, change, fingerprint, rows, opts).await?;
            Ok(Some(format!("Discarded {n} line{}", if n == 1 { "" } else { "s" })))
        });
    }

    /// Stages/unstages diff rows of the file named by `key` (see `select_key`).
    fn stage_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
        let Some((staged, fingerprint, path)) = parse_select_key(&key) else { return };
        if rows.is_empty() {
            return;
        }
        let (change, unborn) = {
            let s = self.st();
            let sum = s.summary.as_ref();
            let change = sum.and_then(|x| {
                let list = if staged { &x.staged } else { &x.unstaged };
                list.iter().find(|c| c.path == path).cloned()
            });
            (change, sum.is_some_and(|x| x.unborn))
        };
        let Some(change) = change else {
            self.info(format!("{path} changed; select the lines again"));
            return;
        };
        self.op("Staging lines", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            let n = ops::stage_lines(&root, change, fingerprint, rows, unborn, opts).await?;
            let verb = if staged { "Unstaged" } else { "Staged" };
            Ok(Some(format!("{verb} {n} line{}", if n == 1 { "" } else { "s" })))
        });
    }

    /// Up/Down in the file list. Returns the row index to scroll to.
    fn step_change(self: &Arc<Self>, delta: i32) -> i32 {
        let (next, rows) = {
            let s = self.st();
            let Some(sum) = s.summary.as_ref() else { return -1 };
            let files: Vec<&FileChange> = sum.staged.iter().chain(sum.unstaged.iter()).collect();
            if files.is_empty() {
                return -1;
            }
            let cur = s
                .selected
                .as_ref()
                .and_then(|sel| files.iter().position(|c| c.path == sel.path && c.staged == sel.staged));
            let i = match cur {
                Some(i) => (i as i32 + delta).clamp(0, files.len() as i32 - 1) as usize,
                None => 0,
            };
            (files[i].clone(), change_rows(sum))
        };
        self.select_change(&next.path, next.staged);
        rows.iter()
            .position(|r| !r.header && r.path == next.path.as_str() && r.staged == next.staged)
            .map_or(-1, |i| i as i32)
    }

    /// Up/Down in the (filtered) history list. Returns the row index to scroll to.
    fn step_commit(self: &Arc<Self>, delta: i32) -> i32 {
        let next = {
            let s = self.st();
            let visible: Vec<&git::CommitInfo> =
                s.commits.iter().filter(|c| commit_matches(c, &s.history_query)).collect();
            if visible.is_empty() {
                return -1;
            }
            let cur = s.selected_commit.as_ref().and_then(|id| visible.iter().position(|c| &c.id == id));
            let i = match cur {
                Some(i) => (i as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize,
                None => 0,
            };
            (i, visible[i].id.clone(), i + 1 == visible.len())
        };
        let (i, id, last) = next;
        self.select_commit(id);
        if last {
            self.load_history(false);
        }
        i as i32
    }

    fn select_change(self: &Arc<Self>, path: &str, staged: bool) {
        let change = {
            let mut s = self.st();
            let found = s.summary.as_ref().and_then(|sum| {
                sum.staged.iter().chain(sum.unstaged.iter()).find(|c| c.path == path && c.staged == staged).cloned()
            });
            if found.is_some() {
                s.selected = found.clone();
                s.force_full_diff = false;
            }
            found
        };
        if let Some(c) = change {
            let (p, st) = (c.path.clone(), c.staged);
            self.ui(move |_, ui| {
                ui.set_selected_path(p.into());
                ui.set_selected_staged(st);
            });
            self.load_file_diff(c, true);
        }
    }

    fn load_file_diff(self: &Arc<Self>, change: FileChange, spinner: bool) {
        let Some(root) = self.repo() else { return };
        let seq = self.file_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let force = self.st().force_full_diff;
        let (name, dir) = split_path(&change.path);
        let header = DiffPayload {
            title: name,
            dir: match &change.old_path {
                Some(old) => format!("renamed from {old}"),
                None => dir,
            },
            status: change.status.to_string(),
            ..Default::default()
        };
        if spinner && self.st().tab == 0 {
            self.push_diff(header.clone(), true);
        }
        let app = self.clone();
        self.spawn(async move {
            let c = change.clone();
            let res = blocking(move || git::file_diff(&git::open(&root)?, &c, force)).await;
            if app.file_seq.load(Ordering::SeqCst) != seq {
                return; // a newer selection superseded this one
            }
            match res {
                Ok(view) => {
                    let key = view.fingerprint.map(|fp| select_key(&change, fp)).unwrap_or_default();
                    let mut p = DiffPayload::from_view(view);
                    p.select_key = key;
                    p.title = header.title;
                    p.dir = header.dir;
                    p.status = header.status;
                    let show = {
                        let mut s = app.st();
                        s.changes_diff = Some(p.clone());
                        s.tab == 0
                    };
                    if show {
                        app.push_diff(p, false);
                    }
                }
                Err(e) => {
                    app.push_diff(header, false);
                    app.fail(e);
                }
            }
        });
    }

    fn select_commit(self: &Arc<Self>, id: String) {
        let Some(root) = self.repo() else { return };
        self.st().selected_commit = Some(id.clone());
        let seq = self.commit_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let idc = id.clone();
        self.ui(move |_, st| st.set_selected_commit(idc.into()));
        let app = self.clone();
        self.spawn(async move {
            let res = blocking(move || git::commit_detail(&git::open(&root)?, &id)).await;
            if app.commit_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            match res {
                Ok(d) => {
                    let parents = if d.parents.len() > 1 {
                        format!(" · merge of {} (diff vs first parent)", d.parents.join(" + "))
                    } else {
                        String::new()
                    };
                    let mut p = DiffPayload::from_view(d.diff);
                    p.summary = d.summary;
                    p.body = d.body;
                    p.meta = format!(
                        "{} <{}> · {} · {}{}",
                        d.author,
                        d.email,
                        util::datetime(d.time, d.offset_minutes),
                        &d.id[..10.min(d.id.len())],
                        parents
                    );
                    let show = {
                        let mut s = app.st();
                        s.commit_diff = Some(p.clone());
                        s.tab == 1
                    };
                    if show {
                        app.push_diff(p, false);
                    }
                }
                Err(e) => app.fail(e),
            }
        });
    }

    // ───────────────────────── history & branches ─────────────────────────

    fn load_history(self: &Arc<Self>, reset: bool) {
        let Some(root) = self.repo() else { return };
        let (gen_, skip) = {
            let mut s = self.st();
            if reset {
                s.history_gen += 1;
                s.commits_exhausted = false;
                s.commits_loading = true;
                (s.history_gen, 0)
            } else {
                if s.commits_loading || s.commits_exhausted {
                    return;
                }
                s.commits_loading = true;
                (s.history_gen, s.commits.len())
            }
        };
        self.ui(|_, st| st.set_commits_loading(true));
        let app = self.clone();
        self.spawn(async move {
            let res = blocking(move || git::history(&git::open(&root)?, skip, HISTORY_PAGE)).await;
            {
                let mut s = app.st();
                if s.history_gen != gen_ {
                    return;
                }
                s.commits_loading = false;
                match &res {
                    Ok(page) => {
                        s.commits_exhausted = page.len() < HISTORY_PAGE;
                        if skip == 0 {
                            s.commits.clear();
                        }
                    }
                    Err(_) => s.commits_exhausted = true,
                }
                if let Ok(page) = &res {
                    s.commits.extend(page.iter().cloned());
                }
                let exhausted = s.commits_exhausted;
                app.ui(move |_, st| {
                    st.set_commits_loading(false);
                    st.set_commits_exhausted(exhausted);
                });
            }
            match res {
                Ok(_) => app.push_commits(),
                Err(e) => app.fail(e),
            }
        });
    }

    fn push_commits(&self) {
        let items: Vec<CommitItem> = {
            let s = self.st();
            let now = util::now();
            s.commits
                .iter()
                .filter(|c| commit_matches(c, &s.history_query))
                .map(|c| CommitItem {
                    id: sh(c.id.clone()),
                    short_id: sh(c.id[..7.min(c.id.len())].to_string()),
                    summary: sh(c.summary.clone()),
                    author: sh(c.author.clone()),
                    when: sh(util::relative_time(c.time, now)),
                    refs: sh(match c.refs.len() {
                        0 => String::new(),
                        1 | 2 => c.refs.join(", "),
                        n => format!("{}, {} +{}", c.refs[0], c.refs[1], n - 2),
                    }),
                    unpushed: c.unpushed,
                    merge: c.merge,
                })
                .collect()
        };
        self.ui(move |_, st| set_model!(st, get_commits, set_commits, items));
    }

    fn load_branches(self: &Arc<Self>) {
        let Some(root) = self.repo() else { return };
        let app = self.clone();
        self.spawn(async move {
            match blocking(move || git::branches(&git::open(&root)?)).await {
                Ok(b) => {
                    app.st().branches = b;
                    app.push_branches();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    fn push_branches(&self) {
        let items: Vec<BranchItem> = {
            let s = self.st();
            let q = s.branch_query.trim().to_lowercase();
            let now = util::now();
            s.branches
                .iter()
                .filter(|b| q.is_empty() || b.name.to_lowercase().contains(&q))
                .map(|b| BranchItem {
                    name: sh(b.name.clone()),
                    is_head: b.is_head,
                    is_remote: b.is_remote,
                    upstream: sh(b.upstream.clone().unwrap_or_default()),
                    ahead: b.ahead as i32,
                    behind: b.behind as i32,
                    when: sh(if b.time > 0 { util::relative_time(b.time, now) } else { String::new() }),
                    summary: sh(b.summary.clone()),
                })
                .collect()
        };
        self.ui(move |_, st| set_model!(st, get_branches, set_branches, items));
    }

    // ───────────────────────── git operations ─────────────────────────

    /// Runs a repository mutation. `quiet` ops (stage/unstage) queue behind the lock
    /// without the busy bar; loud ops refuse to overlap and show progress + cancel.
    fn op<F, Fut>(self: &Arc<Self>, label: &'static str, quiet: bool, needs_repo: bool, f: F)
    where
        F: FnOnce(Arc<Self>, Option<PathBuf>, cli::Opts) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<String>>> + Send + 'static,
    {
        let app = self.clone();
        self.spawn(async move {
            let root = app.repo();
            if needs_repo && root.is_none() {
                return;
            }
            // Git would run the repository's own programs (hooks, fsmonitor, filters…).
            if needs_repo && !app.st().untrusted.is_empty() {
                app.info("This repository runs programs through its Git settings. Review it (red banner) before Gitust runs Git here.");
                return;
            }
            let _guard = if quiet {
                app.op_lock.lock().await
            } else {
                match app.op_lock.try_lock() {
                    Ok(g) => g,
                    Err(_) => {
                        app.info("Another Git operation is still running");
                        return;
                    }
                }
            };
            let cancel = Arc::new(Notify::new());
            let mut opts = cli::Opts::default();
            if !quiet {
                *app.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
                let a = app.clone();
                opts.cancel = Some(cancel);
                opts.progress = Some(Arc::new(move |line: &str| a.progress(line)));
                app.ui(move |_, st| {
                    st.set_busy(true);
                    st.set_busy_text(format!("{label}…").into());
                    st.set_busy_progress(-1.0);
                    st.set_busy_cancellable(true);
                });
            }
            let result = f(app.clone(), root, opts).await;
            if !quiet {
                *app.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                app.ui(|_, st| {
                    st.set_busy(false);
                    st.set_busy_progress(-1.0);
                });
            }
            match result {
                Ok(Some(msg)) => app.info(msg),
                Ok(None) => {}
                Err(e) if e.is::<Cancelled>() => app.info(format!("{label} cancelled")),
                Err(e) => app.fail(e),
            }
            drop(_guard);
            app.schedule_refresh();
        });
    }

    fn progress(&self, line: &str) {
        let pct = cli::parse_percent(line);
        let text = line.trim_start_matches("remote: ").to_string();
        if text.is_empty() {
            return;
        }
        self.ui(move |_, st| {
            st.set_busy_text(text.into());
            st.set_busy_progress(pct.unwrap_or(-1.0));
        });
    }

    fn simple_op(self: &Arc<Self>, label: &'static str, args: Vec<&str>, done: &'static str) {
        let args: Vec<String> = args.into_iter().map(str::to_string).collect();
        self.op(label, false, true, move |app, root, opts| async move {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let res = git_ok(root.as_deref(), &argv, opts).await;
            app.load_history(true);
            app.load_branches();
            res.map(|_| Some(done.to_string()))
        });
    }

    fn stage(self: &Arc<Self>, paths: Vec<String>, stage: bool) {
        // Include the source of a staged rename so unstaging restores both sides.
        let mut paths = paths;
        let unborn = {
            let s = self.st();
            if let Some(sum) = &s.summary {
                let olds: Vec<String> =
                    sum.staged.iter().filter(|c| paths.contains(&c.path)).filter_map(|c| c.old_path.clone()).collect();
                paths.extend(olds);
            }
            s.summary.as_ref().is_some_and(|x| x.unborn)
        };
        self.op("Staging", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            if stage {
                ops::stage_paths(&root, &paths, opts).await?;
            } else {
                ops::unstage_paths(&root, &paths, unborn, opts).await?;
            }
            Ok(None)
        });
    }

    fn stage_all(self: &Arc<Self>, stage: bool) {
        let unborn = self.st().summary.as_ref().is_some_and(|x| x.unborn);
        self.op("Staging", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            if stage {
                ops::stage_all(&root, opts).await?
            } else {
                ops::unstage_all(&root, unborn, opts).await?
            }
            Ok(None)
        });
    }

    fn discard(self: &Arc<Self>, path: String) {
        self.op("Discarding", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            ops::discard(&root, &path, opts).await?;
            Ok(Some(format!("Discarded changes to {path}")))
        });
    }

    fn amend_toggled(self: &Arc<Self>, on: bool) {
        let Some(root) = self.repo() else { return };
        let app = self.clone();
        self.spawn(async move {
            let msg = if on {
                blocking(move || Ok(git::head_message(&git::open(&root)?))).await.ok().flatten()
            } else {
                None
            };
            app.ui(move |_, st| {
                st.set_amend(on);
                // Prefill the previous message only if the user hasn't typed one.
                if let Some(m) = msg
                    && st.get_commit_summary().is_empty()
                    && st.get_commit_body().is_empty()
                {
                    let (summary, body) = m.trim().split_once('\n').unwrap_or((m.trim(), ""));
                    st.set_commit_summary(summary.trim().into());
                    st.set_commit_body(body.trim().into());
                }
            });
        });
    }

    fn commit(self: &Arc<Self>, confirmed_amend: bool) {
        let app = self.clone();
        // Read the draft on the UI thread, then hop to the runtime.
        let w = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let st = ui.global::<AppState>();
            let summary = st.get_commit_summary().trim().to_string();
            let body = st.get_commit_body().trim().to_string();
            let amend = st.get_amend();
            if st.get_busy() {
                return;
            }
            if summary.is_empty() {
                app.info("Write a summary first");
                return;
            }
            let (staged, conflicted, pushed_head) = {
                let s = app.st();
                let sum = s.summary.as_ref();
                (
                    sum.map(|x| x.staged.len()).unwrap_or(0),
                    sum.map(|x| x.conflicted).unwrap_or(0),
                    sum.is_some_and(|x| x.upstream.is_some() && x.ahead == 0),
                )
            };
            if conflicted > 0 {
                app.info("Resolve and stage the conflicted files first");
                return;
            }
            if !amend && staged == 0 {
                app.info("Nothing staged — stage files to commit");
                return;
            }
            if amend && pushed_head && !confirmed_amend {
                app.confirm(
                    Pending::Amend,
                    "Amend a pushed commit?",
                    "The last commit is already on the remote. Amending rewrites it, so the next push would need a force-push (Gitust never force-pushes for you).",
                    "Amend anyway",
                    true,
                );
                return;
            }
            let message = if body.is_empty() { summary } else { format!("{summary}\n\n{body}") };
            app.op("Committing", false, true, move |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                let sha = ops::commit(&root, message, amend, opts).await?;
                app.ui(|_, st| {
                    st.set_commit_summary("".into());
                    st.set_commit_body("".into());
                    st.set_amend(false);
                });
                app.load_history(true);
                app.load_branches();
                Ok(Some(if amend { format!("Amended {sha}") } else { format!("Committed {sha}") }))
            });
        });
    }

    fn stash(self: &Arc<Self>) {
        let branch = self.st().summary.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
        self.op("Stashing", false, true, move |_, root, opts| async move {
            let msg = format!("Gitust: {branch} {}", util::date(util::now()));
            git_ok(root.as_deref(), &["stash", "push", "--include-untracked", "-m", &msg], opts)
                .await
                .map(|_| Some("Changes stashed".into()))
        });
    }

    fn stash_pop(self: &Arc<Self>) {
        self.op("Restoring stash", false, true, |_, root, opts| async move {
            git_ok(root.as_deref(), &["stash", "pop"], opts).await.map(|_| Some("Stash restored".into()))
        });
    }

    fn fetch(self: &Arc<Self>) {
        self.op("Fetching", false, true, |app, root, opts| async move {
            git_ok(root.as_deref(), &["fetch", "--all", "--prune", "--progress"], opts).await?;
            if let Some(r) = root {
                app.st().last_fetch.insert(r, util::now());
            }
            app.load_branches();
            Ok(Some("Fetched".into()))
        });
    }

    fn pull(self: &Arc<Self>) {
        self.pull_mode(ops::PullMode::Default);
    }

    /// `Default` follows the user's Git config; if Git stops because the branches
    /// diverged and no strategy is configured, ask instead of failing.
    fn pull_mode(self: &Arc<Self>, mode: ops::PullMode) {
        self.op("Pulling", false, true, move |app, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            let res = ops::pull(&root, mode, opts).await;
            app.load_history(true);
            app.load_branches();
            let out = match res {
                Err(e) if mode == ops::PullMode::Default && ops::is_divergence(&e) => {
                    app.st().last_fetch.insert(root, util::now());
                    app.ui(|_, st| st.set_dialog("pull-diverged".into()));
                    return Ok(Some("Branches diverged — choose merge or rebase".into()));
                }
                r => r?,
            };
            let root = Some(root);
            if let Some(r) = root {
                app.st().last_fetch.insert(r, util::now());
            }
            let up_to_date = out.stdout.contains("Already up to date");
            Ok(Some(if up_to_date { "Already up to date".into() } else { "Pulled".into() }))
        });
    }

    fn push(self: &Arc<Self>) {
        let (branch, upstream, remote, blocked) = {
            let s = self.st();
            match &s.summary {
                Some(x) => (x.branch.clone(), x.upstream.is_some(), x.remote.clone(), x.detached || x.unborn),
                None => return,
            }
        };
        if blocked {
            self.info("Check out a branch with at least one commit to push");
            return;
        }
        let Some(remote) = remote else {
            self.info("This repository has no remote to push to");
            return;
        };
        self.op(
            if upstream { "Pushing" } else { "Publishing branch" },
            false,
            true,
            move |app, root, opts| async move {
                let args: Vec<&str> = if upstream {
                    vec!["push", "--progress"]
                } else {
                    vec!["push", "--progress", "--set-upstream", &remote, &branch]
                };
                git_ok(root.as_deref(), &args, opts).await?;
                app.load_history(true);
                app.load_branches();
                Ok(Some(if upstream { "Pushed".into() } else { format!("Published {branch} to {remote}") }))
            },
        );
    }

    fn request_checkout(self: &Arc<Self>, name: String, remote: bool) {
        let dirty = self.st().summary.as_ref().is_some_and(|s| s.has_tracked_changes());
        if dirty {
            self.confirm(
                Pending::Checkout { name: name.clone(), remote },
                "Switch with uncommitted changes?",
                &format!(
                    "Your changes will come with you to {name}. If they'd conflict, Git cancels the switch and nothing is lost. Stash them first if you'd rather leave them here."
                ),
                "Switch branch",
                false,
            );
        } else {
            self.checkout(name, remote);
        }
    }

    fn checkout(self: &Arc<Self>, name: String, remote: bool) {
        let local_exists = {
            let s = self.st();
            let local = if remote { name.split_once('/').map(|x| x.1).unwrap_or(&name) } else { &name };
            s.branches.iter().any(|b| !b.is_remote && b.name == local)
        };
        self.op("Switching branch", false, true, move |app, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            let local = if remote {
                name.split_once('/').map(|x| x.1.to_string()).unwrap_or(name.clone())
            } else {
                name.clone()
            };
            let track = (remote && !local_exists).then_some(name.as_str());
            ops::switch(&root, &local, track, opts).await?;
            app.load_history(true);
            app.load_branches();
            Ok(Some(format!("Switched to {local}")))
        });
    }

    fn create_branch(self: &Arc<Self>, name: String) {
        if !git::valid_branch_name(&name) {
            self.info(format!("'{name}' isn't a valid branch name (no spaces, '..', '~', '^', ':' or leading '-')"));
            return;
        }
        if self.st().branches.iter().any(|b| !b.is_remote && b.name == name) {
            self.info(format!("A branch named {name} already exists"));
            return;
        }
        self.ui(|_, st| st.set_dialog("".into()));
        self.op("Creating branch", false, true, move |app, root, opts| async move {
            git_ok(root.as_deref(), &["switch", "-c", &name], opts).await?;
            app.load_branches();
            Ok(Some(format!("Created and switched to {name}")))
        });
    }

    // ───────────────────────── clone ─────────────────────────

    fn show_clone(self: &Arc<Self>, url: String) {
        if !url.is_empty() {
            self.suggest_clone_dest(&url);
        }
        self.ui(move |_, st| {
            if !url.is_empty() {
                st.set_clone_url(url.into());
            }
            st.set_dialog("clone".into());
        });
    }

    fn suggest_clone_dest(self: &Arc<Self>, url: &str) {
        let root = self.st().clone_root.clone();
        let dest = util::repo_name_from_url(&util::expand_clone_url(url)).map(|n| root.join(n).display().to_string());
        self.ui(move |_, st| st.set_clone_dest(dest.unwrap_or_default().into()));
    }

    fn browse_folder(self: &Arc<Self>, for_clone: bool) {
        let app = self.clone();
        self.spawn(async move {
            let start = app.st().clone_root.clone();
            let picked = rfd::AsyncFileDialog::new()
                .set_title(if for_clone { "Clone into folder" } else { "Default clone folder" })
                .set_directory(&start)
                .pick_folder()
                .await;
            let Some(dir) = picked else { return };
            let dir = dir.path().to_path_buf();
            if for_clone {
                let url = {
                    let w = app.ui.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = slint::invoke_from_event_loop(move || {
                        let v =
                            w.upgrade().map(|u| u.global::<AppState>().get_clone_url().to_string()).unwrap_or_default();
                        let _ = tx.send(v);
                    });
                    rx.await.unwrap_or_default()
                };
                let name = util::repo_name_from_url(&util::expand_clone_url(&url));
                let dest = match name {
                    Some(n) if dir.file_name().is_none_or(|f| f.to_string_lossy() != n) => dir.join(n),
                    _ => dir,
                };
                app.ui(move |_, st| st.set_clone_dest(dest.display().to_string().into()));
            } else {
                let _ = app.db().set("clone_root", &dir.display().to_string());
                app.st().clone_root = dir.clone();
                app.ui(move |_, st| st.set_clone_root(dir.display().to_string().into()));
            }
        });
    }

    fn start_clone(self: &Arc<Self>) {
        let w = self.ui.clone();
        let app = self.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let st = ui.global::<AppState>();
            let url = util::expand_clone_url(&st.get_clone_url());
            let dest = PathBuf::from(st.get_clone_dest().trim());
            if url.is_empty() || dest.as_os_str().is_empty() {
                return;
            }
            if url.starts_with('-') {
                app.info("That doesn't look like a repository URL");
                return;
            }
            if dest.exists() && std::fs::read_dir(&dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
                app.info(format!("{} already exists and isn't empty", dest.display()));
                return;
            }
            st.set_dialog("".into());
            app.op("Cloning", false, false, move |app, _, opts| async move {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let d = dest.display().to_string();
                let res = git_ok(None, &["clone", "--progress", "--", &url, &d], opts).await;
                if res.is_err()
                    && dest.exists()
                    && std::fs::read_dir(&dest).map(|mut x| x.next().is_none()).unwrap_or(false)
                {
                    let _ = std::fs::remove_dir(&dest);
                }
                res?;
                app.open_repo(dest.clone(), true);
                app.st().gh_loaded.remove("repos");
                Ok(Some(format!("Cloned into {d}")))
            });
        });
    }

    // ───────────────────────── external apps ─────────────────────────

    fn open_url(&self, url: &str) {
        if is_http_url(url) {
            let _ = open::that_detached(url);
        }
    }

    fn open_file(&self, rel: &str) {
        let Some(root) = self.repo() else { return };
        let path = root.join(rel);
        if is_executable_like(&path) || !path.exists() {
            reveal(&path);
        } else {
            let _ = open::that_detached(&path);
        }
    }

    fn open_terminal(&self) {
        let Some(dir) = self.repo() else { return };
        if let Err(e) = spawn_terminal(&dir) {
            self.fail(anyhow!("Couldn't open a terminal: {e}"));
        }
    }

    fn create_pr(self: &Arc<Self>) {
        let (slug, branch, upstream) = {
            let s = self.st();
            (
                app_slug(&s),
                s.summary.as_ref().map(|x| x.branch.clone()),
                s.summary.as_ref().is_some_and(|x| x.upstream.is_some()),
            )
        };
        let (Some(slug), Some(branch)) = (slug, branch) else { return };
        if !upstream {
            self.info("Publish this branch first, then create the pull request");
            return;
        }
        self.open_url(&format!("https://github.com/{slug}/compare/{}?expand=1", encode_branch(&branch)));
    }

    // ───────────────────────── command palette ─────────────────────────

    fn palette_query(self: &Arc<Self>, q: &str) {
        let items = {
            let s = self.st();
            build_palette(&s, q)
        };
        self.ui(move |_, st| {
            st.set_palette_index(0);
            set_model!(st, get_palette_items, set_palette_items, items);
        });
    }

    fn palette_run(self: &Arc<Self>, id: &str) {
        self.ui(|_, st| st.set_dialog("".into()));
        let (kind, arg) = id.split_once(':').unwrap_or((id, ""));
        match kind {
            "repo" => self.open_repo(PathBuf::from(arg), true),
            "branch" => {
                let (remote, name) = arg.split_once(':').unwrap_or(("0", arg));
                self.request_checkout(name.to_string(), remote == "1");
            }
            "clone" => self.show_clone(arg.to_string()),
            "tab" => {
                let t: i32 = arg.parse().unwrap_or(0);
                self.ui(move |_, st| st.set_tab(t));
                self.tab_changed(t);
            }
            "theme" => {
                let m: i32 = arg.parse().unwrap_or(0);
                let _ = self.db().set("theme", &m.to_string());
                self.ui(move |_, st| st.set_theme_mode(m));
            }
            "cmd" => match arg {
                "open" => self.pick_repo(),
                "clone" => self.show_clone(String::new()),
                "refresh" => self.schedule_refresh(),
                "fetch" => self.fetch(),
                "pull" => self.pull(),
                "push" => self.push(),
                "commit" => self.commit(false),
                "stage-all" => self.stage_all(true),
                "unstage-all" => self.stage_all(false),
                "stash" => self.stash(),
                "stash-pop" => self.stash_pop(),
                "new-branch" => self.show_dialog("new-branch"),
                "switch-branch" => self.show_dialog("switch-branch"),
                "terminal" => self.open_terminal(),
                "explorer" => {
                    if let Some(r) = self.repo() {
                        let _ = open::that_detached(r);
                    }
                }
                "github" => self.ui(|_, st| st.invoke_open_on_github()),
                "create-pr" => self.create_pr(),
                "settings" => self.show_dialog("settings"),
                "sign-out" => self.gh_sign_out(),
                _ => {}
            },
            _ => {}
        }
    }

    // ───────────────────────── GitHub ─────────────────────────

    fn gh_restore_session(self: &Arc<Self>) {
        let cached_login = self.db().get("gh_login");
        let app = self.clone();
        self.spawn(async move {
            let Some(token) = blocking(|| Ok(github::load_token())).await.ok().flatten() else { return };
            let Ok(client) = github::client(&token) else { return };
            // Show the cached identity immediately; verify in the background.
            if let Some(login) = cached_login.clone() {
                app.gh_set_session(client.clone(), login);
            }
            match github::whoami(&client).await {
                Ok(login) => {
                    if cached_login.as_deref() != Some(login.as_str()) {
                        app.gh_set_session(client, login);
                    }
                }
                Err(e) if e.to_string().contains("rejected the token") => {
                    github::delete_token();
                    app.gh_clear_session();
                    app.fail(e);
                }
                Err(e) => {
                    if cached_login.is_none() {
                        app.fail(e);
                    } else {
                        app.ui(|_, st| st.set_gh_error("Offline — showing cached data.".into()));
                    }
                }
            }
        });
    }

    fn gh_set_session(self: &Arc<Self>, client: octocrab::Octocrab, login: String) {
        let _ = self.db().set("gh_login", &login);
        {
            let mut s = self.st();
            s.gh = Some(client);
            s.gh_login = login.clone();
            s.gh_loaded.clear();
            if s.gh_repos.is_empty()
                && let Some((payload, _)) = self.db().cache_get("repos")
            {
                s.gh_repos = serde_json::from_str(&payload).unwrap_or_default();
            }
        }
        self.ui(move |_, st| {
            st.set_gh_signed_in(true);
            st.set_gh_login(login.into());
            st.set_gh_error("".into());
            st.set_signing_in(false);
            st.set_device_code("".into());
        });
        self.push_gh_repos();
        if self.st().tab == 3 {
            self.gh_load(false);
        }
    }

    fn gh_clear_session(self: &Arc<Self>) {
        {
            let mut s = self.st();
            s.gh = None;
            s.gh_login.clear();
            s.gh_repos.clear();
            s.gh_items.clear();
            s.gh_loaded.clear();
        }
        let db = self.db();
        let _ = db.cache_clear();
        let _ = db.set("gh_login", "");
        drop(db);
        self.ui(|_, st| {
            st.set_gh_signed_in(false);
            st.set_gh_login("".into());
            st.set_gh_repos(ModelRc::default());
            st.set_gh_items(ModelRc::default());
            st.set_signing_in(false);
            st.set_device_code("".into());
        });
    }

    async fn gh_finish_sign_in(self: &Arc<Self>, token: String) -> Result<()> {
        let client = github::client(&token)?;
        let login = github::whoami(&client).await?;
        blocking(move || github::save_token(&token)).await?;
        self.gh_set_session(client, login.clone());
        self.info(format!("Signed in to GitHub as {login}"));
        Ok(())
    }

    fn gh_begin(self: &Arc<Self>) {
        self.ui(|_, st| {
            st.set_signing_in(true);
            st.set_gh_error("".into());
        });
    }

    fn gh_end_with(self: &Arc<Self>, r: Result<()>) {
        if let Err(e) = r {
            let m = format!("{e:#}");
            self.ui(move |_, st| {
                st.set_signing_in(false);
                st.set_device_code("".into());
                st.set_gh_error(m.into());
            });
        }
    }

    fn gh_sign_in_cli(self: &Arc<Self>) {
        self.gh_begin();
        let app = self.clone();
        self.spawn(async move {
            let r = async {
                let token = github::gh_cli_token().await?;
                app.gh_finish_sign_in(token).await
            }
            .await;
            app.gh_end_with(r);
        });
    }

    fn gh_sign_in_token(self: &Arc<Self>, token: String) {
        if token.is_empty() {
            return;
        }
        self.gh_begin();
        let app = self.clone();
        self.spawn(async move {
            let r = app.gh_finish_sign_in(token).await;
            app.gh_end_with(r);
        });
    }

    fn gh_sign_in_device(self: &Arc<Self>) {
        let Some(client_id) = github::device_client_id() else { return };
        self.gh_begin();
        let cancel = Arc::new(Notify::new());
        self.st().device_cancel = Some(cancel.clone());
        let app = self.clone();
        self.spawn(async move {
            let r = async {
                let login = github::DeviceLogin::start(&client_id).await?;
                let (code, url) = (login.user_code().to_string(), login.verification_uri().to_string());
                app.ui(move |_, st| {
                    st.set_device_code(code.into());
                    st.set_device_url(url.into());
                });
                let token = tokio::select! {
                    t = login.wait() => t?,
                    _ = cancel.notified() => {
                        app.ui(|_, st| { st.set_signing_in(false); st.set_device_code("".into()); });
                        return Ok(());
                    }
                };
                app.gh_finish_sign_in(token).await
            }
            .await;
            app.st().device_cancel = None;
            app.gh_end_with(r);
        });
    }

    fn gh_sign_out(self: &Arc<Self>) {
        let app = self.clone();
        self.spawn(async move {
            let _ = blocking(|| {
                github::delete_token();
                Ok(())
            })
            .await;
            app.gh_clear_session();
            app.info("Signed out of GitHub. To revoke the token itself, visit github.com/settings/applications.");
        });
    }

    /// Loads the visible GitHub list if it's stale (or `force`). Never polls.
    fn gh_load(self: &Arc<Self>, force: bool) {
        let (client, tab, key, slug, seq) = {
            let mut s = self.st();
            let Some(client) = s.gh.clone() else { return };
            let tab = s.gh_tab;
            let slug = if s.gh_this_repo { app_slug(&s) } else { None };
            let key = match tab {
                0 => "repos".to_string(),
                1 => format!("prs:{}", slug.clone().unwrap_or_default()),
                2 => format!("issues:{}", slug.clone().unwrap_or_default()),
                _ => "notifications".to_string(),
            };
            let fresh = s.gh_loaded.get(&key).is_some_and(|t| util::now() - t < GH_STALE_SECS);
            if fresh && !force {
                return;
            }
            s.gh_seq += 1;
            (client, tab, key, slug, s.gh_seq)
        };
        self.ui(|_, st| {
            st.set_gh_loading(true);
            st.set_gh_error("".into());
        });
        let app = self.clone();
        self.spawn(async move {
            let result: Result<()> = async {
                if tab == 0 {
                    let repos = github::repos(&client).await?;
                    let json = serde_json::to_string(&repos)?;
                    let _ = app.db().cache_put("repos", &json);
                    let local = app.find_local_clones(&repos).await;
                    let mut s = app.st();
                    s.gh_repos = repos;
                    s.gh_local = local;
                } else {
                    let items = match tab {
                        1 => github::search(&client, "pr", slug.as_deref()).await?,
                        2 => github::search(&client, "issue", slug.as_deref()).await?,
                        _ => github::notifications(&client).await?,
                    };
                    let mut s = app.st();
                    if s.gh_seq != seq {
                        return Ok(()); // user switched tabs meanwhile
                    }
                    s.gh_items = items;
                }
                app.st().gh_loaded.insert(key, util::now());
                Ok(())
            }
            .await;
            let latest = app.st().gh_seq == seq;
            app.ui(move |_, st| {
                if latest {
                    st.set_gh_loading(false);
                }
            });
            match result {
                Ok(()) => {
                    app.push_gh_repos();
                    if latest {
                        app.push_gh_items();
                    }
                }
                Err(e) => {
                    let m = format!("{e:#}");
                    app.ui(move |_, st| st.set_gh_error(m.into()));
                }
            }
        });
    }

    /// Which GitHub repos already exist locally (recents or the clone folder).
    async fn find_local_clones(self: &Arc<Self>, repos: &[github::Repo]) -> HashMap<String, String> {
        let recents: Vec<String> = self.db().recents().unwrap_or_default().into_iter().map(|r| r.path).collect();
        let clone_root = self.st().clone_root.clone();
        let wanted: Vec<(String, String)> = repos
            .iter()
            .map(|r| (r.full_name.to_lowercase(), r.full_name.rsplit('/').next().unwrap_or("").to_string()))
            .collect();
        blocking(move || {
            let mut map = HashMap::new();
            for p in recents {
                if let Ok(repo) = git2::Repository::open(&p)
                    && let Some(slug) = git::github_slug(&repo)
                {
                    map.insert(slug.to_lowercase(), p);
                }
            }
            for (slug, name) in wanted {
                if map.contains_key(&slug) || name.is_empty() {
                    continue;
                }
                let candidate = clone_root.join(&name);
                if candidate.join(".git").exists()
                    && let Ok(repo) = git2::Repository::open(&candidate)
                    && git::github_slug(&repo).is_some_and(|s| s.to_lowercase() == slug)
                {
                    map.insert(slug, candidate.display().to_string());
                }
            }
            Ok(map)
        })
        .await
        .unwrap_or_default()
    }

    fn push_gh_repos(&self) {
        let items: Vec<GhRepoItem> = {
            let s = self.st();
            let q = s.gh_query.trim().to_lowercase();
            let now = util::now();
            s.gh_repos
                .iter()
                .filter(|r| {
                    q.is_empty()
                        || r.full_name.to_lowercase().contains(&q)
                        || r.description.as_deref().unwrap_or("").to_lowercase().contains(&q)
                })
                .map(|r| GhRepoItem {
                    full_name: sh(r.full_name.clone()),
                    description: sh(r.description.clone().unwrap_or_default()),
                    private: r.private,
                    fork: r.fork,
                    archived: r.archived,
                    stars: sh(util::compact_count(r.stargazers_count)),
                    language: sh(r.language.clone().unwrap_or_default()),
                    updated: sh(r
                        .pushed_at
                        .as_deref()
                        .and_then(util::parse_iso8601)
                        .map(|t| util::relative_time(t, now))
                        .unwrap_or_default()),
                    clone_url: sh(r.clone_url.clone()),
                    html_url: sh(r.html_url.clone()),
                    local_path: sh(s.gh_local.get(&r.full_name.to_lowercase()).cloned().unwrap_or_default()),
                })
                .collect()
        };
        self.ui(move |_, st| set_model!(st, get_gh_repos, set_gh_repos, items));
    }

    fn push_gh_items(&self) {
        let items: Vec<GhItem> = {
            let s = self.st();
            let q = s.gh_query.trim().to_lowercase();
            let want = match s.gh_tab {
                1 => "pr",
                2 => "issue",
                _ => "notification",
            };
            s.gh_items
                .iter()
                .filter(|i| i.kind == want)
                .filter(|i| {
                    q.is_empty() || i.title.to_lowercase().contains(&q) || i.subtitle.to_lowercase().contains(&q)
                })
                .map(|i| GhItem {
                    kind: sh(i.kind),
                    title: sh(i.title.clone()),
                    subtitle: sh(i.subtitle.clone()),
                    state: sh(i.state.clone()),
                    url: sh(i.url.clone()),
                    unread: i.unread,
                })
                .collect()
        };
        self.ui(move |_, st| set_model!(st, get_gh_items, set_gh_items, items));
    }
}

// ───────────────────────────── free helpers ─────────────────────────────

fn read_repo(path: &Path) -> Result<(git::Summary, Vec<String>)> {
    let repo = git::open(path)?;
    Ok((git::summary(&repo)?, git::programs_run_by_repo(&repo)))
}

fn commit_matches(c: &git::CommitInfo, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || c.summary.to_lowercase().contains(&q)
        || c.author.to_lowercase().contains(&q)
        || c.id.starts_with(&q)
}

fn app_slug(s: &State) -> Option<String> {
    s.summary.as_ref().and_then(|x| x.github_slug.clone())
}

fn split_path(p: &str) -> (String, String) {
    match p.rsplit_once('/') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (p.to_string(), String::new()),
    }
}

fn change_rows(sum: &git::Summary) -> Vec<ChangeRow> {
    let mut rows = Vec::with_capacity(sum.staged.len() + sum.unstaged.len() + 2);
    let header = |title: &str, staged: bool, count: usize| ChangeRow {
        header: true,
        staged,
        title: title.into(),
        count: count as i32,
        ..Default::default()
    };
    let file = |c: &FileChange| {
        let (name, dir) = split_path(&c.path);
        ChangeRow {
            header: false,
            staged: c.staged,
            title: SharedString::default(),
            count: 0,
            path: c.path.clone().into(),
            name: name.into(),
            dir: match &c.old_path {
                Some(old) => format!("← {old}").into(),
                None => dir.into(),
            },
            status: c.status.to_string().into(),
        }
    };
    if !sum.staged.is_empty() {
        rows.push(header("Staged", true, sum.staged.len()));
        rows.extend(sum.staged.iter().map(file));
    }
    if !sum.unstaged.is_empty() {
        rows.push(header("Changes", false, sum.unstaged.len()));
        rows.extend(sum.unstaged.iter().map(file));
    }
    rows
}

fn build_palette(s: &State, query: &str) -> Vec<PaletteItem> {
    let has_repo = s.repo.is_some();
    let sum = s.summary.as_ref();
    let slug = sum.and_then(|x| x.github_slug.as_deref()).is_some();
    let upstream = sum.is_some_and(|x| x.upstream.is_some());
    let remote = sum.is_some_and(|x| x.remote.is_some());
    let mut out: Vec<(i32, PaletteItem)> = Vec::new();
    let mut add = |id: String, label: String, hint: &str, group: &str, bias: i32| {
        if let Some(score) = util::fuzzy_score(query, &label) {
            out.push((
                score + bias,
                PaletteItem { id: id.into(), label: label.into(), hint: hint.into(), group: group.into() },
            ));
        }
    };

    if s.palette_mode == PaletteMode::All {
        let cmds: &[(&str, &str, &str, bool)] = &[
            ("cmd:open", "Open repository…", "Ctrl+O", true),
            ("cmd:clone", "Clone repository…", "Ctrl+Shift+N", true),
            ("cmd:refresh", "Refresh", "F5", has_repo),
            ("cmd:fetch", "Fetch", "Ctrl+Shift+F", remote),
            ("cmd:pull", "Pull", "Ctrl+Shift+L", has_repo && upstream),
            ("cmd:push", if upstream { "Push" } else { "Publish branch" }, "Ctrl+Shift+P", remote),
            ("cmd:commit", "Commit staged changes", "Ctrl+Enter", has_repo),
            ("cmd:stage-all", "Stage all changes", "Ctrl+Shift+A", has_repo),
            ("cmd:unstage-all", "Unstage all changes", "", has_repo),
            ("cmd:stash", "Stash all changes", "Ctrl+Shift+S", has_repo),
            ("cmd:stash-pop", "Pop latest stash", "", has_repo && sum.is_some_and(|x| x.stash_count > 0)),
            ("cmd:new-branch", "New branch…", "Ctrl+B", has_repo),
            ("cmd:switch-branch", "Switch branch…", "Ctrl+Shift+B", has_repo),
            ("cmd:terminal", "Open in terminal", "Ctrl+`", has_repo),
            ("cmd:explorer", "Show in file manager", "Ctrl+Shift+E", has_repo),
            ("cmd:github", "Open repository on GitHub", "Ctrl+Shift+G", slug),
            ("cmd:create-pr", "Create pull request", "Ctrl+Shift+R", slug),
            ("tab:0", "Go to Changes", "Ctrl+1", has_repo),
            ("tab:1", "Go to History", "Ctrl+2", has_repo),
            ("tab:2", "Go to Branches", "Ctrl+3", has_repo),
            ("tab:3", "Go to GitHub", "Ctrl+4", true),
            ("cmd:settings", "Settings", "Ctrl+,", true),
            ("theme:0", "Theme: follow system", "", true),
            ("theme:1", "Theme: light", "", true),
            ("theme:2", "Theme: dark", "", true),
            ("cmd:sign-out", "Sign out of GitHub", "", s.gh.is_some()),
        ];
        for (id, label, hint, enabled) in cmds {
            if *enabled {
                add(id.to_string(), label.to_string(), hint, "Command", 6);
            }
        }
        for r in &s.recents {
            if !r.current && !r.missing {
                add(format!("repo:{}", r.path), r.name.to_string(), "", "Repo", 2);
            }
        }
        if !query.trim().is_empty() {
            for r in s.gh_repos.iter().take(300) {
                match s.gh_local.get(&r.full_name.to_lowercase()) {
                    Some(path) => add(format!("repo:{path}"), r.full_name.clone(), "local", "GitHub", -2),
                    None => add(format!("clone:{}", r.clone_url), r.full_name.clone(), "clone", "GitHub", -2),
                }
            }
        }
    }
    for b in s.branches.iter().filter(|b| !b.is_head) {
        let remote = if b.is_remote { "1" } else { "0" };
        add(
            format!("branch:{remote}:{}", b.name),
            b.name.clone(),
            if b.is_remote { "remote" } else { "" },
            "Branch",
            0,
        );
    }

    // Stable sort keeps the curated order for an empty query.
    out.sort_by_key(|x| std::cmp::Reverse(x.0));
    out.into_iter().take(60).map(|(_, i)| i).collect()
}

fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path.as_os_str());
        let _ = std::process::Command::new("explorer.exe").arg(arg).spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = path.parent() {
            let _ = open::that_detached(parent);
        }
    }
}

fn spawn_terminal(dir: &Path) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        if Command::new("wt.exe").arg("-d").arg(dir).spawn().is_ok() {
            return Ok(());
        }
        Command::new("powershell.exe").arg("-NoExit").current_dir(dir).creation_flags(CREATE_NEW_CONSOLE).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").args(["-a", "Terminal"]).arg(dir).spawn()?;
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "alacritty", "kitty", "xterm"] {
            if Command::new(term).current_dir(dir).spawn().is_ok() {
                return Ok(());
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal emulator found"))
    }
}

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
