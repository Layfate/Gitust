//! Opening repositories, refreshing status and the trust check.

use super::*;

impl App {
    pub(super) fn pick_repo(self: &Arc<Self>) {
        let app = self.clone();
        self.spawn(async move {
            if let Some(dir) = rfd::AsyncFileDialog::new().set_title("Open a Git repository").pick_folder().await {
                app.open_repo(dir.path().to_path_buf(), true);
            }
        });
    }

    pub(super) fn push_recents(self: &Arc<Self>) {
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
            app.apply_account_pin(&root);
            app.refresh_branch_pr();
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

    pub(super) async fn refresh_now(self: &Arc<Self>) {
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
    pub(super) fn apply_trust(self: &Arc<Self>, root: &Path, findings: Vec<String>) {
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

    pub(super) fn apply_summary(self: &Arc<Self>, sum: git::Summary, opened: bool) {
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
            st.set_head_id(sum.head.clone().unwrap_or_default().into());
            st.set_head_undoable(sum.head_has_parent);
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
            self.refresh_branch_pr();
        }
    }
}

pub(super) fn read_repo(path: &Path) -> Result<(git::Summary, Vec<String>)> {
    let repo = git::open(path)?;
    Ok((git::summary(&repo)?, git::programs_run_by_repo(&repo)))
}
