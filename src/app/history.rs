//! History (with the branch graph) and the branch list.

use super::*;

impl App {
    pub(super) fn load_history(self: &Arc<Self>, reset: bool) {
        let Some(root) = self.repo() else { return };
        let (gen_, skip) = {
            let mut s = self.st();
            if reset {
                s.history_gen += 1;
                s.commits_exhausted = false;
                s.commits_loading = true;
                (s.history_gen, 0)
            } else {
                if s.commits_loading || s.commits_exhausted {
                    return;
                }
                s.commits_loading = true;
                (s.history_gen, s.commits.len())
            }
        };
        self.ui(|_, st| st.set_commits_loading(true));
        let app = self.clone();
        self.spawn(async move {
            let res = blocking(move || git::history(&git::open(&root)?, skip, HISTORY_PAGE)).await;
            {
                let mut s = app.st();
                if s.history_gen != gen_ {
                    return;
                }
                s.commits_loading = false;
                match &res {
                    Ok(page) => {
                        s.commits_exhausted = page.len() < HISTORY_PAGE;
                        if skip == 0 {
                            s.commits.clear();
                        }
                    }
                    Err(_) => s.commits_exhausted = true,
                }
                if let Ok(page) = &res {
                    s.commits.extend(page.iter().cloned());
                }
                let exhausted = s.commits_exhausted;
                app.ui(move |_, st| {
                    st.set_commits_loading(false);
                    st.set_commits_exhausted(exhausted);
                });
            }
            match res {
                Ok(_) => app.push_commits(),
                Err(e) => app.fail(e),
            }
        });
    }

    pub(super) fn push_commits(&self) {
        let now = util::now();
        let items: Vec<(git::CommitInfo, i32, GraphPaths)> = {
            let s = self.st();
            // The graph only makes sense for the unfiltered list.
            let (graph, lanes) = if s.history_query.trim().is_empty() {
                let input: Vec<(&str, Vec<&str>)> =
                    s.commits.iter().map(|c| (c.id.as_str(), c.parents.iter().map(String::as_str).collect())).collect();
                let (rows, width) = crate::graph::layout(&input);
                (rows, width.min(MAX_GRAPH_LANES))
            } else {
                (Vec::new(), 0)
            };
            let lanes_i = lanes as i32;
            let items = s
                .commits
                .iter()
                .enumerate()
                .filter(|(_, c)| commit_matches(c, &s.history_query))
                .map(|(i, c)| {
                    let (node, paths) = match graph.get(i) {
                        Some(row) => (row.node as i32, graph_paths(row, lanes)),
                        None => (0, Vec::new()),
                    };
                    (c.clone(), node, paths)
                })
                .collect();
            self.ui(move |_, st| st.set_graph_lanes(lanes_i));
            items
        };
        self.ui(move |_, st| {
            let items: Vec<CommitItem> = items
                .into_iter()
                .map(|(c, node, paths)| {
                    let mut item = commit_item(&c, now, node);
                    item.paths = ModelRc::new(VecModel::from(
                        paths.into_iter().map(|(d, color)| GraphPath { d: d.into(), color }).collect::<Vec<_>>(),
                    ));
                    item
                })
                .collect();
            // Rows hold models (compared by identity), so replace the data in place.
            set_model!(st, get_commits, set_commits, items)
        });
    }

    pub(super) fn load_branches(self: &Arc<Self>) {
        let Some(root) = self.repo() else { return };
        let app = self.clone();
        self.spawn(async move {
            match blocking(move || {
                let repo = git::open(&root)?;
                Ok((git::branches(&repo)?, git::tags(&repo)?))
            })
            .await
            {
                Ok((b, t)) => {
                    {
                        let mut s = app.st();
                        s.branches = b;
                        s.tags = t;
                    }
                    app.push_branches();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub(super) fn push_branches(&self) {
        let items: Vec<BranchItem> = {
            let s = self.st();
            let q = s.branch_query.trim().to_lowercase();
            let now = util::now();
            s.branches
                .iter()
                .filter(|b| q.is_empty() || b.name.to_lowercase().contains(&q))
                .map(|b| BranchItem {
                    name: sh(b.name.clone()),
                    is_head: b.is_head,
                    is_remote: b.is_remote,
                    is_tag: false,
                    upstream: sh(b.upstream.clone().unwrap_or_default()),
                    ahead: b.ahead as i32,
                    behind: b.behind as i32,
                    when: sh(if b.time > 0 { util::relative_time(b.time, now) } else { String::new() }),
                    summary: sh(b.summary.clone()),
                })
                .chain(s.tags.iter().filter(|t| q.is_empty() || t.name.to_lowercase().contains(&q)).map(|t| {
                    BranchItem {
                        name: sh(t.name.clone()),
                        is_tag: true,
                        upstream: sh(t.target.clone()),
                        when: sh(util::relative_time(t.time, now)),
                        summary: sh(t.summary.clone()),
                        ..Default::default()
                    }
                }))
                .collect()
        };
        self.ui(move |_, st| set_model!(st, get_branches, set_branches, items));
    }
}

/// Lanes beyond this are clipped; very wide graphs are rare and unreadable anyway.
pub(super) const MAX_GRAPH_LANES: usize = 8;

/// Must match the History row: lane width × row height, in logical px.
pub(super) const GRAPH_LANE_W: f32 = 14.0;

pub(super) const GRAPH_ROW_H: f32 = 56.0;

/// SVG path commands and lane colour, one entry per colour in a row.
pub(super) type GraphPaths = Vec<(String, i32)>;

/// One SVG path per colour for a row's graph edges.
pub(super) fn graph_paths(row: &crate::graph::Row, lanes: usize) -> GraphPaths {
    let mut colors: Vec<u16> = row.edges.iter().map(|e| e.color).collect();
    colors.sort_unstable();
    colors.dedup();
    colors
        .into_iter()
        .filter_map(|color| {
            let edges: Vec<&crate::graph::Edge> = row
                .edges
                .iter()
                .filter(|e| e.color == color && (e.from as usize) < lanes && (e.to as usize) < lanes)
                .collect();
            (!edges.is_empty()).then(|| (crate::graph::path(&edges, GRAPH_LANE_W, GRAPH_ROW_H), color as i32))
        })
        .collect()
}

pub(super) fn commit_item(c: &git::CommitInfo, now: i64, node: i32) -> CommitItem {
    CommitItem {
        id: sh(c.id.clone()),
        short_id: sh(c.id[..7.min(c.id.len())].to_string()),
        summary: sh(c.summary.clone()),
        author: sh(c.author.clone()),
        when: sh(util::relative_time(c.time, now)),
        refs: sh(match c.refs.len() {
            0 => String::new(),
            1 | 2 => c.refs.join(", "),
            n => format!("{}, {} +{}", c.refs[0], c.refs[1], n - 2),
        }),
        unpushed: c.unpushed,
        merge: c.merge,
        node,
        paths: ModelRc::default(),
    }
}

pub(super) fn commit_matches(c: &git::CommitInfo, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || c.summary.to_lowercase().contains(&q)
        || c.author.to_lowercase().contains(&q)
        || c.id.starts_with(&q)
}
