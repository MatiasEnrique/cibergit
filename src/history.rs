//! Repository commit history, and the lane graph drawn beside it.
//!
//! This is the one surface in cibergit that is not reached through a pull
//! request: it answers "what happened in this repository", which is a question
//! about refs and merges rather than about review. The read is bounded and
//! offline, and it never writes.
//!
//! The module is split in two on purpose. [`local_history`] talks to Git and
//! produces [`HistoryCommit`]s; [`lay_out`] takes those commits and produces
//! drawing instructions, with no I/O and no knowledge of GPUI. The second half
//! is where every interesting mistake lives, so it is a pure function over a
//! slice and is tested without a repository.
//!
//! Call [`local_history`] away from the UI thread. It runs a subprocess.

use crate::{
    comparisons::{InventoryAvailability, git, text_field},
    review::validate_object_id,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How many commits a single history read will accept. Reading one more than
/// this is what detects truncation: a history that fills the limit exactly and
/// one that was cut short are otherwise indistinguishable.
pub const MAX_HISTORY_COMMITS: usize = 500;

/// How wide the graph is allowed to get. A repository with forty unmerged
/// branches would otherwise push the commit subjects off the right of the page.
/// Past this, lanes share the last column and the graph says so rather than
/// drawing a picture that is quietly wrong.
pub const MAX_GRAPH_LANES: usize = 16;

/// How many distinct lane colours the cycle has. Kept here rather than in the
/// window's palette so `lay_out` can assign colours without knowing what they
/// look like.
pub const GRAPH_LANE_COLORS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    /// The checked-out branch, or a detached `HEAD`.
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCommit {
    pub sha: String,
    /// In Git's order. The first parent is the one a merge is diffed against.
    pub parent_shas: Vec<String>,
    pub message_headline: String,
    pub author_name: String,
    /// Only a provider read can establish an account identity; a local read
    /// knows a name and an address and nothing more.
    pub author_login: Option<String>,
    pub authored_at: String,
    pub committed_at: String,
    pub refs: Vec<RefLabel>,
}

impl HistoryCommit {
    pub fn short_sha(&self) -> &str {
        &self.sha[..7.min(self.sha.len())]
    }

    pub fn is_merge(&self) -> bool {
        self.parent_shas.len() > 1
    }

    /// The pair a commit's diff is taken over. `None` for a root commit, which
    /// has nothing to be compared against.
    pub fn first_parent(&self) -> Option<&str> {
        self.parent_shas.first().map(String::as_str)
    }
}

/// Which commits a history read covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryScope {
    /// Every branch and tag. This is what makes the graph a graph.
    AllRefs,
    /// One ref's ancestry.
    Ref(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryHistory {
    pub scope: HistoryScope,
    pub commits: Vec<HistoryCommit>,
    pub availability: InventoryAvailability,
    pub notice: Option<String>,
}

/// One commit's row in the graph gutter.
///
/// A row carries everything needed to draw it and nothing about its
/// neighbours, which is what lets the list virtualize: scrolling to row 4,000
/// does not require having laid out rows 0 through 3,999 on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    pub node_lane: usize,
    pub node_color: usize,
    /// More than one parent. Drawn as a ring rather than a disc.
    pub merge: bool,
    pub edges: Vec<GraphEdge>,
}

/// Which half of a row's band an edge occupies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Crosses the whole row without touching the node.
    Through,
    /// Comes down from the row above and ends at this row's node.
    Into,
    /// Leaves this row's node and continues into the row below.
    OutOf,
}

/// One line in a row's band. `lane` is always the end *away* from the node, so
/// a `Through` edge is drawn at `lane` top to bottom, an `Into` edge from
/// `lane` at the top edge to `node_lane` at the centre, and an `OutOf` edge
/// from `node_lane` at the centre to `lane` at the bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    pub kind: EdgeKind,
    pub lane: usize,
    pub color: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitGraph {
    pub rows: Vec<GraphRow>,
    /// The widest the graph ever gets, so every row's gutter is one width and
    /// the subjects beside them line up.
    pub lane_count: usize,
    /// The graph needed more than [`MAX_GRAPH_LANES`] lanes and was folded into
    /// the last one. The page says so; it does not pretend otherwise.
    pub overflowed: bool,
}

#[derive(Clone, Debug)]
struct Lane {
    /// The commit this lane is currently waiting to reach.
    expected: String,
    color: usize,
}

