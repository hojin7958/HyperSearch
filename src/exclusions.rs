//! Folder exclusions shared by cache loading, fast indexing and watcher scans.
use crate::model::Index;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct FolderExclusions {
    paths: Vec<String>,
}

pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let text = if let Some(tail) = text.strip_prefix(r"\\?\unc\") {
        format!(r"\\{tail}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    };
    text.trim_end_matches('\\').to_owned()
}

impl FolderExclusions {
    pub fn new(paths: &[PathBuf]) -> Self {
        Self::from_strings(
            &paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )
    }

    pub fn from_strings(paths: &[String]) -> Self {
        let mut paths: Vec<_> = paths
            .iter()
            .map(|p| path_key(Path::new(p)))
            .filter(|p| !p.is_empty())
            .collect();
        paths.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
        let mut selected = Self::default();
        for path in paths {
            if !selected.matches_key(&path) {
                selected.paths.push(path);
            }
        }
        selected.paths.sort_unstable();
        selected
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    pub fn matches(&self, path: &Path) -> bool {
        !self.paths.is_empty() && self.matches_key(&path_key(path))
    }

    pub fn matches_key(&self, path: &str) -> bool {
        self.paths.iter().any(|excluded| {
            path == excluded
                || path
                    .strip_prefix(excluded)
                    .is_some_and(|tail| tail.starts_with('\\'))
        })
    }

    pub fn covers(&self, previous: &Self) -> bool {
        previous.paths.iter().all(|path| self.matches_key(path))
    }

    /// Keep folder IDs stable for incremental updates. Normalize each folder
    /// once rather than building a full path for millions of file records.
    pub fn apply(&self, index: &mut Index) -> usize {
        let before = index.len();
        if !self.paths.is_empty() {
            let mut excluded_names: HashMap<&str, HashSet<&str>> = HashMap::new();
            for path in &self.paths {
                if let Some((parent, name)) = path.rsplit_once('\\') {
                    excluded_names.entry(parent).or_default().insert(name);
                }
            }
            let mut names_by_parent = HashMap::new();
            let blocked: Vec<bool> = index
                .folders
                .iter()
                .enumerate()
                .map(|(id, path)| {
                    let key = path_key(Path::new(&path));
                    if let Some(names) = excluded_names.get(key.as_str()) {
                        names_by_parent.insert(id as u32, names);
                    }
                    self.matches_key(&key)
                })
                .collect();
            index.entries.retain(|entry| {
                !blocked[entry.parent as usize]
                    && !names_by_parent
                        .get(&entry.parent)
                        .is_some_and(|names| names.contains(entry.name.to_lowercase().as_str()))
            });
        }
        index.excluded_folders = self.paths.clone();
        before - index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Entry;

    #[test]
    fn folder_boundaries_case_unicode_extended_paths_and_nested_rules() {
        let rules = FolderExclusions::new(&[
            r"C:\자료\제외\".into(),
            r"c:\자료\제외\하위".into(),
            r"\\?\UNC\Server\Share\Secret".into(),
        ]);
        assert_eq!(rules.paths().len(), 2);
        for path in [
            r"c:/자료/제외",
            r"\\?\C:\자료\제외\파일.xlsx",
            r"\\server\share\SECRET\child.txt",
        ] {
            assert!(rules.matches(Path::new(path)), "{path}");
        }
        for path in [
            r"C:\자료\제외2\file",
            r"C:\자료\제외-보관",
            r"\\server\share\secretary",
        ] {
            assert!(!rules.matches(Path::new(path)), "{path}");
        }
        assert!(FolderExclusions::new(&[r"C:\".into()]).matches(Path::new(r"C:\anything")));
        assert!(!FolderExclusions::new(&[r"C:\".into()]).matches(Path::new(r"D:\anything")));
    }

    #[test]
    fn filters_folder_row_and_descendants_but_keeps_similar_sibling_and_ids() {
        let folders = vec![
            r"C:\資料".into(),
            r"C:\資料\제외".into(),
            r"C:\資料\제외\하위".into(),
            r"C:\資料\제외2".into(),
        ];
        let entries = [
            ("제외", 0, true),
            ("제외2", 0, true),
            ("hidden.txt", 1, false),
            ("nested.txt", 2, false),
            ("visible.txt", 3, false),
        ]
        .into_iter()
        .map(|(name, parent, is_dir)| Entry {
            name: name.into(),
            parent,
            is_dir,
            size: None,
            created: None,
            modified: None,
        })
        .collect();
        let mut index = Index::from_parts(entries, folders);
        let rules = FolderExclusions::new(&[r"c:\資料\제외".into()]);
        assert_eq!(rules.apply(&mut index), 3);
        assert_eq!(
            index.entries.iter().map(|e| e.name).collect::<Vec<_>>(),
            ["제외2", "visible.txt"]
        );
        assert_eq!(index.entry(1).parent, 3);
        assert_eq!(index.excluded_folders, rules.paths());
        assert_eq!(rules.apply(&mut index), 0);
    }
}
