//! GitHub: sign-in, token vault, and the few REST endpoints the UI needs.
//! Responses are decoded into our own small structs so upstream model churn in
//! octocrab can't break us, and nothing private beyond repo metadata is cached.

use crate::cli;
use crate::util;
use anyhow::{Result, anyhow};
use octocrab::Octocrab;
use octocrab::secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

const VAULT_SERVICE: &str = "gitust";
/// Before multiple accounts, the single token lived under this name.
const LEGACY_VAULT_ACCOUNT: &str = "github.com";
pub const SCOPES: [&str; 3] = ["repo", "read:org", "notifications"];

// ---------- token vault (OS credential manager / keychain / secret service) ----------

/// One vault entry per signed-in account: `github.com:<login>`.
fn entry(login: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(VAULT_SERVICE, &format!("{LEGACY_VAULT_ACCOUNT}:{login}"))
        .map_err(|e| anyhow!("Credential vault unavailable: {e}"))
}

pub fn load_token(login: &str) -> Option<String> {
    entry(login).ok()?.get_password().ok().filter(|t| !t.trim().is_empty())
}

pub fn save_token(login: &str, token: &str) -> Result<()> {
    entry(login)?.set_password(token).map_err(|e| anyhow!("Couldn't save the token to the credential vault: {e}"))
}

pub fn delete_token(login: &str) {
    if let Ok(e) = entry(login) {
        let _ = e.delete_credential();
    }
}

/// The token saved by versions with a single account, if any. Startup moves
/// it to the per-account entry, then calls [`delete_legacy_token`].
pub fn legacy_token() -> Option<String> {
    keyring::Entry::new(VAULT_SERVICE, LEGACY_VAULT_ACCOUNT).ok()?.get_password().ok().filter(|t| !t.trim().is_empty())
}

pub fn delete_legacy_token() {
    if let Ok(e) = keyring::Entry::new(VAULT_SERVICE, LEGACY_VAULT_ACCOUNT) {
        let _ = e.delete_credential();
    }
}

/// Signed-in accounts, most recently used first. Stored as JSON in settings;
/// the tokens themselves are only ever in the vault.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Accounts {
    pub logins: Vec<String>,
}

impl Accounts {
    pub fn parse(json: Option<&str>) -> Self {
        json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"logins\":[]}".into())
    }

    pub fn active(&self) -> Option<&str> {
        self.logins.first().map(String::as_str)
    }

    /// Adds or re-selects `login` as the active account. GitHub logins are
    /// case-insensitive, so `octocat` and `OctoCat` are the same account.
    pub fn activate(&mut self, login: &str) {
        self.logins.retain(|l| !l.eq_ignore_ascii_case(login));
        self.logins.insert(0, login.to_string());
    }

    pub fn remove(&mut self, login: &str) {
        self.logins.retain(|l| !l.eq_ignore_ascii_case(login));
    }

    pub fn find(&self, login: &str) -> Option<&str> {
        self.logins.iter().find(|l| l.eq_ignore_ascii_case(login)).map(String::as_str)
    }
}

/// Reuses an existing `gh auth login` session.
pub async fn gh_cli_token() -> Result<String> {
    let out = cli::tool("gh").args(["auth", "token", "--hostname", "github.com"]).output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("GitHub CLI (gh) isn't installed. Get it from https://cli.github.com, or paste a token instead.")
        } else {
            anyhow!("Couldn't run gh: {e}")
        }
    })?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || token.is_empty() {
        return Err(anyhow!("gh isn't signed in. Run `gh auth login` in a terminal, then try again."));
    }
    Ok(token)
}

// ---------- client ----------

pub fn client(token: &str) -> Result<Octocrab> {
    Octocrab::builder()
        .personal_token(token.to_string())
        .build()
        .map_err(|e| anyhow!("Couldn't create GitHub client: {e}"))
}

fn explain(e: octocrab::Error) -> anyhow::Error {
    match &e {
        octocrab::Error::GitHub { source, .. } => {
            let code = source.status_code.as_u16();
            let msg = source.message.as_str();
            match code {
                401 => anyhow!("GitHub rejected the token (expired or revoked). Sign in again."),
                403 | 429 if msg.to_lowercase().contains("rate limit") => {
                    let wait = source
                        .rate_limit_reset
                        .map(|t| (t as i64 - util::now()).max(0) / 60 + 1)
                        .map(|m| format!(" Resets in about {m} min."))
                        .unwrap_or_default();
                    anyhow!("GitHub rate limit reached.{wait}")
                }
                403 => {
                    anyhow!("GitHub denied access: {msg}. The token may be missing a scope ({}).", SCOPES.join(", "))
                }
                404 => anyhow!("Not found on GitHub, or this token can't see it."),
                422 => anyhow!("GitHub couldn't process the request: {msg}"),
                _ => anyhow!("GitHub error {code}: {msg}"),
            }
        }
        _ => anyhow!("Couldn't reach GitHub — you may be offline. ({e})"),
    }
}

