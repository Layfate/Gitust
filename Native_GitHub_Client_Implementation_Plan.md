# Native GitHub Client — Implementation Plan

**Working title:** Native GitHub Client (name to be chosen)  
**Product:** Lightweight, cross-platform, native desktop Git and GitHub application  
**Platforms:** Windows first; Linux and macOS after the MVP is stable  
**Design principle:** Native Rust desktop app, no Electron/browser shell, local-first Git workflow, optional GitHub connectivity

> Version-independent architecture plan. Verify current library compatibility, GitHub API details, authentication policies, and licensing before pinning dependencies.

## 1. Product vision

Build a small, fast GitHub desktop client that works offline for local Git operations, then enriches them with GitHub repositories, issues, pull requests, and notifications when the user signs in.

**User promise:** Open a repository, see what changed, stage and commit, switch branches, and push/pull without opening a browser or a heavyweight desktop client.

### Differentiators
- **Native interface:** Rust + Slint, with no Electron or Node runtime.
- **Local-first:** Git functions without a GitHub account or network connection.
- **Fast startup:** Render the window immediately, then load repository state asynchronously.
- **Low idle footprint:** Minimal background work and no automatic aggressive polling.
- **Keyboard-first:** Command palette, searchable lists, configurable shortcuts.
- **Safe defaults:** Never discard work or force-push without explicit confirmation.
- **GitHub integration:** Add the remote workflow incrementally; do not block local Git functionality on API access.

## 2. Scope and delivery sequence

### MVP (first usable release)
1. Open an existing local Git repository; list recent repositories.
2. Show branch name, worktree status, staged/unstaged/untracked files.
3. View text diffs with large-file safeguards.
4. Stage/unstage individual files and commit with message validation.
5. View history, commit details, and branches; create/switch branches safely.
6. Fetch, pull, push through installed Git, respecting existing credential helpers.
7. GitHub sign-in (optional) and remote repository list.
8. Clone GitHub repositories with a progress view.
9. Settings, theme, keyboard shortcuts, errors, and update information.

### V1 (after stable MVP)
- GitHub issues, pull requests, reviews, and notifications.
- PR diff browsing, create PR from branch, open discussion in browser.
- Multiple GitHub accounts and per-repository account selection.
- Stash, tags, worktrees, conflict assistance, and ahead/behind details.
- Repo-wide search and richer filters.

### Later / optional
- Integrated terminal, Git LFS assistance, submodules, commit signing configuration.
- Built-in three-way merge editor and GitHub Actions status.
- GitLab/Forgejo support through provider adapters.
- Optional background notifications and auto-update.

**Explicitly out of MVP:** Hosting a Git server; replacing every advanced Git CLI feature; proprietary GitHub code execution; embedded full web browser.

## 3. Technology choices

| Layer | Primary choice | Why |
|---|---|---|
| Language | Rust (stable toolchain) | Performance, safe concurrency, portability |
| UI | Slint | Compiled declarative desktop UI; no webview |
| Async work | Tokio | Remote I/O, task cancellation, bounded concurrency |
| Local Git read operations | git2 | Status, history, diffs, branches, index operations |
| Remote Git operations | Git CLI via `std::process::Command`/`tokio::process` | Reuses users' SSH agents and established Git credential helpers; never interpolate commands into a shell |
| GitHub API | Octocrab + Reqwest (where needed) | Typed API use with an escape hatch for unsupported endpoints |
| Persistence | SQLite + SQLx or rusqlite | Recent repositories, preferences, metadata, cache |
| Secrets | OS credential vault via a maintained keyring library | Keep tokens out of SQLite, logs, and plain config files |
| Serialization | serde | Typed settings and API data |
| Error handling | thiserror + anyhow (at application boundary) | Structured errors and actionable UI messages |
| Logging | tracing + tracing-subscriber | Opt-in diagnostics with secret redaction |
| File watching | notify | Debounced status refresh |
| Testing | cargo test + integration tests | Unit, repository fixture, UI and end-to-end validation |
| Packaging | Per-OS native installers | Windows installer first; Linux packages; signed macOS app when credentials are available |

