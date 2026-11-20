//! Repository operations: the op runner, staging, commit, stash, pull/push, branches.

use super::*;

impl App {
    /// Runs a repository mutation. `quiet` ops (stage/unstage) queue behind the lock
    /// without the busy bar; loud ops refuse to overlap and show progress + cancel.
    pub(super) fn op<F, Fut>(self: &Arc<Self>, label: &'static str, quiet: bool, needs_repo: bool, f: F)
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

    pub(super) fn progress(&self, line: &str) {
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

    pub(super) fn simple_op(self: &Arc<Self>, label: &'static str, args: Vec<&str>, done: &'static str) {
        let args: Vec<String> = args.into_iter().map(str::to_string).collect();
        self.op(label, false, true, move |app, root, opts| async move {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let res = git_ok(root.as_deref(), &argv, opts).await;
            app.load_history(true);
            app.load_branches();
            res.map(|_| Some(done.to_string()))
        });
    }

    pub(super) fn stage(self: &Arc<Self>, paths: Vec<String>, stage: bool) {
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

    pub(super) fn stage_all(self: &Arc<Self>, stage: bool) {
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

    pub(super) fn discard(self: &Arc<Self>, path: String) {
        self.op("Discarding", true, true, move |_, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            ops::discard(&root, &path, opts).await?;
            Ok(Some(format!("Discarded changes to {path}")))
        });
    }

    pub(super) fn amend_toggled(self: &Arc<Self>, on: bool) {
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

    pub(super) fn commit(self: &Arc<Self>, confirmed_amend: bool) {
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

    pub(super) fn stash(self: &Arc<Self>) {
        let branch = self.st().summary.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
        self.op("Stashing", false, true, move |_, root, opts| async move {
            let msg = format!("Gitust: {branch} {}", util::date(util::now()));
            git_ok(root.as_deref(), &["stash", "push", "--include-untracked", "-m", &msg], opts)
                .await
                .map(|_| Some("Changes stashed".into()))
        });
    }

    pub(super) fn stash_pop(self: &Arc<Self>) {
        self.op("Restoring stash", false, true, |_, root, opts| async move {
            git_ok(root.as_deref(), &["stash", "pop"], opts).await.map(|_| Some("Stash restored".into()))
        });
    }

    pub(super) fn fetch(self: &Arc<Self>) {
        self.op("Fetching", false, true, |app, root, opts| async move {
            git_ok(root.as_deref(), &["fetch", "--all", "--prune", "--progress"], opts).await?;
            if let Some(r) = root {
                app.st().last_fetch.insert(r, util::now());
            }
            app.load_branches();
            app.refresh_branch_pr();
            Ok(Some("Fetched".into()))
        });
    }

    pub(super) fn pull(self: &Arc<Self>) {
        self.pull_mode(ops::PullMode::Default);
    }

    /// `Default` follows the user's Git config; if Git stops because the branches
    /// diverged and no strategy is configured, ask instead of failing.
    pub(super) fn pull_mode(self: &Arc<Self>, mode: ops::PullMode) {
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

    pub(super) fn push(self: &Arc<Self>) {
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
                app.refresh_branch_pr();
                Ok(Some(if upstream { "Pushed".into() } else { format!("Published {branch} to {remote}") }))
            },
        );
    }

    pub(super) fn request_checkout(self: &Arc<Self>, name: String, remote: bool) {
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

    pub(super) fn checkout(self: &Arc<Self>, name: String, remote: bool) {
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

    pub(super) fn create_branch(self: &Arc<Self>, name: String) {
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
}
