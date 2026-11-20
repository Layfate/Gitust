//! Runs the user's installed `git` for anything that touches the network or
//! should honour hooks, filters and signing. Arguments are always an explicit
//! argv — never a shell string — and paths travel over stdin, NUL-separated.

use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}
impl std::error::Error for Cancelled {}

#[derive(Debug, Default)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default, Clone)]
pub struct Opts {
    pub stdin: Option<Vec<u8>>,
    pub literal_pathspecs: bool,
    pub cancel: Option<Arc<Notify>>,
    pub progress: Option<Progress>,
    pub timeout: Option<Duration>,
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `Command` for a non-interactive helper: no console window on Windows, no TTY prompts.
pub fn tool(program: &str) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

pub async fn git(cwd: Option<&Path>, args: &[String], opts: Opts) -> Result<Output> {
    let mut cmd = tool("git");
    cmd.args(args)
        .stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        // Never block on a terminal prompt we can't show; GUI credential managers still work.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "auto")
        // Never open an editor nobody can see (also overrides a repo's core.editor);
        // Git treats ":" as "keep the prepared message".
        .env("GIT_EDITOR", ":")
        .env("GIT_SEQUENCE_EDITOR", ":");
    if opts.literal_pathspecs {
        cmd.env("GIT_LITERAL_PATHSPECS", "1");
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("Git isn't installed or isn't on PATH. Install it from https://git-scm.com and restart Gitust.")
        } else {
            anyhow!("Couldn't start git: {e}")
        }
    })?;

    let stdin = child.stdin.take();
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let input = opts.stdin;
    let progress = opts.progress;

    let work = async move {
        let write = async move {
            if let (Some(mut pipe), Some(bytes)) = (stdin, input) {
                let _ = pipe.write_all(&bytes).await;
                let _ = pipe.shutdown().await;
            }
        };
        let read_out = async {
            let mut v = Vec::new();
            let _ = out.read_to_end(&mut v).await;
            v
        };
        let read_err = async {
            let mut all = Vec::new();
            let mut buf = [0u8; 4096];
            let mut line = Vec::new();
            let mut last = Instant::now() - Duration::from_secs(1);
            loop {
                let n = match err.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                all.extend_from_slice(&buf[..n]);
                if let Some(p) = &progress {
                    // git redraws progress with '\r'; surface the newest line, throttled.
                    for &b in &buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            if !line.is_empty() && last.elapsed() >= Duration::from_millis(80) {
                                p(String::from_utf8_lossy(&line).trim());
                                last = Instant::now();
                            }
                            line.clear();
                        } else {
                            line.push(b);
                        }
                    }
                }
            }
            all
        };
        let ((), o, e) = tokio::join!(write, read_out, read_err);
        let status = child.wait().await;
        (o, e, status)
    };

    let cancel = opts.cancel;
    let timeout = opts.timeout;
    let (o, e, status) = tokio::select! {
        r = work => r,
        _ = async { match &cancel { Some(n) => n.notified().await, None => std::future::pending().await } } => {
            return Err(Cancelled.into()); // dropping `work` drops the child → killed
        }
        _ = async { match timeout { Some(t) => tokio::time::sleep(t).await, None => std::future::pending().await } } => {
            return Err(anyhow!("git {} timed out", args.first().map(String::as_str).unwrap_or("")));
        }
    };
    let status = status?;
    Ok(Output {
        ok: status.success(),
        stdout: String::from_utf8_lossy(&o).into_owned(),
        stderr: String::from_utf8_lossy(&e).into_owned(),
    })
}

/// NUL-separated pathspec list for `--pathspec-from-file=- --pathspec-file-nul`.
pub fn nul_paths<S: AsRef<str>>(paths: &[S]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in paths {
        v.extend_from_slice(p.as_ref().as_bytes());
        v.push(0);
    }
    v
}

/// Percent from git progress lines such as `Receiving objects:  45% (450/1000)`.
pub fn parse_percent(line: &str) -> Option<f32> {
    let idx = line.find('%')?;
    let digits: String = line[..idx].chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    let n: f32 = digits.chars().rev().collect::<String>().parse().ok()?;
    Some((n / 100.0).clamp(0.0, 1.0))
}

