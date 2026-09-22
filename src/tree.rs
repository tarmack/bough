//! In-memory tree of changed files.
//!
//! Flat `(status, path)` pairs from the git engine are grouped into a nested
//! node tree. A directory aggregates the set of statuses present anywhere
//! beneath it; a file carries its own single status letter.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    /// Full slash-joined path within the repository.
    pub path: String,
    pub kind: Kind,
    /// File: a single status letter; directory: sorted aggregate of all child
    /// statuses (e.g. `"AMD"`).
    pub status: String,
    /// Indices into [`Tree::nodes`].
    pub children: Vec<usize>,
    pub expanded: bool,
    /// Number of path components below the virtual root.
    pub depth: usize,
    pub added: usize,
    pub deleted: usize,
    pub file_count: usize,
    /// Files beneath this node per status letter, indexed by [`STATUS_ORDER`].
    /// Only the root's is consumed by the UI (the tree header); directories
    /// aggregate so the root's total is complete.
    pub status_counts: [usize; 6],
}

#[derive(Debug, Clone)]
pub struct Tree {
    pub nodes: Vec<TreeNode>,
    pub root: usize,
}

impl Tree {
    /// Build a tree from a flat list of `(status, path)` entries. Directories
    /// start collapsed — the caller decides what is visible via
    /// [`TreeNode::expanded`] or [`Tree::expand_paths`].
    pub fn build(entries: Vec<(char, String)>) -> Self {
        let mut nodes = vec![TreeNode {
            name: String::new(),
            path: String::new(),
            kind: Kind::Dir,
            status: String::new(),
            children: Vec::new(),
            expanded: true,
            depth: 0,
            added: 0,
            deleted: 0,
            file_count: 0,
            status_counts: [0; 6],
        }];
        let root = 0;

        for (status, path) in entries {
            insert(&mut nodes, root, &path, status);
        }

        sort_recursive(&mut nodes, root);
        aggregate(&mut nodes, root);

        Tree { nodes, root }
    }

    /// Flatten visible nodes (respecting expansion state) into a list of node
    /// indices. The virtual root itself is omitted.
    pub fn flatten(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.flatten_rec(self.root, &mut out);
        out
    }

    fn flatten_rec(&self, idx: usize, out: &mut Vec<usize>) {
        let node = &self.nodes[idx];
        if idx != self.root {
            out.push(idx);
        }
        if node.kind == Kind::Dir && node.expanded {
            for &child in &node.children {
                self.flatten_rec(child, out);
            }
        }
    }

    /// Apply per-file `(added, deleted)` stats and propagate sums up to
    /// directories.
    pub fn annotate_stats(&mut self, stats: &HashMap<String, (usize, usize)>) {
        for node in &mut self.nodes {
            if node.kind == Kind::File {
                if let Some(&(added, deleted)) = stats.get(&node.path) {
                    node.added = added;
                    node.deleted = deleted;
                }
            }
        }
        sum_stats(&mut self.nodes, self.root);
    }

    /// Reapply expansion state from a set of expanded dir paths. Every
    /// directory whose `path` is in `paths` becomes expanded; all others
    /// collapse. Used by the UI after swapping in a fresh `Snapshot` so the
    /// user's collapse/expand choices survive a tree rebuild.
    pub fn expand_paths(&mut self, paths: &HashSet<String>) {
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            // The virtual root is not a user-collapsible directory; leaving it
            // expanded keeps the whole tree reachable even when `paths` omits
            // its empty-string path.
            if idx != self.root && node.kind == Kind::Dir {
                node.expanded = paths.contains(&node.path);
            }
        }
    }
}

fn insert(nodes: &mut Vec<TreeNode>, root: usize, path: &str, status: char) {
    let parts: Vec<&str> = path.split('/').collect();
    let mut cur = root;

    for (i, part) in parts.iter().enumerate() {
        let is_file = i == parts.len() - 1;
        let kind = if is_file { Kind::File } else { Kind::Dir };

        if let Some(existing) = find_child(nodes, cur, part) {
            // A repeated `(status, path)` entry: the newer file status wins.
            debug_assert_eq!(nodes[existing].kind, kind);
            if is_file && nodes[existing].kind == Kind::File {
                nodes[existing].status = status.to_string();
            }
            cur = existing;
            continue;
        }

        let depth = i + 1;
        let node_path = parts[..=i].join("/");
        let new_idx = nodes.len();
        nodes.push(TreeNode {
            name: (*part).to_string(),
            path: node_path,
            kind,
            status: if is_file {
                status.to_string()
            } else {
                String::new()
            },
            children: Vec::new(),
            expanded: false,
            depth,
            added: 0,
            deleted: 0,
            file_count: 0,
            status_counts: [0; 6],
        });
        nodes[cur].children.push(new_idx);
        cur = new_idx;
    }
}

