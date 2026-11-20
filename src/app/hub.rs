//! GitHub accounts, lists and the update check.

use super::*;

impl App {
    pub(super) fn accounts(&self) -> github::Accounts {
        github::Accounts::parse(self.db().get("gh_accounts").as_deref())
    }

    pub(super) fn save_accounts(&self, accounts: &github::Accounts) {
        let _ = self.db().set("gh_accounts", &accounts.to_json());
        let logins: Vec<SharedString> = accounts.logins.iter().map(sh).collect();
        self.ui(move |_, st| st.set_gh_accounts(ModelRc::new(VecModel::from(logins))));
    }

    pub(super) fn gh_restore_session(self: &Arc<Self>) {
        let legacy_login = self.db().get("gh_login").filter(|l| !l.is_empty());
        let app = self.clone();
        self.spawn(async move {
            // One-time move of the single-account token to its per-account entry.
            // The old entry is deleted only once the token is safely stored again.
            if let Some(token) = blocking(|| Ok(github::legacy_token())).await.ok().flatten() {
                let login = match legacy_login {
                    Some(l) => Some(l),
                    None => match github::client(&token) {
                        Ok(c) => github::whoami(&c).await.ok(),
                        Err(_) => None,
                    },
                };
                if let Some(login) = login {
                    let l = login.clone();
                    if blocking(move || github::save_token(&l, &token)).await.is_ok() {
                        let mut accounts = app.accounts();
                        accounts.activate(&login);
                        app.save_accounts(&accounts);
                        let _ = blocking(|| {
                            github::delete_legacy_token();
                            Ok(())
                        })
                        .await;
                        let _ = app.db().set("gh_login", "");
                    }
                }
            }
            let accounts = app.accounts();
            app.save_accounts(&accounts);
            if let Some(login) = accounts.active() {
                app.gh_activate(login.to_string());
            }
        });
    }

    /// Makes `login` the active account: its cached data shows at once, the
    /// token is checked in the background.
    pub(super) fn gh_activate(self: &Arc<Self>, login: String) {
        let app = self.clone();
        self.spawn(async move {
            let l = login.clone();
            let token = blocking(move || Ok(github::load_token(&l))).await.ok().flatten();
            let Some(client) = token.and_then(|t| github::client(&t).ok()) else {
                app.gh_forget(&login);
                app.fail(anyhow!(
                    "The saved GitHub token for {login} is missing from the credential vault. Sign in again."
                ));
                return;
            };
            app.gh_set_session(client.clone(), login.clone());
            match github::whoami(&client).await {
                Ok(_) => {}
                Err(e) if e.to_string().contains("rejected the token") => {
                    app.gh_forget(&login);
                    app.fail(e);
                }
                Err(_) => app.ui(|_, st| st.set_gh_error("Offline — showing cached data.".into())),
            }
        });
    }

    pub(super) fn gh_set_session(self: &Arc<Self>, client: octocrab::Octocrab, login: String) {
        let mut accounts = self.accounts();
        accounts.activate(&login);
        self.save_accounts(&accounts);
        let cached = self.db().cache_get(&format!("repos:{login}"));
        {
            let mut s = self.st();
            s.gh = Some(client);
            s.gh_login = login.clone();
            s.gh_loaded.clear();
            s.gh_items.clear();
            s.gh_repos = cached.and_then(|(payload, _)| serde_json::from_str(&payload).ok()).unwrap_or_default();
        }
        self.ui(move |_, st| {
            st.set_gh_signed_in(true);
            st.set_gh_login(login.into());
            st.set_gh_error("".into());
            st.set_signing_in(false);
            st.set_gh_adding_account(false);
            st.set_device_code("".into());
            st.set_gh_items(ModelRc::default());
        });
        self.push_gh_repos();
        self.push_account_pin();
        if self.st().tab == 3 {
            self.gh_load(false);
        }
    }