**Architecture nuance:** The application is native, but `git2` wraps libgit2, and Git CLI remote operations require Git installed. “No Electron” is a clear claim; “entirely pure Rust, zero external dependencies” is not.

**Alternatives:** Explore `gix` (gitoxide) if a pure-Rust Git backend becomes a requirement. Assess its feature coverage before replacing a tested backend. Check Slint's applicable license terms before commercial redistribution.

## 4. Architecture

Use one desktop process with a thin Slint presentation layer and independent Rust services.

```text
Slint UI (windows, lists, dialogs, command palette)
          |
          v
UI Controller / Application State
          |
          +--> GitService --------> git2 (local read/index/commit)
          |                      \-> Git CLI (fetch/pull/push/clone)
          +--> GitHubService -----> Octocrab / HTTPS
          +--> AuthService -------> OAuth device flow + OS credential vault
          +--> RepositoryService -> repo catalog + filesystem watcher
          +--> SettingsService --> SQLite / configuration
          +--> CacheService -----> SQLite, TTL and invalidation
          +--> TaskManager ------> Tokio tasks + cancellation + progress events
```

### Architecture rules
1. **No blocking work on the UI thread.** Git walks, network requests, DB queries, and diff generation run in workers.
2. **Slint is a view, not the business-logic layer.** Expose typed commands and state snapshots.
3. **GitHub is optional.** Opening a local repo does not trigger login or external calls.
4. **Provider abstraction.** Define `HostingProvider` to make later GitLab/Forgejo support possible.
5. **Git backend abstraction.** Keep Git process execution isolated behind `GitService`.
6. **All mutations are serialized per repository.** Prevent simultaneous branch switch, commit, pull, or checkout conflicts.
7. **Never execute a user-influenced command string through a shell.** Pass an explicit argv vector to the Git executable.
8. **UI only receives safe event messages.** Worker results must not mutate views directly.

## 5. Suggested repository structure

```text
native-github-client/
├── Cargo.toml                       # Workspace
├── Cargo.lock
├── README.md
├── LICENSE                          # Select before distribution
├── rust-toolchain.toml
├── .github/workflows/
│   ├── ci.yml
│   └── release.yml
├── apps/
│   └── desktop/
│       ├── Cargo.toml
│       ├── build.rs
│       ├── src/
│       │   ├── main.rs
│       │   ├── app.rs
│       │   ├── commands.rs
│       │   └── ui_bridge.rs
│       ├── ui/
│       │   ├── main-window.slint
│       │   ├── components/
│       │   │   ├── sidebar.slint
│       │   │   ├── repo-header.slint
│       │   │   ├── file-list.slint
│       │   │   ├── diff-view.slint
│       │   │   ├── commit-dialog.slint
│       │   │   └── command-palette.slint
│       │   └── dialogs/
│       └── assets/
├── crates/
│   ├── app-core/                    # Domain models, commands, service interfaces
│   ├── git-service/                 # git2 + Git CLI adapter
│   ├── github-service/              # REST/GraphQL, pagination, caching
│   ├── auth-service/                # OAuth and credential vault
│   ├── repository-service/          # Recent repos, watcher, coordinator
│   ├── persistence/                 # SQLite migrations, settings and caches
│   └── diagnostics/                 # logging, metrics, redaction
├── migrations/
├── tests/
│   ├── fixtures/
│   ├── git_workflows.rs
│   ├── auth_flow.rs
│   └── provider_contracts.rs
├── scripts/
└── docs/
    ├── ARCHITECTURE.md
    ├── SECURITY.md
    ├── PERFORMANCE.md
    └── CONTRIBUTING.md
```

Avoid making every feature its own crate in the very first commit; split modules into crates when interfaces stabilize.

## 6. Domain model and commands