fn sum_stats(nodes: &mut Vec<TreeNode>, idx: usize) -> (usize, usize) {
    if nodes[idx].kind == Kind::File {
        nodes[idx].file_count = 1;
        let mut counts = [0usize; 6];
        if let Some(i) = nodes[idx].status.chars().next().and_then(status_index) {
            counts[i] = 1;
        }
        nodes[idx].status_counts = counts;
        return (nodes[idx].added, nodes[idx].deleted);
    }
    let children = nodes[idx].children.clone();
    let (mut added, mut deleted) = (0usize, 0usize);
    let mut files = 0usize;
    let mut counts = [0usize; 6];
    for child in children {
        let (a, d) = sum_stats(nodes, child);
        added += a;
        deleted += d;
        files += nodes[child].file_count;
        for (slot, &c) in counts.iter_mut().zip(nodes[child].status_counts.iter()) {
            *slot += c;
        }
    }
    nodes[idx].added = added;
    nodes[idx].deleted = deleted;
    nodes[idx].file_count = files;
    nodes[idx].status_counts = counts;
    (added, deleted)
}

fn find_child(nodes: &[TreeNode], parent: usize, name: &str) -> Option<usize> {
    nodes[parent]
        .children
        .iter()
        .copied()
        .find(|&c| nodes[c].name == name)
}

/// Order children: directories first, then files, each alphabetically
/// (case-insensitive).
fn sort_recursive(nodes: &mut Vec<TreeNode>, idx: usize) {
    let mut children = nodes[idx].children.clone();
    children.sort_by(|&a, &b| {
        let ra = kind_rank(nodes[a].kind);
        let rb = kind_rank(nodes[b].kind);
        ra.cmp(&rb).then_with(|| {
            nodes[a]
                .name
                .to_lowercase()
                .cmp(&nodes[b].name.to_lowercase())
        })
    });
    nodes[idx].children = children;

    let children = nodes[idx].children.clone();
    for child in children {
        sort_recursive(nodes, child);
    }
}

fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Dir => 0,
        Kind::File => 1,
    }
}

/// Populate each directory's aggregate status from its descendants' statuses.
fn aggregate(nodes: &mut Vec<TreeNode>, idx: usize) -> String {
    if nodes[idx].kind == Kind::File {
        return nodes[idx].status.clone();
    }

    let children = nodes[idx].children.clone();
    let mut present: Vec<char> = Vec::new();
    for child in children {
        for ch in aggregate(nodes, child).chars() {
            if !present.contains(&ch) {
                present.push(ch);
            }
        }
    }

    present.sort_by_key(|&c| dir_status_rank(c));
    let status: String = present.into_iter().collect();
    nodes[idx].status = status.clone();
    status
}

/// Sort rank for a status letter inside a directory's aggregate status string:
/// A, M, D, R, C, T. Display ordering only. It deliberately differs from
/// [`STATUS_ORDER`] (M, A, D, R, C, T), which indexes
/// [`TreeNode::status_counts`]; do not unify the two.
fn dir_status_rank(c: char) -> u8 {
    match c {
        'A' => 0,
        'M' => 1,
        'D' => 2,
        'R' => 3,
        'C' => 4,
        'T' => 5,
        _ => 6,
    }
}

/// Status letters in the order the tree header lists them; indexes
/// [`TreeNode::status_counts`]. Distinct from [`dir_status_rank`], whose
/// A-first order sorts a directory's aggregate status string.
pub const STATUS_ORDER: [char; 6] = ['M', 'A', 'D', 'R', 'C', 'T'];

