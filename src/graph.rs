//! Commit graph layout for the History list (like `git log --graph`).
//!
//! Each commit gets a lane; each row is drawn independently from a list of
//! edges, so the UI can virtualise rows. Pure and deterministic.

/// One line segment within a row. `top` edges run from the top of the row to
/// the node's height; bottom edges from the node's height to the bottom.
/// `from == to` is a straight line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: u16,
    pub to: u16,
    pub top: bool,
    /// Lane whose colour the edge takes.
    pub color: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub node: u16,
    pub edges: Vec<Edge>,
}

/// Lays out commits given newest-first as `(id, parent ids)`.
/// Returns one row per commit plus the number of lanes used.
pub fn layout<S: AsRef<str>>(commits: &[(S, Vec<S>)]) -> (Vec<Row>, usize) {
    // lanes[i]: the commit id lane i is waiting for (None = free).
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    let mut width = 0;

    for (id, parents) in commits {
        let id = id.as_ref();
        let mut row = Row::default();
        let waiting: Vec<usize> = (0..lanes.len()).filter(|&i| lanes[i].as_deref() == Some(id)).collect();
        let node = match waiting.first() {
            Some(&i) => i,
            None => match lanes.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        row.node = node as u16;

        // Top half: every occupied lane continues down; lanes waiting for this
        // commit converge into the node.
        for (i, lane) in lanes.iter().enumerate() {
            if lane.is_none() {
                continue;
            }
            let to = if waiting.contains(&i) { node } else { i };
            row.edges.push(Edge { from: i as u16, to: to as u16, top: true, color: i as u16 });
        }
        for &i in &waiting {
            lanes[i] = None;
        }

        // Bottom half: the first parent continues in the node's lane; others
        // join a lane already waiting for them, or open a new one.
        let parents: Vec<&str> = parents.iter().map(AsRef::as_ref).collect();
        if let Some(first) = parents.first() {
            match lanes.iter().position(|l| l.as_deref() == Some(*first)) {
                // A branch point further left: this lane ends by joining it.
                Some(j) if j < node => {
                    row.edges.push(Edge { from: node as u16, to: j as u16, top: false, color: j as u16 });
                }
                // Further right: fold that lane into this one, so the older
                // line keeps the lower lane (as `git log --graph` does).
                Some(j) if j > node => {
                    lanes[j] = None;
                    lanes[node] = Some(first.to_string());
                    row.edges.push(Edge { from: j as u16, to: node as u16, top: false, color: node as u16 });
                }
                _ => lanes[node] = Some(first.to_string()),
            }
        }
        for p in parents.iter().skip(1) {
            let j = match lanes.iter().position(|l| l.as_deref() == Some(*p)) {
                Some(j) => j,
                None => {
                    let j = match lanes.iter().position(Option::is_none) {
                        Some(j) => j,
                        None => {
                            lanes.push(None);
                            lanes.len() - 1
                        }
                    };
                    lanes[j] = Some(p.to_string());
                    j
                }
            };
            row.edges.push(Edge { from: node as u16, to: j as u16, top: false, color: j as u16 });
        }
        // Lanes still occupied continue to the bottom of the row.
        for (i, lane) in lanes.iter().enumerate() {
            // A lane just opened by a curve from the node has no straight line
            // above it; the node's own lane always continues from the node.
            let opened_here = row.edges.iter().any(|e| !e.top && e.to as usize == i && e.from as usize != i);
            if lane.is_some() && (i == node || !opened_here) {
                row.edges.push(Edge { from: i as u16, to: i as u16, top: false, color: i as u16 });
            }
        }
        // Trim free lanes at the end so the graph doesn't stay wide.
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        width = width.max(lanes.len()).max(node + 1);
        rows.push(row);
    }
    (rows, width)
}

/// SVG path commands for a row's edges in one colour, in a viewbox of
/// `lane_w × row_h` per lane. Lines are straight; lane changes are S-curves.
pub fn path(edges: &[&Edge], lane_w: f32, row_h: f32) -> String {
    let x = |lane: u16| lane as f32 * lane_w + lane_w / 2.0;
    let mid = row_h / 2.0;
    let mut d = String::new();
    for e in edges {
        let (y1, y2) = if e.top { (0.0, mid) } else { (mid, row_h) };
        let (x1, x2) = (x(e.from), x(e.to));
        if e.from == e.to {
            d.push_str(&format!("M {x1} {y1} L {x2} {y2} "));
        } else {
            let c = (y1 + y2) / 2.0;
            d.push_str(&format!("M {x1} {y1} C {x1} {c} {x2} {c} {x2} {y2} "));
        }
    }
    d.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c<'a>(id: &'a str, parents: &[&'a str]) -> (&'a str, Vec<&'a str>) {
        (id, parents.to_vec())
    }

    #[test]
    fn linear_history_is_one_lane() {
        let (rows, w) = layout(&[c("c", &["b"]), c("b", &["a"]), c("a", &[])]);
        assert_eq!(w, 1);
        assert!(rows.iter().all(|r| r.node == 0));
        // The root has no line below it; the tip has none above it.
        assert!(rows[0].edges.iter().all(|e| !e.top));
        assert!(rows[2].edges.iter().all(|e| e.top));
    }

    #[test]
    fn merge_opens_and_closes_a_lane() {
        // m merges f (feature) into b (main); both come from a.
        let (rows, w) = layout(&[c("m", &["b", "f"]), c("f", &["a"]), c("b", &["a"]), c("a", &[])]);
        assert_eq!(w, 2);
        assert_eq!(rows[0].node, 0);
        assert!(rows[0].edges.contains(&Edge { from: 0, to: 1, top: false, color: 1 }), "{:?}", rows[0]);
        assert_eq!(rows[1].node, 1, "the feature commit sits in the second lane");
        assert_eq!(rows[2].node, 0);
        // Branch point: the feature lane folds back into main's lane below b.
        assert!(rows[2].edges.iter().any(|e| !e.top && e.from == 1 && e.to == 0), "{:?}", rows[2]);
        assert!(rows[2].edges.contains(&Edge { from: 0, to: 0, top: false, color: 0 }), "b's own line continues");
        assert_eq!(rows[3].node, 0);
        assert!(rows[3].edges.iter().all(|e| e.top && e.from == 0), "{:?}", rows[3]);
    }

    #[test]
    fn unrelated_tips_get_their_own_lanes() {
        let (rows, w) = layout(&[c("x", &["w"]), c("y", &[]), c("w", &[])]);
        assert_eq!(w, 2);
        assert_eq!(rows[1].node, 1);
        assert_eq!(rows[2].node, 0);
    }

    #[test]
    fn every_edge_connects_rows() {
        // Whatever leaves the bottom of a row must arrive at the top of the next.
        let commits = [
            c("h", &["g", "e"]),
            c("g", &["f"]),
            c("e", &["d"]),
            c("f", &["d", "c"]),
            c("d", &["b"]),
            c("c", &["b"]),
            c("b", &["a"]),
            c("a", &[]),
        ];
        let (rows, _) = layout(&commits);
        for pair in rows.windows(2) {
            let mut out: Vec<u16> = pair[0].edges.iter().filter(|e| !e.top).map(|e| e.to).collect();
            let mut inn: Vec<u16> = pair[1].edges.iter().filter(|e| e.top).map(|e| e.from).collect();
            out.sort();
            out.dedup();
            inn.sort();
            inn.dedup();
            assert_eq!(out, inn, "{:?} → {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn path_commands() {
        let e = Edge { from: 0, to: 1, top: false, color: 1 };
        assert_eq!(path(&[&e], 12.0, 56.0), "M 6 28 C 6 42 18 42 18 56");
        let s = Edge { from: 1, to: 1, top: true, color: 1 };
        assert_eq!(path(&[&s], 12.0, 56.0), "M 18 0 L 18 28");
    }
}
