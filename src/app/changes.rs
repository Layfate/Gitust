//! The Changes tab: diffs, line selection and staging, discard, commit, conflicts.

use super::*;

impl App {
    pub(super) fn push_diff(&self, p: DiffPayload, loading: bool) {
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
                let lines: Vec<DiffLine> = p.lines.into_iter().map(Row::into_line).collect();
                st.set_diff(ModelRc::new(VecModel::from(lines)));
            }
        });
    }

    /// Runs `f` on the UI thread's current diff model. Call from the UI thread.
    pub(super) fn with_diff_model(&self, f: impl FnOnce(&AppState<'_>, &ModelRc<DiffLine>)) {
        if let Some(ui) = self.ui.upgrade() {
            let st = ui.global::<AppState>();
            let model = st.get_diff();
            f(&st, &model);
        }
    }

    /// Click on a +/- row: toggle it; with Shift, set the range from the last
    /// clicked row to the state that row has.
    pub(super) fn toggle_diff_line(&self, row: i32, shift: bool) {
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
    pub(super) fn hunk_rows(&self, header_row: i32) -> (String, Vec<usize>) {
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
    pub(super) fn picked_rows(&self) -> (String, Vec<usize>) {
        let mut rows = Vec::new();
        let mut key = String::new();
        self.with_diff_model(|st, model| {
            key = st.get_diff_select_key().to_string();
            rows = model.iter().enumerate().filter(|(_, r)| r.selected).map(|(i, _)| i).collect();
        });
        (key, rows)
    }

    pub(super) fn stage_hunk(self: &Arc<Self>, header_row: i32) {
        let (key, rows) = self.hunk_rows(header_row);
        self.stage_rows(key, rows);
    }

    pub(super) fn stage_selected_lines(self: &Arc<Self>) {
        let (key, rows) = self.picked_rows();
        self.stage_rows(key, rows);
    }

    /// Asks first: discarded lines can't be recovered.
    pub(super) fn confirm_discard_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
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

    pub(super) fn discard_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
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
    pub(super) fn stage_rows(self: &Arc<Self>, key: String, rows: Vec<usize>) {
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
    pub(super) fn step_change(self: &Arc<Self>, delta: i32) -> i32 {
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
    pub(super) fn step_commit(self: &Arc<Self>, delta: i32) -> i32 {
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

    pub(super) fn select_change(self: &Arc<Self>, path: &str, staged: bool) {
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

    pub(super) fn load_file_diff(self: &Arc<Self>, change: FileChange, spinner: bool) {
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

    pub(super) fn select_commit(self: &Arc<Self>, id: String) {
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
}

pub(super) fn change_rows(sum: &git::Summary) -> Vec<ChangeRow> {
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