### Core models
- `RepositoryId`, `RepositoryInfo`, `RepoPath`, `RemoteInfo`
- `BranchInfo`, `CommitInfo`, `CommitDiff`, `FileChange`, `FileStatus`
- `GitOperation`, `OperationProgress`, `OperationError`
- `AccountId`, `GithubAccount`, `IssueSummary`, `PullRequestSummary`
- `AppSettings`, `Theme`, `RepositoryPreferences`

### Typed UI commands
- `OpenRepository(path)`
- `RefreshStatus(repo_id)`
- `StageFiles(repo_id, paths)` / `UnstageFiles(...)`
- `Commit(repo_id, message)`
- `CheckoutBranch(repo_id, branch)` / `CreateBranch(...)`
- `Fetch(repo_id)` / `Pull(repo_id)` / `Push(repo_id)`
- `Clone(url, destination)`
- `SignInGithub()` / `SignOutGithub(account_id)`
- `LoadRepositories(account_id, page)`

All long-running commands return an operation ID and update UI progress asynchronously. Support cancellation where safe; do not suggest that a partially completed Git mutation can always be rolled back.

## 7. User interface specification

### Main window
```text
+------------------------------------------------------------------+
|  App  File  Repository  View           [Search / Command]  _ □ X |
+----------------+---------------------------------+---------------+
| RECENT REPOS   | Project Name / main             | DETAILS       |
|  project-a     | [Fetch] [Pull] [Push]           | Selected file |
|  project-b     +---------------------------------+               |
|                | Changes | History | Branches     | Diff preview  |
| GITHUB         +---------------------------------+               |
|  Repositories  | M src/main.rs                   | - old         |
|  Issues        | A src/utils.rs                  | + new         |
|  Pull Requests | ?? README.md                    |               |
|  Notifications |                                 |               |
+----------------+---------------------------------+---------------+
|  Commit message: [________________________________] [Commit]      |
+------------------------------------------------------------------+
|  Status: 3 changed files                Git: idle   Offline: no   |
+------------------------------------------------------------------+
```

### UX requirements
- Navigation: **Repositories, Changes, History, Branches, GitHub**.
- Dark and light themes; system default; keyboard shortcuts.
- Immediate loading skeletons and empty states; never freeze while scanning repos.
- Search/filter file changes and history.
- Show local/remote ahead-behind indicators when known; distinguish stale from refreshed.
- Display reason and recovery action for auth, network, merge, and Git errors.
- Accessible contrast, keyboard focus, screen-reader labels where supported by UI toolkit.
- Restore last-opened local repository and window layout on launch.

## 8. Git workflow implementation

### Repository scanning
- Validate selected path and detect repo/worktree using Git libraries.
- Load HEAD and branch, status, remotes, first page of history.
- Watch `.git` metadata and worktree changes; debounce events (e.g., 150–300 ms).
- Ignore large generated directories only where Git rules or configured exclusions warrant it.
- For huge repositories, avoid a full history traversal or loading all diffs on initial open.

### Changes and commits
- Separate staged, unstaged, and untracked groups.
- Start with file-level staging; postpone hunk staging until index/diff handling is thoroughly tested.
- Show binary-file markers; cap rendered diffs and allow explicit deeper loading.
- Commit through GitService, preserving Git config identity and respecting hooks/signing strategy.
- Prompt before checkout when uncommitted changes could be lost.

### Remote operations
- Use an installed Git executable for fetch, clone, pull, push, at least initially.
- Inherit supported credential-helper and SSH-agent behavior; provide actionable install/auth errors.
- Do not assume GitHub API login automatically authenticates Git remotes.
- Stream sanitized stdout/stderr progress and support cancellation.
- Never use silent forced pushes or automatic hard resets.
- For conflicts, show affected paths and suggested next steps; support opening the repo in a terminal or editor.

## 9. GitHub auth and API

