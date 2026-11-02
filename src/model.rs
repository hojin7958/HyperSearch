//! Compact 28-byte file records, deduplicated names, and shared folder trees.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::OnceLock,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub parent: u32,
    pub size: Option<u64>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub is_dir: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct EntryView<'a> {
    pub name: &'a str,
    pub parent: u32,
    pub size: Option<u64>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub is_dir: bool,
}
impl EntryView<'_> {
    pub fn to_owned(self) -> Entry {
        Entry {
            name: self.name.to_owned(),
            parent: self.parent,
            size: self.size,
            created: self.created,
            modified: self.modified,
            is_dir: self.is_dir,
        }
    }
}
// Splitting size into u32 words avoids u64 alignment padding.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub(crate) struct Record(pub(crate) [u32; 7]);
const NO_TIME: u32 = u32::MAX;
const WIDE_TIME: u32 = u32::MAX - 1;
const NO_NODE: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub struct EntryStore {
    pub(crate) records: Vec<Record>,
    pub(crate) names: Vec<u8>,
    pub(crate) wide_times: Vec<(u32, i64)>,
    intern: Option<HashMap<u64, u32>>,
    garbage_hint: usize,
}
impl EntryStore {
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    pub fn get(&self, row: usize) -> Option<EntryView<'_>> {
        (row < self.len()).then(|| self.entry(row))
    }
    pub fn entry(&self, row: usize) -> EntryView<'_> {
        let r = self.records[row].0;
        EntryView {
            name: self.name(r[0]),
            parent: r[1],
            size: (r[6] & 2 != 0).then_some(u64::from(r[2]) | (u64::from(r[3]) << 32)),
            created: self.time(row, false, r[4]),
            modified: self.time(row, true, r[5]),
            is_dir: r[6] & 1 != 0,
        }
    }
    fn time(&self, row: usize, modified: bool, value: u32) -> Option<i64> {
        match value {
            NO_TIME => None,
            WIDE_TIME => {
                let key = row as u32 * 2 + u32::from(modified);
                self.wide_times
                    .binary_search_by_key(&key, |&(k, _)| k)
                    .ok()
                    .map(|i| self.wide_times[i].1)
            }
            _ => Some(i64::from(value)),
        }
    }
    fn encode_time(&mut self, row: usize, modified: bool, value: Option<i64>) -> u32 {
        match value {
            None => NO_TIME,
            Some(t) if (0..i64::from(WIDE_TIME)).contains(&t) => t as u32,
            Some(t) => {
                self.wide_times
                    .push((row as u32 * 2 + u32::from(modified), t));
                WIDE_TIME
            }
        }
    }
    pub fn push(&mut self, entry: Entry) {
        self.push_view(EntryView {
            name: &entry.name,
            parent: entry.parent,
            size: entry.size,
            created: entry.created,
            modified: entry.modified,
            is_dir: entry.is_dir,
        });
    }
    pub fn push_view(&mut self, entry: EntryView<'_>) {
        let name = self.intern_name(entry.name);
        let row = self.len();
        let created = self.encode_time(row, false, entry.created);
        let modified = self.encode_time(row, true, entry.modified);
        let size = entry.size.unwrap_or(0);
        self.records.push(Record([
            name,
            entry.parent,
            size as u32,
            (size >> 32) as u32,
            created,
            modified,
            u32::from(entry.is_dir) | (u32::from(entry.size.is_some()) << 1),
        ]));
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = EntryView<'_>> + '_ {
        (0..self.len()).map(|row| self.entry(row))
    }
    pub fn retain(&mut self, mut keep: impl FnMut(EntryView<'_>) -> bool) {
        let mut to = 0;
        let mut wide = Vec::with_capacity(self.wide_times.len());
        for from in 0..self.len() {
            let view = self.entry(from);
            if keep(view) {
                if self.records[from].0[4] == WIDE_TIME {
                    wide.push((to as u32 * 2, view.created.unwrap()));
                }
                if self.records[from].0[5] == WIDE_TIME {
                    wide.push((to as u32 * 2 + 1, view.modified.unwrap()));
                }
                self.records[to] = self.records[from];
                to += 1;
            } else {
                self.garbage_hint = self.garbage_hint.saturating_add(view.name.len() + 4);
            }
        }
        self.records.truncate(to);
        self.wide_times = wide;
    }
    pub fn set_parent(&mut self, row: usize, parent: u32) {
        self.records[row].0[1] = parent;
    }
    pub fn compact(&mut self) {
        self.intern = None;
        if self.garbage_hint >= 8 * 1024 * 1024 && self.garbage_hint >= self.names.len() / 4 {
            self.reclaim_names();
        }
        self.records.shrink_to_fit();
        self.names.shrink_to_fit();
        self.wide_times.shrink_to_fit();
    }
    /// Repack only after material deletion churn. A bit mask and sorted offset
    /// remap keep this bounded; neither a second name pool nor a full hash table
    /// is needed while the existing index remains resident.
    fn reclaim_names(&mut self) {
        self.garbage_hint = 0;
        let mut live = vec![0u64; self.names.len().div_ceil(64)];
        for record in &self.records {
            let offset = record.0[0] as usize;
            live[offset / 64] |= 1u64 << (offset % 64);
        }
        let (mut from, mut to) = (0usize, 0usize);
        let mut remap = Vec::<(u32, u32)>::new();
        while from < self.names.len() {
            let length =
                4 + u32::from_le_bytes(self.names[from..from + 4].try_into().unwrap()) as usize;
            if live[from / 64] & (1u64 << (from % 64)) != 0 {
                if from != to {
                    self.names.copy_within(from..from + length, to);
                    remap.push((from as u32, to as u32));
                }
                to += length;
            }
            from += length;
        }
        drop(live);
        for record in &mut self.records {
            if let Ok(position) = remap.binary_search_by_key(&record.0[0], |&(old, _)| old) {
                record.0[0] = remap[position].1;
            }
        }
        self.names.truncate(to);
    }
    pub fn allocated_bytes(&self) -> usize {
        self.records.capacity() * 28
            + self.names.capacity()
            + self.wide_times.capacity() * 16
            + self.intern.as_ref().map_or(0, |m| m.capacity() * 24)
    }
    pub(crate) fn name(&self, offset: u32) -> &str {
        let start = offset as usize;
        let length = u32::from_le_bytes(self.names[start..start + 4].try_into().unwrap()) as usize;
        // Pools are populated from str or validated cache bytes only.
        unsafe { std::str::from_utf8_unchecked(&self.names[start + 4..start + 4 + length]) }
    }
    fn intern_name(&mut self, value: &str) -> u32 {
        if self.intern.is_none() {
            if !self.names.is_empty() {
                // A compact/loaded index receives a few watcher updates at a
                // time. Rebuilding the all-filename hash table for each edit
                // would temporarily allocate tens of megabytes. Append those
                // small changes directly; deletion churn is reclaimed later.
                let offset = u32::try_from(self.names.len()).expect("name pool exceeds 4 GiB");
                self.names
                    .extend_from_slice(&(value.len() as u32).to_le_bytes());
                self.names.extend_from_slice(value.as_bytes());
                return offset;
            }
            self.intern = Some(HashMap::new());
        }
        let mut hash = hash_name(0, value);
        while let Some(&offset) = self.intern.as_ref().unwrap().get(&hash) {
            if self.name(offset) == value {
                return offset;
            }
            hash = hash.wrapping_add(1);
        }
        let offset = u32::try_from(self.names.len()).expect("name pool exceeds 4 GiB");
        self.names
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.names.extend_from_slice(value.as_bytes());
        self.intern.as_mut().unwrap().insert(hash, offset);
        offset
    }
}
impl FromIterator<Entry> for EntryStore {
    fn from_iter<T: IntoIterator<Item = Entry>>(iter: T) -> Self {
        let iter = iter.into_iter();
        let mut store = Self::default();
        store.records.reserve(iter.size_hint().0);
        for entry in iter {
            store.push(entry);
        }
        store
    }
}
impl IntoIterator for EntryStore {
    type Item = Entry;
    type IntoIter = OwnedEntries;
    fn into_iter(self) -> Self::IntoIter {
        OwnedEntries {
            store: self,
            row: 0,
        }
    }
}
pub struct OwnedEntries {
    store: EntryStore,
    row: usize,
}
impl Iterator for OwnedEntries {
    type Item = Entry;
    fn next(&mut self) -> Option<Entry> {
        let entry = self.store.get(self.row)?.to_owned();
        self.row += 1;
        Some(entry)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.store.len() - self.row;
        (n, Some(n))
    }
}
impl ExactSizeIterator for OwnedEntries {}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FolderNode {
    pub(crate) parent: u32,
    pub(crate) offset: u32,
    pub(crate) len: u32,
}
#[derive(Clone, Copy, Debug)]
struct FolderLookup {
    low: u32,
    high: u32,
    id: u32,
}
impl FolderLookup {
    fn hash(self) -> u64 {
        u64::from(self.low) | (u64::from(self.high) << 32)
    }
}
#[derive(Clone, Debug, Default)]
pub struct FolderStore {
    pub(crate) ids: Vec<u32>,
    pub(crate) nodes: Vec<FolderNode>,
    pub(crate) names: Vec<u8>,
    intern: Option<HashMap<u64, u32>>,
    lookup: OnceLock<Vec<FolderLookup>>,
}
impl FolderStore {
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    /// Adds exactly one public folder ID, even when ancestors need new nodes.
    pub fn push(&mut self, value: String) {
        self.intern_path(&value);
    }
    pub fn intern_path(&mut self, value: &str) -> u32 {
        self.lookup.take();
        if self.intern.is_none() {
            let mut intern = HashMap::with_capacity(self.nodes.len());
            for (id, node) in self.nodes.iter().enumerate() {
                let mut hash = hash_name(node.parent, self.node_name(*node));
                while intern.contains_key(&hash) {
                    hash = hash.wrapping_add(1);
                }
                intern.insert(hash, id as u32);
            }
            self.intern = Some(intern);
        }
        // Pieces include original separators, preserving UNC and trailing roots.
        let mut parent = NO_NODE;
        let mut start = 0;
        for (offset, c) in value.char_indices() {
            if c == '\\' || c == '/' {
                parent = self.intern_node(parent, &value[start..offset + c.len_utf8()]);
                start = offset + c.len_utf8();
            }
        }
        if start < value.len() {
            parent = self.intern_node(parent, &value[start..]);
        }
        if parent == NO_NODE {
            parent = self.intern_node(parent, value);
        }
        let id = self.ids.len() as u32;
        self.ids.push(parent);
        id
    }
    fn intern_node(&mut self, parent: u32, value: &str) -> u32 {
        let mut hash = hash_name(parent, value);
        while let Some(&id) = self.intern.as_ref().unwrap().get(&hash) {
            let node = self.nodes[id as usize];
            if node.parent == parent && self.node_name(node) == value {
                return id;
            }
            hash = hash.wrapping_add(1);
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(FolderNode {
            parent,
            offset: self.names.len() as u32,
            len: value.len() as u32,
        });
        self.names.extend_from_slice(value.as_bytes());
        self.intern.as_mut().unwrap().insert(hash, id);
        id
    }
    fn node_name(&self, node: FolderNode) -> &str {
        let start = node.offset as usize;
        unsafe { std::str::from_utf8_unchecked(&self.names[start..start + node.len as usize]) }
    }
    pub fn path(&self, folder: usize) -> String {
        let mut node = self.ids[folder];
        let mut pieces = Vec::with_capacity(16);
        let mut length = 0;
        while node != NO_NODE {
            let n = self.nodes[node as usize];
            let part = self.node_name(n);
            length += part.len();
            pieces.push(part);
            node = n.parent;
        }
        let mut path = String::with_capacity(length);
        for part in pieces.into_iter().rev() {
            path.push_str(part);
        }
        path
    }
    /// Reuse both buffers while searching many files. No complete folder-path
    /// string table is retained alongside the compact tree.
    pub(crate) fn write_path(&self, folder: usize, path: &mut String, ancestry: &mut Vec<u32>) {
        path.clear();
        ancestry.clear();
        let mut node = self.ids[folder];
        while node != NO_NODE {
            ancestry.push(node);
            node = self.nodes[node as usize].parent;
        }
        for &node in ancestry.iter().rev() {
            path.push_str(self.node_name(self.nodes[node as usize]));
        }
    }
    pub fn get(&self, folder: usize) -> Option<String> {
        (folder < self.len()).then(|| self.path(folder))
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = String> + '_ {
        (0..self.len()).map(|id| self.path(id))
    }
    /// Lazy twelve-byte-per-folder lookup. Construct paths one at a time so an
    /// ordinary file edit never needs a full map of allocated path strings.
    /// Hash collisions are verified against the original path before returning.
    pub fn find_path(&self, path: &str) -> Option<u32> {
        let normalized = normalized_folder(path);
        let hash = hash_name(1, &normalized);
        let lookup = self.lookup.get_or_init(|| {
            let mut lookup: Vec<FolderLookup> = self
                .iter()
                .enumerate()
                .map(|(id, path)| {
                    let hash = hash_name(1, &normalized_folder(&path));
                    FolderLookup {
                        low: hash as u32,
                        high: (hash >> 32) as u32,
                        id: id as u32,
                    }
                })
                .collect();
            lookup.sort_unstable_by_key(|key| (key.hash(), key.id));
            lookup
        });
        let start = lookup.partition_point(|key| key.hash() < hash);
        lookup[start..]
            .iter()
            .take_while(|key| key.hash() == hash)
            .find(|key| normalized_folder(&self.path(key.id as usize)) == normalized)
            .map(|key| key.id)
    }
    pub fn compact(&mut self) {
        self.intern = None;
        self.ids.shrink_to_fit();
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
    }
    pub fn allocated_bytes(&self) -> usize {
        self.ids.capacity() * 4
            + self.nodes.capacity() * 12
            + self.names.capacity()
            + self.intern.as_ref().map_or(0, |m| m.capacity() * 24)
            + self.lookup.get().map_or(0, |lookup| lookup.capacity() * 12)
    }
}
impl FromIterator<String> for FolderStore {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        let mut folders = Self::default();
        for folder in iter {
            folders.push(folder);
        }
        folders
    }
}
impl From<Vec<String>> for FolderStore {
    fn from(value: Vec<String>) -> Self {
        value.into_iter().collect()
    }
}
fn normalized_folder(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}
fn hash_name(parent: u32, value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parent.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}
#[derive(Clone, Debug, Default)]
pub struct Index {
    pub entries: EntryStore,
    pub folders: FolderStore,
    pub roots: Vec<String>,
    pub excluded_folders: Vec<String>,
    pub scan_ms: u64,
    pub skipped: u64,
    pub built_at: i64,
}
impl Index {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn entry(&self, row: usize) -> EntryView<'_> {
        self.entries.entry(row)
    }
    pub fn from_parts(entries: Vec<Entry>, folders: Vec<String>) -> Self {
        let mut result = Self {
            entries: entries.into_iter().collect(),
            folders: folders.into(),
            ..Self::default()
        };
        result.compact();
        result
    }
    pub fn compact(&mut self) {
        self.entries.compact();
        self.folders.compact();
    }
    pub fn allocated_bytes(&self) -> usize {
        self.entries.allocated_bytes() + self.folders.allocated_bytes()
    }
    pub fn full_path(&self, row: usize) -> String {
        let e = self.entry(row);
        let mut path = self.folders.path(e.parent as usize);
        if !path.ends_with(['\\', '/']) {
            path.push('\\');
        }
        path.push_str(e.name);
        path
    }
    pub fn parent_path(&self, row: usize) -> String {
        self.folders.path(self.entry(row).parent as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_records_are_28_bytes_and_lossless() {
        assert_eq!(std::mem::size_of::<Record>(), 28);
        let mut index = Index::default();
        index.folders.push("C:\\자료\\".into());
        for time in [None, Some(-1), Some(0), Some(4_294_967_294), Some(i64::MAX)] {
            index.entries.push(Entry {
                name: "동일.PDF".into(),
                parent: 0,
                size: Some(u64::MAX),
                created: time,
                modified: time,
                is_dir: false,
            });
        }
        index.compact();
        assert_eq!(index.entries.names.len(), 4 + "동일.PDF".len());
        assert_eq!(index.entry(1).created, Some(-1));
        assert_eq!(index.entry(4).modified, Some(i64::MAX));
        assert_eq!(index.entry(0).size, Some(u64::MAX));
        assert_eq!(index.full_path(0), "C:\\자료\\동일.PDF");
        index
            .entries
            .retain(|e| e.created.is_some_and(|t| t < 0 || t > 100));
        assert_eq!(index.entry(0).created, Some(-1));
        assert_eq!(index.entry(2).modified, Some(i64::MAX));
    }
    #[test]
    fn folders_share_ancestors_and_preserve_ids_and_spelling() {
        let paths = [
            "C:\\",
            "C:\\자료\\보고서",
            "C:\\자료\\사진",
            "\\\\server\\share\\x",
            "C:/mixed\\path/",
        ];
        let mut folders = FolderStore::default();
        for path in paths {
            folders.push(path.into());
        }
        folders.compact();
        assert_eq!(folders.iter().collect::<Vec<_>>(), paths);
        folders.push("C:\\자료\\보고서\\새폴더".into());
        assert_eq!(folders.path(5), "C:\\자료\\보고서\\새폴더");
    }
    #[test]
    fn lazy_folder_lookup_handles_windows_spelling_and_invalidation() {
        let mut folders = FolderStore::default();
        for path in ["C:\\", "C:\\자료\\Reports", "\\\\server\\Share\\한글"] {
            folders.push(path.into());
        }
        assert_eq!(folders.find_path("c:/자료/reports/"), Some(1));
        assert_eq!(folders.find_path("C:"), Some(0));
        assert_eq!(folders.find_path("\\\\SERVER\\share\\한글\\"), Some(2));
        assert_eq!(folders.find_path("C:\\자료\\missing"), None);
        assert_eq!(std::mem::size_of::<FolderLookup>(), 12);
        folders.push("C:\\자료\\New".into());
        folders.compact();
        assert_eq!(folders.find_path("c:\\자료\\new"), Some(3));
    }
    #[test]
    fn incremental_append_does_not_rebuild_all_filename_hashes() {
        let mut store = EntryStore::default();
        let entry = Entry {
            name: "old.txt".into(),
            parent: 0,
            size: None,
            created: None,
            modified: None,
            is_dir: false,
        };
        store.push(entry.clone());
        store.compact();
        store.push(Entry {
            name: "new.txt".into(),
            ..entry
        });
        assert!(store.intern.is_none());
        assert_eq!(store.entry(1).name, "new.txt");
    }
    #[test]
    fn name_reclamation_preserves_shared_names_and_wide_dates() {
        let mut store = EntryStore::default();
        for (name, created) in [
            ("discard", -1),
            ("shared", -2),
            ("discard2", -3),
            ("shared", -4),
            ("last", -5),
        ] {
            store.push(Entry {
                name: name.into(),
                parent: 0,
                size: None,
                created: Some(created),
                modified: None,
                is_dir: false,
            });
        }
        store.retain(|e| !e.name.starts_with("discard"));
        store.garbage_hint = usize::MAX;
        store.compact();
        assert_eq!(
            store.iter().map(|e| e.name).collect::<Vec<_>>(),
            ["shared", "shared", "last"]
        );
        assert_eq!(store.names.len(), 4 + "shared".len() + 4 + "last".len());
        assert_eq!(store.entry(1).created, Some(-4));
    }
}