/// Turns raw git stderr into a sentence a person can act on.
pub fn explain(stderr: &str) -> String {
    let s = stderr.to_lowercase();
    let has = |needle: &str| s.contains(needle);
    let hint = if has("authentication failed")
        || has("could not read username")
        || has("terminal prompts disabled")
        || has("permission denied (publickey)")
        || has("invalid username or password")
    {
        Some(
            "Git couldn't authenticate with the remote. Sign in with Git Credential Manager (bundled with Git for Windows), run `gh auth setup-git`, or load your SSH key into the agent.",
        )
    } else if has("could not resolve host")
        || has("unable to access") && has("timed out")
        || has("network is unreachable")
    {
        Some("Can't reach the remote — check your network connection.")
    } else if has("[rejected]") || has("non-fast-forward") || has("fetch first") {
        Some("The remote has commits you don't have yet. Pull first, then push again.")
    } else if has("need to specify how to reconcile divergent branches") {
        Some(
            "Your branch and the remote have diverged. Choose a strategy once with `git config --global pull.rebase false` (merge) or `true` (rebase), then pull again.",
        )
    } else if has("conflict") && (has("automatic merge failed") || has("could not apply") || has("merge conflict")) {
        Some("Merge conflicts. Resolve the conflicted files in the Changes tab, stage them, then commit.")
    } else if has("would be overwritten by") || has("please commit your changes or stash them") {
        Some("Your local changes would be overwritten. Commit or stash them first.")
    } else if has("not something we can merge") {
        Some("That branch can't be merged — fetch first and try again.")
    } else if has("is not fully merged") {
        Some("This branch has commits that aren't merged anywhere. Merge it first if you want to keep them.")
    } else if has("already exists") && has("destination path") {
        Some("The destination folder already exists and isn't empty.")
    } else if has("repository not found") || has("does not appear to be a git repository") {
        Some("Repository not found — check the URL and that you have access.")
    } else if has("nothing to commit") {
        Some("Nothing to commit.")
    } else if has("no stash entries found") {
        Some("There's nothing stashed.")
    } else if has("index.lock") {
        Some("Another Git process is using this repository. Wait for it to finish (or remove a stale .git/index.lock).")
    } else if has("please tell me who you are") || has("author identity unknown") {
        Some(
            "Git doesn't know who you are yet. Run `git config --global user.name \"Your Name\"` and `git config --global user.email you@example.com`.",
        )
    } else {
        None
    };
    match hint {
        Some(h) => h.to_string(),
        None => stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("hint:"))
            .unwrap_or("Git failed without a message")
            .trim_start_matches("fatal: ")
            .trim_start_matches("error: ")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent() {
        assert_eq!(parse_percent("Receiving objects:  45% (450/1000), 1.2 MiB"), Some(0.45));
        assert_eq!(parse_percent("Counting objects: 100% (3/3), done."), Some(1.0));
        assert_eq!(parse_percent("remote: Enumerating objects"), None);
    }

    #[test]
    fn explanations() {
        assert!(explain("fatal: Authentication failed for 'https://x'").contains("authenticate"));
        assert!(explain(" ! [rejected] main -> main (fetch first)").contains("Pull first"));
        assert_eq!(explain("fatal: something odd\n"), "something odd");
        assert!(explain("CONFLICT (content): Merge conflict in a.txt\nAutomatic merge failed").contains("conflict"));
    }

    #[test]
    fn nul() {
        assert_eq!(nul_paths(&["a", "b c"]), b"a\0b c\0".to_vec());
    }

    #[tokio::test]
    async fn runs_git_and_streams_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = git(Some(dir.path()), &["init".into(), "-q".into()], Opts::default()).await.unwrap();
        assert!(out.ok, "{}", out.stderr);
        std::fs::write(dir.path().join("x y.txt"), "hi").unwrap();
        let add = git(
            Some(dir.path()),
            &["add".into(), "-A".into(), "--pathspec-from-file=-".into(), "--pathspec-file-nul".into()],
            Opts { stdin: Some(nul_paths(&["x y.txt"])), literal_pathspecs: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(add.ok, "{}", add.stderr);
        let st = git(Some(dir.path()), &["diff".into(), "--cached".into(), "--name-only".into()], Opts::default())
            .await
            .unwrap();
        assert_eq!(st.stdout.trim(), "x y.txt");
    }

    #[tokio::test]
    async fn cancellation_kills_child() {
        let n = Arc::new(Notify::new());
        n.notify_one();
        let r = git(None, &["--version".into()], Opts { cancel: Some(n), ..Default::default() }).await;
        // Either the tiny command won the race or we cancelled; neither may hang.
        if let Err(e) = r {
            assert!(e.is::<Cancelled>());
        }
    }
}
