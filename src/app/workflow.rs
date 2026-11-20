//! History editing: abort/continue an operation, cherry-pick, tags, rename
//! branch, undo last commit, and the stash list. Anything that changes history
//! or throws work away asks first.

use super::*;
use crate::StashItem;

/// What the shared name dialog was opened for.
pub(super) enum Input {
    Tag { target: String },
    RenameBranch { old: String },
}

impl App {
    pub(super) fn bind_workflow(self: &Arc<Self>, st: &AppState<'_>) {
        macro_rules! on {
            ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
                let a = self.clone();
                st.$name(move |$($arg: $ty),*| {
                    let $app = &a;
                    $body
                });
            }};
        }
        on!(on_abort_operation, |app| {
            let state = app.st().summary.as_ref().map(|s| s.state.clone()).unwrap_or_default();
            if state.is_empty() {
                return;
            }
            let what = state.to_lowercase();
            app.confirm(
                Pending::Abort(state),
                &format!("Abort {what}?"),
                "Goes back to how the branch was before it started. Conflict resolutions and staged changes from it are thrown away; your other uncommitted work is kept if Git can.",
                "Abort",
                true,
            );
        });
        on!(on_continue_operation, |app| {
            let state = app.st().summary.as_ref().map(|s| s.state.clone()).unwrap_or_default();
            app.op("Continuing", false, true, move |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                ops::continue_operation(&root, &state, opts).await?;
                app.load_history(true);
                app.load_branches();
                Ok(Some(format!("{state} continued")))
            });
        });
        on!(on_cherry_pick, |app, id: SharedString| {
            let branch = app.st().summary.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
            app.confirm(
                Pending::CherryPick(id.to_string()),
                "Cherry-pick this commit?",
                &format!(
                    "Applies the changes from {} as a new commit on {branch}. If they conflict, you resolve them in the Changes tab and continue.",
                    &id[..7.min(id.len())]
                ),
                "Cherry-pick",
                false,
            );
        });
        on!(on_create_tag, |app, id: SharedString| {
            app.st().input = Some(Input::Tag { target: id.to_string() });
            let short = id[..7.min(id.len())].to_string();
            app.ui(move |_, st| {
                st.set_input_title("Create a tag".into());
                st.set_input_label("Tag name".into());
                st.set_input_value("".into());
                st.set_input_message("".into());
                st.set_input_has_message(true);
                st.set_input_hint(
                    format!("Tags commit {short}. It stays local until you push it from the Branches tab.").into(),
                );
                st.set_input_button("Create tag".into());
                st.set_dialog("input".into());
            });
        });
        on!(on_undo_commit, |app| {
            let pushed = app.st().summary.as_ref().is_some_and(|s| s.upstream.is_some() && s.ahead == 0);
            let text = if pushed {
                "The last commit is already on the remote. Undoing it rewrites history, so your next push would need a force-push (Gitust never force-pushes for you). Its changes stay staged."
            } else {
                "Removes the last commit and puts its changes back in the staging area, ready to edit and commit again. Nothing in your files changes."
            };
            app.confirm(Pending::UndoCommit, "Undo the last commit?", text, "Undo commit", pushed);
        });
        on!(on_rename_branch, |app, name: SharedString| {
            app.st().input = Some(Input::RenameBranch { old: name.to_string() });
            let current = name.to_string();
            app.ui(move |_, st| {
                st.set_input_title("Rename branch".into());
                st.set_input_label("New name".into());
                st.set_input_value(current.clone().into());
                st.set_input_has_message(false);
                st.set_input_hint(
                    format!("Renames the local branch {current}. A published branch keeps its old name on the remote.")
                        .into(),
                );
                st.set_input_button("Rename".into());
                st.set_dialog("input".into());
            });
        });
        on!(on_delete_tag, |app, name: SharedString| app.confirm(
            Pending::DeleteTag(name.to_string()),
            "Delete tag?",
            &format!("Deletes the local tag {name}. If it was pushed, it stays on the remote."),
            "Delete tag",
            true,
        ));
        on!(on_push_tag, |app, name: SharedString| {
            let Some(remote) = app.st().summary.as_ref().and_then(|s| s.remote.clone()) else { return };
            let name = name.to_string();
            app.op("Pushing tag", false, true, move |_, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                ops::push_tag(&root, &remote, &name, opts).await?;
                Ok(Some(format!("Pushed tag {name} to {remote}")))
            });
        });
        on!(on_show_stashes, |app| app.load_stashes(true));
        on!(on_stash_apply, |app, index: i32| app.stash_at(index as usize, ops::StashAction::Apply));
        on!(on_stash_pop_at, |app, index: i32| app.stash_at(index as usize, ops::StashAction::Pop));
        on!(on_stash_drop, |app, index: i32| app.confirm(
            Pending::DropStash(index as usize),
            "Drop this stash?",
            "The stashed changes are deleted for good.",
            "Drop stash",
            true,
        ));
        on!(on_input_accept, |app| app.input_accept());
        on!(on_submit_pr, |app, title: SharedString, body: SharedString, base: SharedString, draft: bool| {
            app.submit_pr(title.to_string(), body.to_string(), base.to_string(), draft)
        });
    }

    pub(super) fn workflow_confirmed(self: &Arc<Self>, pending: Pending) {
        match pending {
            Pending::Abort(state) => self.op("Aborting", false, true, move |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                ops::abort_operation(&root, &state, opts).await?;
                app.load_history(true);
                app.load_branches();
                Ok(Some(format!("{state} aborted")))
            }),
            Pending::CherryPick(id) => self.op("Cherry-picking", false, true, move |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                let res = ops::cherry_pick(&root, &id, opts).await;
                app.load_history(true);
                res.map(|_| Some(format!("Cherry-picked {}", &id[..7.min(id.len())])))
            }),
            Pending::UndoCommit => self.op("Undoing commit", false, true, |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                let message = {
                    let r = root.clone();
                    blocking(move || Ok(git::head_message(&git::open(&r)?))).await.ok().flatten()
                };
                ops::undo_last_commit(&root, opts).await?;
                // Offer the old message for the next commit, if the box is empty.
                if let Some(m) = message {
                    app.ui(move |_, st| {
                        if st.get_commit_summary().is_empty() && st.get_commit_body().is_empty() {
                            let (summary, body) = m.trim().split_once('\n').unwrap_or((m.trim(), ""));
                            st.set_commit_summary(summary.trim().into());
                            st.set_commit_body(body.trim().into());
                        }
                    });
                }
                app.load_history(true);
                app.load_branches();
                Ok(Some("Commit undone; its changes are staged".into()))
            }),
            Pending::DeleteTag(name) => self.op("Deleting tag", true, true, move |app, root, opts| async move {
                let root = root.ok_or_else(|| anyhow!("No repository"))?;
                ops::delete_tag(&root, &name, opts).await?;
                app.load_branches();
                app.load_history(true);
                Ok(Some(format!("Deleted tag {name}")))
            }),
            Pending::DropStash(index) => self.stash_at(index, ops::StashAction::Drop),
            _ => {}
        }
    }

    fn input_accept(self: &Arc<Self>) {
        let w = self.ui.clone();
        let Some(ui) = w.upgrade() else { return };
        let st = ui.global::<AppState>();
        let value = st.get_input_value().trim().to_string();
        let message = st.get_input_message().to_string();
        let Some(input) = self.st().input.take() else { return };
        match input {
            Input::Tag { target } => {
                if !git::valid_tag_name(&value) {
                    self.st().input = Some(Input::Tag { target });
                    self.info(format!(
                        "'{value}' isn't a valid tag name (no spaces, '..', '~', '^', ':' or leading '-')"
                    ));
                    return;
                }
                if self.st().tags.iter().any(|t| t.name == value) {
                    self.st().input = Some(Input::Tag { target });
                    self.info(format!("A tag named {value} already exists"));
                    return;
                }
                st.set_dialog("".into());
                self.op("Creating tag", true, true, move |app, root, opts| async move {
                    let root = root.ok_or_else(|| anyhow!("No repository"))?;
                    ops::create_tag(&root, &value, &target, &message, opts).await?;
                    app.load_branches();
                    app.load_history(true);
                    Ok(Some(format!("Created tag {value}")))
                });
            }
            Input::RenameBranch { old } => {
                if value == old {
                    st.set_dialog("".into());
                    return;
                }
                if !git::valid_branch_name(&value) || self.st().branches.iter().any(|b| !b.is_remote && b.name == value)
                {
                    self.st().input = Some(Input::RenameBranch { old });
                    self.info(format!("'{value}' isn't a valid new branch name or already exists"));
                    return;
                }
                st.set_dialog("".into());
                self.op("Renaming branch", false, true, move |app, root, opts| async move {
                    let root = root.ok_or_else(|| anyhow!("No repository"))?;
                    ops::rename_branch(&root, &old, &value, opts).await?;
                    app.load_branches();
                    app.load_history(true);
                    Ok(Some(format!("Renamed {old} to {value}")))
                });
            }
        }
    }

    /// Reads the stash list; `open` shows the dialog.
    pub(super) fn load_stashes(self: &Arc<Self>, open: bool) {
        let Some(root) = self.repo() else { return };
        let app = self.clone();
        self.spawn(async move {
            match blocking(move || git::stashes(&mut git2::Repository::open(&root)?)).await {
                Ok(list) => {
                    let items: Vec<(i32, String)> = list.into_iter().map(|s| (s.index as i32, s.message)).collect();
                    app.ui(move |_, st| {
                        let rows: Vec<StashItem> = items
                            .into_iter()
                            .map(|(index, message)| StashItem { index, message: message.into() })
                            .collect();
                        st.set_stashes(ModelRc::new(VecModel::from(rows)));
                        if open {
                            st.set_dialog("stashes".into());
                        }
                    });
                }
                Err(e) => app.fail(e),
            }
        });
    }

    fn stash_at(self: &Arc<Self>, index: usize, action: ops::StashAction) {
        let label = match action {
            ops::StashAction::Apply => "Applying stash",
            ops::StashAction::Pop => "Restoring stash",
            ops::StashAction::Drop => "Dropping stash",
        };
        self.op(label, false, true, move |app, root, opts| async move {
            let root = root.ok_or_else(|| anyhow!("No repository"))?;
            let res = ops::stash_action(&root, index, action, opts).await;
            app.load_stashes(false);
            res?;
            Ok(Some(match action {
                ops::StashAction::Apply => "Stash applied (kept in the list)".into(),
                ops::StashAction::Pop => "Stash restored".into(),
                ops::StashAction::Drop => "Stash dropped".into(),
            }))
        });
    }
}
