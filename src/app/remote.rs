//! Cloning and opening things in other apps.

use super::*;

impl App {
    pub(super) fn show_clone(self: &Arc<Self>, url: String) {
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

    pub(super) fn suggest_clone_dest(self: &Arc<Self>, url: &str) {
        let root = self.st().clone_root.clone();
        let dest = util::repo_name_from_url(&util::expand_clone_url(url)).map(|n| root.join(n).display().to_string());
        self.ui(move |_, st| st.set_clone_dest(dest.unwrap_or_default().into()));
    }

    pub(super) fn browse_folder(self: &Arc<Self>, for_clone: bool) {
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

    pub(super) fn start_clone(self: &Arc<Self>) {
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

    pub(super) fn open_url(&self, url: &str) {
        if is_http_url(url) {
            let _ = open::that_detached(url);
        }
    }

    pub(super) fn open_file(&self, rel: &str) {
        let Some(root) = self.repo() else { return };
        let path = root.join(rel);
        if is_executable_like(&path) || !path.exists() {
            reveal(&path);
        } else {
            let _ = open::that_detached(&path);
        }
    }

    pub(super) fn open_terminal(&self) {
        let Some(dir) = self.repo() else { return };
        if let Err(e) = spawn_terminal(&dir) {
            self.fail(anyhow!("Couldn't open a terminal: {e}"));
        }
    }

    pub(super) fn create_pr(self: &Arc<Self>) {
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
}

pub(super) fn reveal(path: &Path) {
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

pub(super) fn spawn_terminal(dir: &Path) -> std::io::Result<()> {
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