async fn get_json<T: serde::de::DeserializeOwned>(o: &Octocrab, route: &str) -> Result<T> {
    o.get::<T, _, ()>(route, None).await.map_err(explain)
}

#[derive(Deserialize)]
struct UserResp {
    login: String,
}

pub async fn whoami(o: &Octocrab) -> Result<String> {
    Ok(get_json::<UserResp>(o, "/user").await?.login)
}

// ---------- updates ----------

#[derive(Deserialize)]
struct ReleaseResp {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The latest published release of Gitust: `(tag, page url)`. Anonymous
/// request; nothing about the user is sent. `None` if there is no release yet.
pub async fn latest_release() -> Result<Option<(String, String)>> {
    let crab = Octocrab::builder().build().map_err(|e| anyhow!("Couldn't create GitHub client: {e}"))?;
    match get_json::<ReleaseResp>(&crab, "/repos/Layfate/Gitust/releases/latest").await {
        Ok(r) if !r.draft && !r.prerelease && r.html_url.starts_with("https://github.com/") => {
            Ok(Some((r.tag_name, r.html_url)))
        }
        Ok(_) => Ok(None),
        Err(e) if e.to_string().starts_with("Not found") => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------- repositories ----------

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Repo {
    pub full_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub stargazers_count: u64,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub pushed_at: Option<String>,
    pub clone_url: String,
    pub html_url: String,
}

/// Up to 300 most recently pushed repositories (owned, collaborator, org).
pub async fn repos(o: &Octocrab) -> Result<Vec<Repo>> {
    let mut all = Vec::new();
    for page in 1..=3 {
        let route = format!(
            "/user/repos?per_page=100&page={page}&sort=pushed&affiliation=owner,collaborator,organization_member"
        );
        let batch: Vec<Repo> = get_json(o, &route).await?;
        let done = batch.len() < 100;
        all.extend(batch);
        if done {
            break;
        }
    }
    Ok(all)
}

// ---------- pull requests, issues, notifications ----------

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: &'static str,
    pub title: String,
    pub subtitle: String,
    pub state: String,
    pub url: String,
    pub unread: bool,
}

#[derive(Deserialize)]
struct SearchResp {
    items: Vec<Issue>,
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    pull_request: Option<PrRef>,
    repository_url: String,
    #[serde(default)]
    user: Option<Login>,
    updated_at: String,
    #[serde(default)]
    comments: u64,
}

#[derive(Deserialize)]
struct PrRef {
    #[serde(default)]
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

/// `kind` is "pr" or "issue"; `repo` narrows to one repository, otherwise "involves:@me".
pub async fn search(o: &Octocrab, kind: &'static str, repo: Option<&str>) -> Result<Vec<Item>> {
    let mut q = format!("is:{} is:open archived:false", if kind == "pr" { "pr" } else { "issue" });
    match repo {
        Some(r) => q.push_str(&format!(" repo:{r}")),
        None => q.push_str(" involves:@me"),
    }
    let route = format!("/search/issues?q={}&sort=updated&order=desc&per_page=50", util::percent_encode(&q));
    let resp: SearchResp = get_json(o, &route).await?;
    let now = util::now();
    Ok(resp
        .items
        .into_iter()
        .map(|i| {
            let repo_name = i.repository_url.rsplit("/repos/").next().unwrap_or("").to_string();
            let state = match (&i.pull_request, i.draft) {
                (Some(pr), _) if pr.merged_at.is_some() => "merged".to_string(),
                (Some(_), Some(true)) => "draft".to_string(),
                _ => i.state.clone(),
            };
            let updated = util::parse_iso8601(&i.updated_at).map(|t| util::relative_time(t, now)).unwrap_or_default();
            let author = i.user.map(|u| u.login).unwrap_or_default();
            let comments = if i.comments > 0 { format!(" · {} 💬", i.comments) } else { String::new() };
            Item {
                kind,
                title: i.title,
                subtitle: format!("{repo_name}#{} · {author} · updated {updated}{comments}", i.number),
                state,
                url: i.html_url,
                unread: false,
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct Notification {
    reason: String,
    unread: bool,
    updated_at: String,
    subject: Subject,
    repository: NotificationRepo,
}

#[derive(Deserialize)]
struct Subject {
    title: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct NotificationRepo {
    full_name: String,
    html_url: String,
}

/// Maps an API subject URL to the page a human wants.
fn subject_html_url(api: Option<&str>, repo_html: &str) -> String {
    let Some(api) = api else { return repo_html.to_string() };
    let Some(rest) = api.strip_prefix("https://api.github.com/repos/") else { return repo_html.to_string() };
    let mut parts = rest.splitn(4, '/');
    let (owner, name, kind, id) = (parts.next(), parts.next(), parts.next(), parts.next());
    match (owner, name, kind, id) {
        (Some(o), Some(n), Some("pulls"), Some(id)) => format!("https://github.com/{o}/{n}/pull/{id}"),
        (Some(o), Some(n), Some("issues"), Some(id)) => format!("https://github.com/{o}/{n}/issues/{id}"),
        (Some(o), Some(n), Some("commits"), Some(id)) => format!("https://github.com/{o}/{n}/commit/{id}"),
        (Some(o), Some(n), Some("releases"), _) => format!("https://github.com/{o}/{n}/releases"),
        (Some(o), Some(n), Some("discussions"), Some(id)) => format!("https://github.com/{o}/{n}/discussions/{id}"),
        _ => repo_html.to_string(),
    }
}

pub async fn notifications(o: &Octocrab) -> Result<Vec<Item>> {
    let list: Vec<Notification> = get_json(o, "/notifications?per_page=50").await?;
    let now = util::now();
    Ok(list
        .into_iter()
        .map(|n| {
            let updated = util::parse_iso8601(&n.updated_at).map(|t| util::relative_time(t, now)).unwrap_or_default();
            let kind = match n.subject.kind.as_str() {
                "PullRequest" => "pull request",
                "Issue" => "issue",
                "Release" => "release",
                "Discussion" => "discussion",
                "CheckSuite" => "checks",
                other => other,
            };
            Item {
                kind: "notification",
                title: n.subject.title,
                subtitle: format!("{} · {kind} · {updated}", n.repository.full_name),
                state: n.reason.replace('_', " "),
                url: subject_html_url(n.subject.url.as_deref(), &n.repository.html_url),
                unread: n.unread,
            }
        })
        .collect())
}

// ---------- OAuth device flow ----------

/// The OAuth App client id. Set `GITUST_GITHUB_CLIENT_ID` at build or run time
/// (register an OAuth App with "Enable Device Flow" checked; no secret is needed).
pub fn device_client_id() -> Option<String> {
    std::env::var("GITUST_GITHUB_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("GITUST_GITHUB_CLIENT_ID").map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

pub struct DeviceLogin {
    crab: Octocrab,
    codes: octocrab::auth::DeviceCodes,
    client_id: SecretString,
}

impl DeviceLogin {
    pub async fn start(client_id: &str) -> Result<Self> {
        let crab = Octocrab::builder()
            .base_uri("https://github.com")
            .map_err(|e| anyhow!("{e}"))?
            .add_header(http::header::ACCEPT, "application/json".to_string())
            .build()
            .map_err(|e| anyhow!("{e}"))?;
        let client_id = SecretString::from(client_id.to_string());
        let codes = crab.authenticate_as_device(&client_id, SCOPES).await.map_err(explain)?;
        Ok(Self { crab, codes, client_id })
    }

    pub fn user_code(&self) -> &str {
        &self.codes.user_code
    }

    pub fn verification_uri(&self) -> &str {
        &self.codes.verification_uri
    }

    /// Polls at the server-mandated interval (honouring `slow_down`) until approved or expired.
    pub async fn wait(self) -> Result<String> {
        let expiry = std::time::Duration::from_secs(self.codes.expires_in.max(60));
        let auth = tokio::time::timeout(expiry, self.codes.poll_until_available(&self.crab, &self.client_id))
            .await
            .map_err(|_| anyhow!("The sign-in code expired. Start again."))?
            .map_err(|e| anyhow!("Sign-in failed: {e}"))?;
        Ok(auth.access_token.expose_secret().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_list() {
        let mut a = Accounts::parse(None);
        assert_eq!(a.active(), None);
        a.activate("octocat");
        a.activate("work-me");
        assert_eq!(a.active(), Some("work-me"));
        // Re-selecting moves it to the front without duplicating; case-insensitive.
        a.activate("OctoCat");
        assert_eq!(a.logins, vec!["OctoCat", "work-me"]);
        assert_eq!(a.find("octocat"), Some("OctoCat"));
        let round = Accounts::parse(Some(&a.to_json()));
        assert_eq!(round, a);
        a.remove("OCTOCAT");
        assert_eq!(a.active(), Some("work-me"));
        assert_eq!(Accounts::parse(Some("not json")), Accounts::default());
    }

    #[test]
    fn notification_urls() {
        let repo = "https://github.com/o/r";
        assert_eq!(
            subject_html_url(Some("https://api.github.com/repos/o/r/pulls/12"), repo),
            "https://github.com/o/r/pull/12"
        );
        assert_eq!(
            subject_html_url(Some("https://api.github.com/repos/o/r/issues/3"), repo),
            "https://github.com/o/r/issues/3"
        );
        assert_eq!(subject_html_url(None, repo), repo);
        assert_eq!(subject_html_url(Some("https://example.com/x"), repo), repo);
    }

    #[test]
    fn repo_json_decodes_with_missing_fields() {
        let r: Repo = serde_json::from_str(
            r#"{"full_name":"o/r","clone_url":"https://github.com/o/r.git","html_url":"https://github.com/o/r","description":null}"#,
        )
        .unwrap();
        assert_eq!(r.full_name, "o/r");
        assert!(!r.private && r.description.is_none());
    }
}