    /// Removes an account (token, cached data); switches to the next one if any.
    pub(super) fn gh_forget(self: &Arc<Self>, login: &str) {
        let l = login.to_string();
        std::thread::spawn(move || github::delete_token(&l));
        let mut accounts = self.accounts();
        accounts.remove(login);
        self.save_accounts(&accounts);
        let _ = self.db().cache_remove(&format!("repos:{login}"));
        let was_active = self.st().gh_login.eq_ignore_ascii_case(login);
        if !was_active {
            return;
        }
        match accounts.active() {
            Some(next) => self.gh_activate(next.to_string()),
            None => self.gh_clear_session(),
        }
    }

    pub(super) fn gh_clear_session(self: &Arc<Self>) {
        {
            let mut s = self.st();
            s.gh = None;
            s.gh_login.clear();
            s.gh_repos.clear();
            s.gh_items.clear();
            s.gh_loaded.clear();
        }
        let _ = self.db().cache_clear();
        self.ui(|_, st| {
            st.set_gh_signed_in(false);
            st.set_gh_login("".into());
            st.set_gh_repos(ModelRc::default());
            st.set_gh_items(ModelRc::default());
            st.set_signing_in(false);
            st.set_gh_adding_account(false);
            st.set_device_code("".into());
        });
        self.push_account_pin();
    }

    pub(super) async fn gh_finish_sign_in(self: &Arc<Self>, token: String) -> Result<()> {
        let client = github::client(&token)?;
        let login = github::whoami(&client).await?;
        let l = login.clone();
        blocking(move || github::save_token(&l, &token)).await?;
        self.gh_set_session(client, login.clone());
        self.info(format!("Signed in to GitHub as {login}"));
        Ok(())
    }

    /// Whether the open repository is set to always use the active account.
    pub(super) fn push_account_pin(&self) {
        let (root, login) = {
            let s = self.st();
            (s.repo.clone(), s.gh_login.clone())
        };
        let pinned = root.is_some_and(|r| {
            !login.is_empty()
                && self.db().get(&format!("gh_account:{}", r.display())).is_some_and(|p| p.eq_ignore_ascii_case(&login))
        });
        self.ui(move |_, st| st.set_gh_account_pinned(pinned));
    }

    /// On opening a repository: switch to the account it is pinned to.
    pub(super) fn apply_account_pin(self: &Arc<Self>, root: &Path) {
        let Some(pinned) = self.db().get(&format!("gh_account:{}", root.display())) else {
            self.push_account_pin();
            return;
        };
        let accounts = self.accounts();
        let current = self.st().gh_login.clone();
        match accounts.find(&pinned) {
            Some(login) if !login.eq_ignore_ascii_case(&current) => {
                self.info(format!("Using GitHub account {login} for this repository"));
                self.gh_activate(login.to_string());
            }
            _ => self.push_account_pin(),
        }
    }

    pub(super) fn gh_begin(self: &Arc<Self>) {
        self.ui(|_, st| {
            st.set_signing_in(true);
            st.set_gh_error("".into());
        });
    }

    pub(super) fn gh_end_with(self: &Arc<Self>, r: Result<()>) {
        if let Err(e) = r {
            let m = format!("{e:#}");
            self.ui(move |_, st| {
                st.set_signing_in(false);
                st.set_device_code("".into());
                st.set_gh_error(m.into());
            });
        }
    }

    pub(super) fn gh_sign_in_cli(self: &Arc<Self>) {
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

    pub(super) fn gh_sign_in_token(self: &Arc<Self>, token: String) {
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

    pub(super) fn gh_sign_in_device(self: &Arc<Self>) {
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

    pub(super) fn gh_sign_out(self: &Arc<Self>) {
        let login = self.st().gh_login.clone();
        if login.is_empty() {
            return;
        }
        self.gh_forget(&login);
        self.info(format!(
            "Signed {login} out of Gitust. To revoke the token itself, visit github.com/settings/applications."
        ));
    }

    /// Loads the visible GitHub list if it's stale (or `force`). Never polls.
    pub(super) fn gh_load(self: &Arc<Self>, force: bool) {
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
                    let login = app.st().gh_login.clone();
                    let _ = app.db().cache_put(&format!("repos:{login}"), &json);
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
    pub(super) async fn find_local_clones(self: &Arc<Self>, repos: &[github::Repo]) -> HashMap<String, String> {
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

    pub(super) fn push_gh_repos(&self) {
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

    pub(super) fn push_gh_items(&self) {
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