/// Assign every commit a lane, and every lane a colour that survives until the
/// lane is freed.
///
/// `commits` must be in topological order — a parent never before its child —
/// which is what `--topo-order` guarantees and what the whole algorithm rests
/// on. A lane is claimed by the commit it was waiting for, handed to that
/// commit's first parent, and freed when nothing expects it.
///
/// Lanes are never renumbered. Compacting them after a branch ends would be
/// tidier on paper and wrong on screen: a pass-through line would jump sideways
/// between two rows with no edge connecting the two positions.
pub fn lay_out(commits: &[HistoryCommit]) -> CommitGraph {
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    let mut next_color = 0usize;
    let mut lane_count = 0usize;
    let mut overflowed = false;

    for commit in commits {
        // Every lane waiting for this commit converges on it. The leftmost is
        // where the node is drawn; the rest end here.
        let claims: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| {
                lane.as_ref()
                    .filter(|lane| lane.expected == commit.sha)
                    .map(|_| index)
            })
            .collect();

        let (node_lane, node_color) = match claims.first() {
            Some(&first) => (
                first,
                lanes[first]
                    .as_ref()
                    .expect("claimed lane is occupied")
                    .color,
            ),
            // Nothing was waiting for this commit, so it is a tip: it starts a
            // lane of its own.
            None => {
                let lane = allocate(&mut lanes, &mut overflowed);
                let color = next_color % GRAPH_LANE_COLORS;
                next_color += 1;
                (lane, color)
            }
        };

        let mut edges = Vec::new();
        // The top half: what arrives from the row above. This is read before
        // any lane is reassigned, because it describes the state entering the
        // row, not leaving it.
        for (index, lane) in lanes.iter().enumerate() {
            let Some(lane) = lane else { continue };
            if claims.contains(&index) {
                edges.push(GraphEdge {
                    kind: EdgeKind::Into,
                    lane: index,
                    color: lane.color,
                });
            } else {
                edges.push(GraphEdge {
                    kind: EdgeKind::Through,
                    lane: index,
                    color: lane.color,
                });
            }
        }

        // Converging lanes are done; the node's own lane is reused below.
        for &claim in claims.iter().skip(1) {
            lanes[claim] = None;
        }
        if claims.is_empty() {
            // The tip allocated above is still empty; fill it so the first
            // parent can take it over by the ordinary path.
            lanes[node_lane] = Some(Lane {
                expected: commit.sha.clone(),
                color: node_color,
            });
        }

        // The bottom half: where this commit's parents continue. The first
        // parent inherits the node's lane and colour so a branch keeps one
        // colour for its whole length; the rest fork off.
        match commit.parent_shas.split_first() {
            Some((first, rest)) => {
                lanes[node_lane] = Some(Lane {
                    expected: first.clone(),
                    color: node_color,
                });
                edges.push(GraphEdge {
                    kind: EdgeKind::OutOf,
                    lane: node_lane,
                    color: node_color,
                });
                for parent in rest {
                    // A parent something else is already waiting for joins that
                    // lane rather than opening a second one for the same commit.
                    let existing = lanes.iter().position(|lane| {
                        lane.as_ref().is_some_and(|lane| &lane.expected == parent)
                    });
                    let (lane, color) = match existing {
                        Some(index) => (
                            index,
                            lanes[index].as_ref().expect("found lane is occupied").color,
                        ),
                        None => {
                            let lane = allocate(&mut lanes, &mut overflowed);
                            let color = next_color % GRAPH_LANE_COLORS;
                            next_color += 1;
                            lanes[lane] = Some(Lane {
                                expected: parent.clone(),
                                color,
                            });
                            (lane, color)
                        }
                    };
                    edges.push(GraphEdge {
                        kind: EdgeKind::OutOf,
                        lane,
                        color,
                    });
                }
            }
            // A root commit ends its lane. Nothing continues below it.
            None => lanes[node_lane] = None,
        }

        lane_count = lane_count.max(lanes.len()).max(node_lane + 1);
        rows.push(GraphRow {
            node_lane,
            node_color,
            merge: commit.is_merge(),
            edges,
        });
    }

    CommitGraph {
        rows,
        lane_count: lane_count.min(MAX_GRAPH_LANES),
        overflowed,
    }
}

/// The lowest free lane, or a new one on the right. Past [`MAX_GRAPH_LANES`]
/// everything shares the last column and the caller is told.
fn allocate(lanes: &mut Vec<Option<Lane>>, overflowed: &mut bool) -> usize {
    if let Some(index) = lanes.iter().position(Option::is_none) {
        return index;
    }
    if lanes.len() >= MAX_GRAPH_LANES {
        *overflowed = true;
        return MAX_GRAPH_LANES - 1;
    }
    lanes.push(None);
    lanes.len() - 1
}

