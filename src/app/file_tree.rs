use cibergit::{domain::ChangedFile, review::file_key};
use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TreeRowKind {
    Directory {
        directory_key: String,
        expanded: bool,
        raw: bool,
    },
    File {
        file_key: String,
        status: String,
        additions: u64,
        deletions: u64,
        patch_available: bool,
        rename_from: Option<String>,
        raw: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TreeRow {
    pub depth: usize,
    pub label: String,
    pub kind: TreeRowKind,
}

impl TreeRow {
    fn identity(&self) -> String {
        match &self.kind {
            TreeRowKind::Directory { directory_key, .. } => format!("d:{directory_key}"),
            TreeRowKind::File { file_key, .. } => format!("f:{file_key}"),
        }
    }

    pub fn file_key(&self) -> Option<&str> {
        match &self.kind {
            TreeRowKind::File { file_key, .. } => Some(file_key),
            TreeRowKind::Directory { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
enum TreeNode {
    Directory {
        key: String,
        label: String,
        raw: bool,
        children: Vec<TreeNode>,
    },
    File {
        key: String,
        label: String,
        status: String,
        additions: u64,
        deletions: u64,
        patch_available: bool,
        rename_from: Option<String>,
        raw: bool,
    },
}

impl TreeNode {
    fn directory_mut(&mut self, wanted: &str) -> Option<&mut Vec<TreeNode>> {
        match self {
            Self::Directory { key, children, .. } if key == wanted => Some(children),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct PathPart {
    identity: String,
    label: String,
}

/// UI-only tree state. File operations always use `file_key`; labels and
/// directory grouping are presentation data and are never sent back to Git.
#[derive(Clone, Debug, Default)]
pub(super) struct FileTree {
    roots: Vec<TreeNode>,
    collapsed: HashSet<String>,
    cursor_identity: Option<String>,
    visible: OnceCell<VisibleRows>,
}

#[derive(Clone, Debug)]
struct VisibleRows {
    rows: Rc<[TreeRow]>,
    indices: HashMap<String, usize>,
}

impl FileTree {
    pub fn new(files: &[ChangedFile]) -> Self {
        let mut tree = Self::default();
        tree.sync(files);
        tree
    }

    pub fn sync(&mut self, files: &[ChangedFile]) {
        self.visible.take();
        self.roots.clear();
        for file in files {
            self.insert(file);
        }
        let directories = self.directory_keys();
        self.collapsed.retain(|key| directories.contains(key));
        if self
            .cursor_identity
            .as_ref()
            .is_some_and(|identity| !self.rows().iter().any(|row| row.identity() == *identity))
        {
            self.cursor_identity = None;
        }
    }

    fn visible_rows(&self) -> &VisibleRows {
        self.visible.get_or_init(|| {
            let mut rows = Vec::new();
            self.flatten(&self.roots, 0, &mut rows);
            let mut indices = HashMap::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                indices.entry(row.identity()).or_insert(index);
            }
            VisibleRows {
                rows: rows.into(),
                indices,
            }
        })
    }

    // Scrolling shares the flattened inventory; only structural changes invalidate it.
    pub fn rows(&self) -> Rc<[TreeRow]> {
        self.visible_rows().rows.clone()
    }

    pub fn cursor_index(&self) -> Option<usize> {
        self.visible_rows()
            .indices
            .get(self.cursor_identity.as_ref()?)
            .copied()
    }

    pub fn set_cursor(&mut self, identity: String) {
        self.cursor_identity = Some(identity);
    }

    pub fn row_identity(row: &TreeRow) -> String {
        row.identity()
    }

    pub fn toggle_directory(&mut self, key: &str) -> bool {
        if !self.directory_keys().contains(key) {
            return false;
        }
        self.visible.take();
        if !self.collapsed.insert(key.to_owned()) {
            self.collapsed.remove(key);
        }
        self.cursor_identity = Some(format!("d:{key}"));
        true
    }

    /// Expands every ancestor of `wanted`, makes it the keyboard cursor, and
    /// returns its current virtual-row index for reveal scrolling.
    pub fn reveal_file(&mut self, wanted: &str) -> Option<usize> {
        let mut ancestors = Vec::new();
        if !find_file_ancestors(&self.roots, wanted, &mut ancestors) {
            return None;
        }
        for ancestor in ancestors {
            if self.collapsed.remove(&ancestor) {
                self.visible.take();
            }
        }
        self.cursor_identity = Some(format!("f:{wanted}"));
        self.cursor_index()
    }

    #[cfg(feature = "ui-smoke")]
    pub fn collapse_ancestor_of(&mut self, wanted: &str) -> Option<String> {
        let mut ancestors = Vec::new();
        if !find_file_ancestors(&self.roots, wanted, &mut ancestors) {
            return None;
        }
        let ancestor = ancestors.last()?.clone();
        self.collapsed.insert(ancestor.clone());
        self.visible.take();
        Some(ancestor)
    }

    #[cfg(feature = "ui-smoke")]
    pub fn file_is_visible(&self, wanted: &str) -> bool {
        self.rows().iter().any(|row| row.file_key() == Some(wanted))
    }

    /// Moves within visible rows. Landing on a leaf returns its stable key so
    /// the caller can make it the one selected diff.
    pub fn move_cursor(&mut self, delta: isize) -> Option<String> {
        let rows = self.rows();
        if rows.is_empty() {
            self.cursor_identity = None;
            return None;
        }
        let current = self
            .cursor_index()
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        let next = current
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.cursor_identity = Some(rows[next].identity());
        rows[next].file_key().map(str::to_owned)
    }

    /// Standard tree left behavior: collapse an open directory, otherwise
    /// move the cursor to its nearest visible parent.
    pub fn left(&mut self) {
        let rows = self.rows();
        let Some(index) = self.cursor_index() else {
            return;
        };
        let row = &rows[index];
        if let TreeRowKind::Directory {
            directory_key,
            expanded: true,
            ..
        } = &row.kind
        {
            self.collapsed.insert(directory_key.clone());
            self.visible.take();
            return;
        }
        if row.depth == 0 {
            return;
        }
        if let Some(parent) = rows[..index]
            .iter()
            .rfind(|candidate| candidate.depth + 1 == row.depth)
        {
            self.cursor_identity = Some(parent.identity());
        }
    }

    /// Standard tree right behavior: expand a closed directory, otherwise
    /// move into its first child. Leaves remain the active review selection.
    pub fn right(&mut self) -> Option<String> {
        let rows = self.rows();
        let index = self.cursor_index()?;
        let row = &rows[index];
        match &row.kind {
            TreeRowKind::Directory {
                directory_key,
                expanded: false,
                ..
            } => {
                self.collapsed.remove(directory_key);
                self.visible.take();
                None
            }
            TreeRowKind::Directory { expanded: true, .. } => {
                let next = self.rows().get(index + 1)?.clone();
                if next.depth == row.depth + 1 {
                    self.cursor_identity = Some(next.identity());
                    next.file_key().map(str::to_owned)
                } else {
                    None
                }
            }
            TreeRowKind::File { file_key, .. } => Some(file_key.clone()),
        }
    }

    pub fn activate_cursor(&mut self) -> Option<String> {
        let rows = self.rows();
        let row = rows.get(self.cursor_index()?)?;
        match &row.kind {
            TreeRowKind::Directory { directory_key, .. } => {
                let key = directory_key.clone();
                self.toggle_directory(&key);
                None
            }
            TreeRowKind::File { file_key, .. } => Some(file_key.clone()),
        }
    }

    fn insert(&mut self, file: &ChangedFile) {
        let raw = file.raw_path.is_some();
        let parts = path_parts(file);
        let Some((leaf, directories)) = parts.split_last() else {
            return;
        };
        let mut children = &mut self.roots;
        let mut accumulated = String::from(if raw { "raw" } else { "utf8" });
        for part in directories {
            accumulated.push('/');
            accumulated.push_str(&part.identity);
            let index = children.iter().position(
                |node| matches!(node, TreeNode::Directory { key, .. } if key == &accumulated),
            );
            let index = index.unwrap_or_else(|| {
                children.push(TreeNode::Directory {
                    key: accumulated.clone(),
                    label: part.label.clone(),
                    raw,
                    children: Vec::new(),
                });
                children.len() - 1
            });
            children = children[index]
                .directory_mut(&accumulated)
                .expect("directory inserted above");
        }
        children.push(TreeNode::File {
            key: file_key(file),
            label: leaf.label.clone(),
            status: file.status.clone(),
            additions: file.additions,
            deletions: file.deletions,
            patch_available: file.patch.is_some(),
            rename_from: file.previous_path.clone(),
            raw,
        });
    }

    fn flatten(&self, nodes: &[TreeNode], depth: usize, rows: &mut Vec<TreeRow>) {
        for node in nodes {
            match node {
                TreeNode::Directory {
                    key,
                    label,
                    raw,
                    children,
                } => {
                    let expanded = !self.collapsed.contains(key);
                    rows.push(TreeRow {
                        depth,
                        label: label.clone(),
                        kind: TreeRowKind::Directory {
                            directory_key: key.clone(),
                            expanded,
                            raw: *raw,
                        },
                    });
                    if expanded {
                        self.flatten(children, depth + 1, rows);
                    }
                }
                TreeNode::File {
                    key,
                    label,
                    status,
                    additions,
                    deletions,
                    patch_available,
                    rename_from,
                    raw,
                } => rows.push(TreeRow {
                    depth,
                    label: label.clone(),
                    kind: TreeRowKind::File {
                        file_key: key.clone(),
                        status: status.clone(),
                        additions: *additions,
                        deletions: *deletions,
                        patch_available: *patch_available,
                        rename_from: rename_from.clone(),
                        raw: *raw,
                    },
                }),
            }
        }
    }

    fn directory_keys(&self) -> HashSet<String> {
        fn collect(nodes: &[TreeNode], keys: &mut HashSet<String>) {
            for node in nodes {
                if let TreeNode::Directory { key, children, .. } = node {
                    keys.insert(key.clone());
                    collect(children, keys);
                }
            }
        }
        let mut keys = HashSet::new();
        collect(&self.roots, &mut keys);
        keys
    }
}

fn find_file_ancestors(nodes: &[TreeNode], wanted: &str, ancestors: &mut Vec<String>) -> bool {
    for node in nodes {
        match node {
            TreeNode::File { key, .. } if key == wanted => return true,
            TreeNode::Directory { key, children, .. } => {
                ancestors.push(key.clone());
                if find_file_ancestors(children, wanted, ancestors) {
                    return true;
                }
                ancestors.pop();
            }
            _ => {}
        }
    }
    false
}

fn path_parts(file: &ChangedFile) -> Vec<PathPart> {
    match &file.raw_path {
        Some(bytes) => bytes
            .split(|byte| *byte == b'/')
            .map(|part| PathPart {
                identity: hex(part),
                label: display_raw(part),
            })
            .collect(),
        None => file
            .path
            .split('/')
            .map(|part| PathPart {
                identity: hex(part.as_bytes()),
                label: part.to_owned(),
            })
            .collect(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn display_raw(bytes: &[u8]) -> String {
    let mut display = String::new();
    for byte in bytes {
        match byte {
            b' '..=b'~' if *byte != b'\\' => display.push(*byte as char),
            _ => display.push_str(&format!("\\x{byte:02x}")),
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            previous_path: None,
            raw_path: None,
            raw_previous_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 2,
            patch: None,
            patch_complete: false,
        }
    }

    #[test]
    fn repeated_tree_reads_share_rows_and_structural_changes_invalidate_them() {
        let files = (0..5000)
            .map(|index| changed(&format!("src/file-{index}.rs")))
            .collect::<Vec<_>>();
        let mut tree = FileTree::new(&files);
        let rows = tree.rows();
        tree.reveal_file("src/file-4999.rs");
        for _ in 0..100 {
            assert!(Rc::ptr_eq(&rows, &tree.rows()));
            assert_eq!(tree.cursor_index(), Some(5000));
        }
        let TreeRowKind::Directory { directory_key, .. } = &rows[0].kind else {
            panic!("missing directory")
        };
        tree.toggle_directory(directory_key);
        assert_eq!(tree.rows().len(), 1);
        tree.reveal_file("src/file-4999.rs");
        assert_eq!(tree.rows().len(), 5001);
        assert!(!Rc::ptr_eq(&rows, &tree.rows()));
        tree.sync(&[changed("replacement.rs")]);
        assert_eq!(tree.rows()[0].file_key(), Some("replacement.rs"));
    }

    #[test]
    fn collapse_and_reveal_restore_selected_ancestors() {
        let files = vec![changed("src/app/main.rs"), changed("docs/main.rs")];
        let mut tree = FileTree::new(&files);
        let src = tree
            .rows()
            .iter()
            .find_map(|row| match &row.kind {
                TreeRowKind::Directory { directory_key, .. } if row.label == "src" => {
                    Some(directory_key.clone())
                }
                _ => None,
            })
            .unwrap();
        tree.toggle_directory(&src);
        assert!(!tree.rows().iter().any(|row| row.label == "app"));

        let key = file_key(&files[0]);
        let index = tree.reveal_file(&key).unwrap();
        let rows = tree.rows();
        assert_eq!(rows[index].file_key(), Some(key.as_str()));
        assert!(rows.iter().any(|row| row.label == "app"));
    }

    #[test]
    fn duplicate_basenames_keep_distinct_leaf_identity() {
        let files = vec![changed("src/main.rs"), changed("tests/main.rs")];
        let tree = FileTree::new(&files);
        let keys = tree
            .rows()
            .iter()
            .filter(|row| row.label == "main.rs")
            .filter_map(|row| row.file_key().map(str::to_owned))
            .collect::<HashSet<_>>();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("src/main.rs"));
        assert!(keys.contains("tests/main.rs"));
    }

    #[test]
    fn raw_and_literal_escape_paths_do_not_share_directory_or_leaf_identity() {
        let literal = changed(r"raw/\xff/name.bin");
        let mut raw = changed(r"raw/\xff/name.bin");
        raw.raw_path = Some(vec![
            b'r', b'a', b'w', b'/', 0xff, b'/', b'n', b'a', b'm', b'e',
        ]);
        let tree = FileTree::new(&[literal, raw]);
        let rows = tree.rows();
        let escaped_directories = rows
            .iter()
            .filter_map(|row| match &row.kind {
                TreeRowKind::Directory {
                    directory_key, raw, ..
                } if row.label == r"\xff" => Some((directory_key.clone(), *raw)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(escaped_directories.len(), 2);
        assert_ne!(escaped_directories[0].0, escaped_directories[1].0);
        assert_ne!(escaped_directories[0].1, escaped_directories[1].1);
        let leaf_keys = rows
            .iter()
            .filter_map(|row| row.file_key().map(str::to_owned))
            .collect::<HashSet<_>>();
        assert_eq!(leaf_keys.len(), 2);
    }

    #[test]
    fn next_previous_order_can_reveal_files_hidden_by_collapse() {
        let files = vec![
            changed("a/first.rs"),
            changed("b/second.rs"),
            changed("c/third.rs"),
        ];
        let mut tree = FileTree::new(&files);
        let b = tree
            .rows()
            .iter()
            .find_map(|row| match &row.kind {
                TreeRowKind::Directory { directory_key, .. } if row.label == "b" => {
                    Some(directory_key.clone())
                }
                _ => None,
            })
            .unwrap();
        tree.toggle_directory(&b);
        let complete_order = files.iter().map(file_key).collect::<Vec<_>>();
        assert_eq!(complete_order[1], "b/second.rs");
        let index = tree.reveal_file(&complete_order[1]).unwrap();
        assert_eq!(tree.rows()[index].file_key(), Some("b/second.rs"));
    }

    #[test]
    fn utf8_rename_keeps_new_file_key_separate_from_labels() {
        let mut file = changed("nuevo/archivo.rs");
        file.previous_path = Some("viejo/archivo.rs".into());
        file.status = "renamed".into();
        let tree = FileTree::new(&[file]);
        let rows = tree.rows();
        let row = rows.iter().find(|row| row.file_key().is_some()).unwrap();
        assert_eq!(row.label, "archivo.rs");
        assert_eq!(row.file_key(), Some("nuevo/archivo.rs"));
        assert!(matches!(
            row.kind,
            TreeRowKind::File {
                rename_from: Some(ref value),
                ..
            } if value == "viejo/archivo.rs"
        ));
    }
}
