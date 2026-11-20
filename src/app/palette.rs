//! The command palette.

use super::*;

impl App {
    pub(super) fn palette_query(self: &Arc<Self>, q: &str) {
        let items = {
            let s = self.st();
            build_palette(&s, q)
        };
        self.ui(move |_, st| {
            st.set_palette_index(0);
            set_model!(st, get_palette_items, set_palette_items, items);
        });
    }

    pub(super) fn palette_run(self: &Arc<Self>, id: &str) {
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
}

pub(super) fn build_palette(s: &State, query: &str) -> Vec<PaletteItem> {
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