/// Read this repository's history from a local checkout.
///
/// One bounded `git log`. The record framing is Git's `-z`: fields inside a
/// record are separated by the NULs in the format, and `-z` terminates each
/// record with one more, so the whole stream splits into `7n + 1` pieces with
/// an empty tail.
pub fn local_history(path: &Path, scope: &HistoryScope) -> Result<RepositoryHistory> {
    // One more than the limit, so a history that was cut short is
    // distinguishable from one that ends exactly on it.
    let limit = (MAX_HISTORY_COMMITS + 1).to_string();
    let mut args: Vec<String> = vec![
        "log".into(),
        "-z".into(),
        "--topo-order".into(),
        // Full ref paths make the decoration unambiguous. The short form
        // cannot tell a branch named `origin/main` from the remote-tracking
        // ref of the same name.
        "--decorate=full".into(),
        "-n".into(),
        limit,
        "--format=%H%x00%P%x00%s%x00%an%x00%aI%x00%cI%x00%D".into(),
    ];
    match scope {
        HistoryScope::AllRefs => args.push("--all".into()),
        HistoryScope::Ref(name) => {
            validate_ref_name(name)?;
            args.push(name.clone());
        }
    }
    // Everything after this is a path, so a ref that shares a name with a file
    // cannot be reinterpreted as one.
    args.push("--".into());

    let output = git(path, &args)?;
    let fields: Vec<_> = output.split(|byte| *byte == 0).collect();
    ensure!(
        fields.last().is_none_or(|field| field.is_empty()) && (fields.len() - 1) % 7 == 0,
        "Invalid local history records"
    );

    let mut commits = Vec::new();
    for record in fields[..fields.len().saturating_sub(1)].chunks_exact(7) {
        let sha = text_field(record[0], "commit OID")?;
        validate_object_id(&sha)?;
        let parent_shas = text_field(record[1], "commit parents")?
            .split_whitespace()
            .map(|parent| {
                validate_object_id(parent)?;
                Ok(parent.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        commits.push(HistoryCommit {
            sha,
            parent_shas,
            message_headline: text_field(record[2], "commit headline")?,
            author_name: text_field(record[3], "commit author")?,
            author_login: None,
            authored_at: text_field(record[4], "commit authored date")?,
            committed_at: text_field(record[5], "commit committed date")?,
            refs: parse_decorations(&text_field(record[6], "commit decorations")?),
        });
    }

    let truncated = commits.len() > MAX_HISTORY_COMMITS;
    commits.truncate(MAX_HISTORY_COMMITS);
    Ok(RepositoryHistory {
        scope: scope.clone(),
        commits,
        availability: if truncated {
            InventoryAvailability::Incomplete
        } else {
            InventoryAvailability::Complete
        },
        notice: truncated.then(|| {
            format!(
                "Showing the most recent {MAX_HISTORY_COMMITS} commits; this history is longer."
            )
        }),
    })
}

/// Parse `%D` under `--decorate=full`: a comma-separated list of full ref
/// paths, with the checked-out branch written as `HEAD -> refs/heads/name`.
///
/// Refs outside heads, remotes and tags — `refs/stash`, `refs/notes`, a
/// provider's own `refs/pull/*` — are dropped rather than shown, because they
/// are not places a reader can go.
fn parse_decorations(raw: &str) -> Vec<RefLabel> {
    raw.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (head, reference) = match entry.strip_prefix("HEAD -> ") {
                Some(rest) => (true, rest.trim()),
                None => (false, entry),
            };
            if reference == "HEAD" {
                return Some(RefLabel {
                    name: "HEAD".into(),
                    kind: RefKind::Head,
                });
            }
            // `--decorate=full` normally prints the bare path, but Git still
            // prefixes tags when `log.decorate` is configured short elsewhere.
            let reference = reference.strip_prefix("tag: ").unwrap_or(reference);
            let (kind, name) = if let Some(name) = reference.strip_prefix("refs/heads/") {
                (
                    if head {
                        RefKind::Head
                    } else {
                        RefKind::LocalBranch
                    },
                    name,
                )
            } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
                (RefKind::RemoteBranch, name)
            } else {
                (RefKind::Tag, reference.strip_prefix("refs/tags/")?)
            };
            (!name.is_empty()).then(|| RefLabel {
                name: name.to_owned(),
                kind,
            })
        })
        .collect()
}