### Sign-in
1. Register a GitHub OAuth app if using OAuth device authorization.
2. Present an **Open GitHub verification page** action and user code.
3. Poll the token endpoint according to server-provided intervals and handle `slow_down`, expiry, and cancellation.
4. Request only permissions needed for enabled GitHub features; clearly explain scopes.
5. Store access token in OS credential storage, keyed by account ID.
6. Store account label and non-secret metadata in SQLite.
7. On logout, delete local credentials and optionally support revocation guidance.

Support a user-provided fine-grained personal access token as an optional advanced fallback; never include tokens in logs, command arguments, screenshots, or error text.

### API behavior
- Use conditional requests/ETags where supported and cache metadata with TTLs.
- Respect GitHub primary and secondary API rate limits; show actionable retry times.
- Use pagination; don't load every repository, issue, or PR on startup.
- Run background refresh only when relevant views are visible or when the user enables polling.
- Handle revoked tokens, missing scopes, inaccessible organizations, and offline mode.
- Separate GitHub issues/PR permissions from local Git repository access.

## 10. Local persistence

Recommended tables:
- `recent_repositories(id, canonical_path, last_opened_at, pinned)`
- `accounts(id, provider, username, avatar_cache_key, last_used_at)` — **no secrets**
- `repository_preferences(repo_id, preferred_account_id, selected_branch, sort_order)`
- `github_cache(key, provider, etag, payload, fetched_at, expires_at)`
- `app_settings(key, value)`
- `schema_migrations(...)`

Use migrations and schema version checks. Cache only data needed for fast UI, with size limits and deletion controls. Do not duplicate full repository history into SQLite during MVP.

## 11. Performance budget (targets to validate, not promises)

Test on a specified mid-range Windows machine and record hardware/software conditions.

| Measure | Initial target |
|---|---|
| Cold app launch to interactive window | < 1.0 s on SSD |
| Idle private memory, one small repo open | < 120 MB |
| Idle CPU (no active operations) | ~0% sustained |
| Main-thread UI responsiveness | No operation blocks the UI > 16–33 ms |
| Status refresh after a small file change | Visible within 500 ms |
| Large history/diff views | Incremental loading; bounded memory |
| Background network refresh | Off by default or very infrequent, user-configurable |

**Measurement:** Record cold/warm startup time, peak RSS/private bytes, CPU idle, repo-open latency, status duration, UI frame stalls, clone throughput, and network call count. Benchmark with small, medium, and large test repositories on Windows, then Linux/macOS. Compare to GitHub Desktop only using the same machine, repository, and test steps.

## 12. Security and reliability

- Keep GitHub tokens in OS credential storage; scrub secrets from diagnostics and crash reports.
- Use HTTPS/TLS verification; never disable certificate checking to fix errors.
- Use structured process arguments instead of shell-string execution; constrain executable discovery.
- Consider repositories untrusted. Git hooks, filters, submodules, external tools, and URLs can execute or contact external systems; disclose and respect user configuration.
- Confirm destructive actions; provide clear diff/target/branch before changes.
- Validate clone destinations and path traversal/symlink edge cases.
- Lock repository mutations per repo and report concurrent external Git activity.
- Never cache private issue/PR contents without a clear retention/delete policy.
- Add timeouts, retries with jitter, request cancellation, and explicit offline states.
- Produce reproducible builds where feasible, publish checksums, and sign releases where credentials permit.
- Evaluate dependency licenses and vulnerability advisories during CI.

## 13. Testing and quality gates

### Unit tests
- Path parsing, status transformation, diff limits, branch state, URL parsing, caches, auth state transitions, error formatting.

### Git integration fixtures
- New repo, detached HEAD, no upstream, dirty tree, merge conflict, shallow clone, renamed files, binary changes, Unicode paths, large files, worktrees, bare repo, SSH remote failures.
- Authenticated and unauthenticated remote Git operations.

### API contract tests
- Pagination, HTTP 401/403/404/429, rate limits, expired credentials, ETags, missing scopes, network interruptions.
- Use recorded fixtures/mock servers; do not put real tokens in CI.