/// Index of `c` in [`STATUS_ORDER`], or `None` for an unknown letter.
fn status_index(c: char) -> Option<usize> {
    STATUS_ORDER.iter().position(|&s| s == c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<(char, String)> {
        vec![
            ('M', "src/core/mod.rs".to_string()),
            ('A', "src/core/new.rs".to_string()),
            ('D', "README.md".to_string()),
        ]
    }

    #[test]
    fn groups_nested_paths() {
        let tree = Tree::build(entries());
        // root -> src (dir) and README.md (file)
        assert_eq!(tree.nodes[tree.root].children.len(), 2);
        // src -> core (dir) -> mod.rs, new.rs
        let src_idx = tree.nodes[tree.root].children[0];
        assert_eq!(tree.nodes[src_idx].children.len(), 1);
        // Directories default collapsed, so only the top level is visible.
        let flat = tree.flatten();
        assert_eq!(flat.len(), 2); // src, README.md
        assert_eq!(flat[0], src_idx);
    }

    #[test]
    fn directory_aggregates_status() {
        let tree = Tree::build(entries());
        let src_idx = tree.nodes[tree.root].children[0];
        assert_eq!(tree.nodes[src_idx].kind, Kind::Dir);
        assert_eq!(tree.nodes[src_idx].status, "AM");
    }

    #[test]
    fn directories_default_collapsed() {
        let tree = Tree::build(entries());
        let src_idx = tree.nodes[tree.root].children[0];
        assert!(!tree.nodes[src_idx].expanded);
        // Only the top level is visible without a manual expand.
        assert_eq!(tree.flatten().len(), 2); // src, README.md
    }

    #[test]
    fn collapse_hides_children_and_expand_restores() {
        let mut tree = Tree::build(entries());
        let src_idx = tree.nodes[tree.root].children[0];
        let core_idx = tree.nodes[src_idx].children[0];
        assert!(!tree.nodes[core_idx].expanded);

        // Expand src: its child dir shows, but core stays collapsed.
        tree.nodes[src_idx].expanded = true;
        let flat = tree.flatten();
        assert_eq!(flat.len(), 3); // src, core, README.md
        assert_eq!(flat[0], src_idx);

        // Collapse src: its whole subtree hides.
        tree.nodes[src_idx].expanded = false;
        let flat = tree.flatten();
        assert_eq!(flat.len(), 2); // src, README.md

        // Expand src again (core is still collapsed from its own default).
        tree.nodes[src_idx].expanded = true;
        assert_eq!(tree.flatten().len(), 3); // src, core, README.md

        // Expanding core too surfaces the files.
        tree.nodes[core_idx].expanded = true;
        assert_eq!(tree.flatten().len(), 5); // src, core, mod.rs, new.rs, README.md
    }

    #[test]
    fn expand_paths_keeps_root_expanded() {
        let mut tree = Tree::build(entries());
        // A set without the root's empty path must not collapse the root,
        // otherwise the whole tree becomes invisible.
        tree.expand_paths(&HashSet::new());
        assert!(tree.nodes[tree.root].expanded);
        assert_eq!(tree.flatten().len(), 2); // src, README.md
    }

    #[test]
    fn duplicate_file_entry_keeps_latest_status() {
        let tree = Tree::build(vec![('M', "a.rs".to_string()), ('A', "a.rs".to_string())]);
        let file_idx = tree.nodes[tree.root].children[0];
        assert_eq!(tree.nodes[file_idx].kind, Kind::File);
        assert_eq!(tree.nodes[file_idx].status, "A");
    }

    #[test]
    fn annotate_stats_aggregates_status_counts() {
        let mut tree = Tree::build(vec![
            ('M', "src/a.rs".to_string()),
            ('A', "src/b.rs".to_string()),
            ('D', "README.md".to_string()),
        ]);
        let mut stats = HashMap::new();
        stats.insert("src/a.rs".to_string(), (1, 2));
        stats.insert("src/b.rs".to_string(), (3, 0));
        tree.annotate_stats(&stats);

        let root = &tree.nodes[tree.root];
        assert_eq!(root.file_count, 3);
        // Indexed by STATUS_ORDER (M, A, D, R, C, T).
        assert_eq!(root.status_counts, [1, 1, 1, 0, 0, 0]);
        assert_eq!(root.added, 4);
        assert_eq!(root.deleted, 2);

        // A directory aggregates its own subtree the same way.
        let src = &tree.nodes[tree.nodes[tree.root].children[0]];
        assert_eq!(src.file_count, 2);
        assert_eq!(src.status_counts, [1, 1, 0, 0, 0, 0]);
    }
}