/// A ref name that reaches the command line. The names this receives come from
/// cibergit's own decoration parse or from the provider's ref list, so this is
/// a backstop rather than the only guard — but a name starting with `-` would
/// become an option, and one containing `..` would become a range.
fn validate_ref_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 255 && !name.starts_with('-'),
        "Invalid Git ref name"
    );
    ensure!(
        !name.contains("..")
            && !name.ends_with(".lock")
            && !name.ends_with('/')
            && name
                .bytes()
                .all(|byte| byte > 0x20 && byte != 0x7f && !b"~^:?*[\\".contains(&byte)),
        "Invalid Git ref name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> HistoryCommit {
        HistoryCommit {
            sha: format!("{sha:0>40}").replace(' ', "0"),
            parent_shas: parents
                .iter()
                .map(|parent| format!("{parent:0>40}").replace(' ', "0"))
                .collect(),
            message_headline: sha.to_owned(),
            author_name: "Test".into(),
            author_login: None,
            authored_at: "2026-01-01T00:00:00Z".into(),
            committed_at: "2026-01-01T00:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn a_linear_history_stays_in_one_lane_and_one_colour() {
        let commits = [commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let graph = lay_out(&commits);
        assert_eq!(graph.lane_count, 1);
        assert!(!graph.overflowed);
        for row in &graph.rows {
            assert_eq!(row.node_lane, 0, "a linear history never leaves lane 0");
            assert_eq!(row.node_color, 0);
            assert!(!row.merge);
        }
        // The root commit ends its lane, so nothing continues below it.
        assert!(
            !graph.rows[2]
                .edges
                .iter()
                .any(|edge| edge.kind == EdgeKind::OutOf),
            "a root commit has no outgoing edge"
        );
    }

    #[test]
    fn a_merge_forks_a_second_lane_and_rejoins_it() {
        // m is a merge of the mainline (b) and a side branch (s), both on a.
        let commits = [
            commit("m", &["b", "s"]),
            commit("b", &["a"]),
            commit("s", &["a"]),
            commit("a", &[]),
        ];
        let graph = lay_out(&commits);
        assert_eq!(graph.lane_count, 2, "a side branch needs a second lane");

        let merge = &graph.rows[0];
        assert!(merge.merge, "two parents is a merge");
        assert_eq!(merge.node_lane, 0);
        let leaving: Vec<_> = merge
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::OutOf)
            .map(|edge| edge.lane)
            .collect();
        assert_eq!(leaving, vec![0, 1], "both parents leave the merge");

        // The first parent keeps the merge's colour; the side branch takes a
        // new one, and keeps it for its whole length.
        let side_color = merge
            .edges
            .iter()
            .find(|edge| edge.kind == EdgeKind::OutOf && edge.lane == 1)
            .expect("the side branch leaves the merge")
            .color;
        assert_ne!(side_color, merge.node_color);
        assert_eq!(graph.rows[2].node_lane, 1);
        assert_eq!(graph.rows[2].node_color, side_color);

        // Both lanes converge on the root, which draws two incoming edges.
        let root = &graph.rows[3];
        let arriving: Vec<_> = root
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Into)
            .map(|edge| edge.lane)
            .collect();
        assert_eq!(arriving, vec![0, 1], "both lanes end at the shared root");
    }

    #[test]
    fn a_freed_lane_is_reused_by_a_later_branch() {
        // s ends at a; t is a separate tip that should take the freed lane 1
        // rather than opening a third column.
        let commits = [
            commit("b", &["a"]),
            commit("s", &["a"]),
            commit("a", &[]),
            commit("t", &[]),
        ];
        let graph = lay_out(&commits);
        assert_eq!(
            graph.rows[3].node_lane, 0,
            "a new tip takes the lowest free lane, not a new one"
        );
        assert_eq!(graph.lane_count, 2);
    }

    #[test]
    fn an_octopus_merge_opens_a_lane_for_every_extra_parent() {
        let commits = [
            commit("m", &["a", "b", "c"]),
            commit("a", &[]),
            commit("b", &[]),
            commit("c", &[]),
        ];
        let graph = lay_out(&commits);
        assert!(graph.rows[0].merge);
        let leaving: Vec<_> = graph.rows[0]
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::OutOf)
            .map(|edge| edge.lane)
            .collect();
        assert_eq!(leaving, vec![0, 1, 2], "three parents occupy three lanes");
        assert_eq!(graph.lane_count, 3);
    }

    #[test]
    fn two_parents_that_are_the_same_commit_share_one_lane() {
        // Both of the merge's parents lead to a. The second must join the lane
        // already waiting for it instead of opening a duplicate.
        let commits = [commit("m", &["a", "a"]), commit("a", &[])];
        let graph = lay_out(&commits);
        let leaving: Vec<_> = graph.rows[0]
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::OutOf)
            .map(|edge| edge.lane)
            .collect();
        assert_eq!(leaving, vec![0, 0], "the repeated parent reuses its lane");
        assert_eq!(graph.lane_count, 1);
    }

    #[test]
    fn a_pass_through_lane_keeps_its_column_across_a_row() {
        // While the side branch s is unresolved, the row for b must carry it
        // through at its own index rather than dropping or moving it.
        let commits = [
            commit("m", &["b", "s"]),
            commit("b", &["a"]),
            commit("s", &["a"]),
            commit("a", &[]),
        ];
        let graph = lay_out(&commits);
        let through: Vec<_> = graph.rows[1]
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Through)
            .map(|edge| edge.lane)
            .collect();
        assert_eq!(
            through,
            vec![1],
            "the side branch crosses b's row untouched"
        );
    }

    #[test]
    fn independent_roots_reuse_one_lane_rather_than_widening_the_graph() {
        // A root commit frees its lane immediately, so tips that share no
        // history still stack into a single column.
        let commits: Vec<_> = (0..MAX_GRAPH_LANES + 4)
            .map(|index| commit(&format!("{index:x}"), &[]))
            .collect();
        let graph = lay_out(&commits);
        assert_eq!(graph.lane_count, 1);
        assert!(!graph.overflowed);
    }

    #[test]
    fn the_graph_folds_past_its_lane_limit_and_says_so() {
        // Every tip is still waiting for the same shared parent, so all of
        // their lanes are live at once and the graph runs out of columns.
        let tips = MAX_GRAPH_LANES + 4;
        let mut commits: Vec<_> = (0..tips)
            .map(|index| commit(&format!("t{index:x}"), &["fff"]))
            .collect();
        commits.push(commit("fff", &[]));
        let graph = lay_out(&commits);
        assert_eq!(graph.lane_count, MAX_GRAPH_LANES);
        assert!(
            graph.overflowed,
            "a graph wider than it can be drawn has to admit it"
        );
        assert!(
            graph.rows.iter().all(|row| row.node_lane < MAX_GRAPH_LANES),
            "no row may be drawn outside the gutter"
        );
    }

    #[test]
    fn lane_colours_cycle_without_running_off_the_palette() {
        let commits: Vec<_> = (0..GRAPH_LANE_COLORS * 3)
            .map(|index| commit(&format!("{index:x}"), &[]))
            .collect();
        let graph = lay_out(&commits);
        assert!(
            graph
                .rows
                .iter()
                .all(|row| row.node_color < GRAPH_LANE_COLORS),
            "a colour index must always address the palette"
        );
    }

    #[test]
    fn decorations_separate_head_branches_remotes_and_tags() {
        let labels = parse_decorations(
            "HEAD -> refs/heads/main, refs/heads/integration/v1, \
             refs/remotes/origin/main, refs/tags/v1.0, refs/stash",
        );
        assert_eq!(
            labels,
            vec![
                RefLabel {
                    name: "main".into(),
                    kind: RefKind::Head
                },
                RefLabel {
                    name: "integration/v1".into(),
                    kind: RefKind::LocalBranch
                },
                RefLabel {
                    name: "origin/main".into(),
                    kind: RefKind::RemoteBranch
                },
                RefLabel {
                    name: "v1.0".into(),
                    kind: RefKind::Tag
                },
            ],
            "refs/stash is not a place a reader can go, so it is not shown"
        );
    }

    #[test]
    fn a_detached_head_is_its_own_label() {
        assert_eq!(
            parse_decorations("HEAD"),
            vec![RefLabel {
                name: "HEAD".into(),
                kind: RefKind::Head
            }]
        );
        assert!(parse_decorations("").is_empty());
    }

    #[test]
    fn ref_names_that_would_become_options_or_ranges_are_refused() {
        for name in [
            "",
            "--all",
            "main..other",
            "main~1",
            "main^",
            "with space",
            "refs/heads/",
            "main.lock",
            "head:ref",
        ] {
            assert!(
                validate_ref_name(name).is_err(),
                "{name:?} must not reach the command line"
            );
        }
        for name in ["main", "refs/heads/main", "feature/a-b_c", "v1.0"] {
            assert!(
                validate_ref_name(name).is_ok(),
                "{name:?} is an ordinary ref"
            );
        }
    }
}