### Release gates
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- Windows build and smoke test required; Linux/macOS builds before claiming support.
- No critical security findings in dependencies.
- No data-loss bugs in stage/commit/branch flows.
- Installer opens and works without a Node/browser runtime.

## 14. Development milestones

### Phase 0 — Foundation
**Build:** Cargo workspace, Slint app shell, navigation, theme tokens, logging, error system, CI, test scaffolding.  
**Done when:** App opens with a nonblocking main window and can be packaged locally.

### Phase 1 — Offline Git MVP
**Build:** Open repository, recent repos, status, file list, diffs, stage/unstage, commit, branches, commit history.  
**Done when:** A developer can complete a normal local commit without using the terminal.

### Phase 2 — Remote Git
**Build:** Credential-helper integration through Git CLI; clone, fetch, pull, push; progress/cancellation and conflict guidance.  
**Done when:** The full clone → edit → commit → push workflow is reliable across HTTPS and SSH test fixtures.

### Phase 3 — GitHub account integration
**Build:** Device sign-in, account UI, repository browsing, token vault, API caching, offline/rate-limit states.  
**Done when:** User can sign in, browse available GitHub repositories, and clone one into the local Git workflow.

### Phase 4 — Polish and beta release
**Build:** Command palette, settings, keyboard support, diagnostics, performance optimization, Windows installer, docs.  
**Done when:** MVP acceptance tests, performance tests, and installation smoke tests pass.

### Phase 5 — GitHub collaboration
**Build:** Issues, PR list/details, create PR, notifications, multi-account support.  
**Done when:** Users can review core GitHub collaboration activity from the desktop client.

### Phase 6 — Cross-platform and advanced Git
**Build:** Linux/macOS QA, packaging, signing where available, conflict helper, stash, tags, worktrees.  
**Done when:** Release artifacts have platform-specific tests and documented limitations.

## 15. MVP acceptance checklist

- [ ] Native window starts without Electron, webview, or Node runtime.
- [ ] Opening a local repo works with no login and no internet.
- [ ] Staged, unstaged, and untracked files are distinguished accurately.
- [ ] Users can inspect diffs, stage, unstage, and commit.
- [ ] Branch creation and checkout handle dirty-tree safety.
- [ ] Clone, fetch, pull, and push show progress and useful error messages.
- [ ] Auth supports GitHub device flow and stores no plaintext token.
- [ ] Repositories can be browsed via authenticated GitHub API.
- [ ] UI remains responsive while a large repo is scanned.
- [ ] Benchmark report states test hardware, repo sizes, and measured results.
- [ ] Windows installer and README enable another developer to run it.

## 16. First implementation sprint — exact build order

1. Initialize a Rust workspace and a single Slint window with sidebar, repo header, file list, and status bar.
2. Define `AppCommand`, `AppEvent`, `AppState` and the async event bridge to the UI.
3. Implement `GitService::open(path)`, `get_status()`, `get_branches()`, and `get_recent_commits(limit)` with git2.
4. Add file-change subscriptions with bounded/debounced refresh; ensure background tasks cannot block the UI thread.
5. Add read-only file diff; set text-size and row-count limits.
6. Add stage/unstage and commit with dirty-tree and index safety tests.
7. Add Git CLI command runner with safe argv, timeout, cancellation, and progress parsing.
8. Add fetch/pull/push/clone with error messages and conflict handling.
9. Add SQLite recent repositories, preferences, and schema migrations.
10. Integrate OAuth device flow and OS keyring; then GitHub remote repository list.
11. Benchmark, fix hotspots, package Windows preview build, and publish documented known limitations.

## 17. Success criteria and positioning

**Tagline:** *A tiny, native Git & GitHub desktop client built with Rust.*

Success is not “100% feature parity with GitHub Desktop.” Success is a reliable daily workflow with **measured** startup, memory, and responsiveness improvements, safe repository handling, and a visibly simpler interface. Avoid marketing absolute RAM or CPU claims until benchmarks exist.
