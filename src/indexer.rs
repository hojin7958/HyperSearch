//! Windows directory indexing without a separate metadata request for every file.
//!
//! Ordinary-permission scanner and watcher updates. The separate `ntfs` module
//! implements the elevated, read-only MFT/USN fast path.

use crate::exclusions::FolderExclusions;
use crate::model::{Entry, Index};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
pub struct ScanProgress {
    pub entries: usize,
    pub folders: usize,
    pub skipped: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct NtfsProbe {
    /// Number of MFT records returned, including filesystem-owned records.
    pub entries: u64,
    pub elapsed_ms: u64,
    pub journal_available: bool,
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_INVALID_PARAMETER,
        ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND, FILETIME, GENERIC_READ,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
        FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetDriveTypeW, GetLogicalDrives,
        GetVolumeInformationW, OPEN_EXISTING, WIN32_FIND_DATAW,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, MFT_ENUM_DATA_V0,
    };

    struct ScanPacer {
        background: bool,
        slice: Instant,
    }
    impl ScanPacer {
        fn new(background: bool) -> Self {
            Self {
                background,
                slice: Instant::now(),
            }
        }
        fn checkpoint(&mut self, cancel: &AtomicBool) -> Result<(), String> {
            if cancel.load(Ordering::Relaxed) {
                return Err("인덱싱을 취소했습니다.".into());
            }
            if self.background && self.slice.elapsed() >= Duration::from_millis(8) {
                std::thread::sleep(Duration::from_millis(1));
                self.slice = Instant::now();
            }
            Ok(())
        }
    }

    struct FindHandle(HANDLE);
    impl Drop for FindHandle {
        fn drop(&mut self) {
            unsafe { FindClose(self.0) };
        }
    }

    struct VolumeHandle(HANDLE);
    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn error_text(code: u32) -> String {
        std::io::Error::from_raw_os_error(code as i32).to_string()
    }

    // Canonical Windows paths use \\?\; keep that prefix for API calls, but
    // present ordinary drive and UNC paths in the result/clipboard.
    pub(super) fn display_path(path: &Path) -> String {
        let s = path.to_string_lossy();
        if let Some(tail) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{tail}")
        } else {
            s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
        }
    }

    fn timestamp(time: FILETIME) -> Option<i64> {
        let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        if ticks == 0 {
            None
        } else {
            Some((ticks / 10_000_000) as i64 - 11_644_473_600)
        }
    }

    fn root_key(path: &Path) -> String {
        display_path(path)
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }

    fn is_same_or_child(path: &str, ancestor: &str) -> bool {
        path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|suffix| suffix.starts_with('\\'))
    }

    fn normalize_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
        if roots.is_empty() {
            return Err("검색할 폴더가 없습니다.".into());
        }
        let mut candidates = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = std::fs::canonicalize(root)
                .map_err(|err| format!("검색 폴더를 열 수 없습니다: {} ({err})", root.display()))?;
            if !canonical.is_dir() {
                return Err(format!("폴더가 아닙니다: {}", root.display()));
            }
            candidates.push((root_key(&canonical), canonical));
        }
        candidates.sort_unstable_by(|a, b| a.0.len().cmp(&b.0.len()).then(a.0.cmp(&b.0)));
        let mut selected: Vec<(String, PathBuf)> = Vec::with_capacity(candidates.len());
        for (key, path) in candidates {
            if !selected
                .iter()
                .any(|(existing, _)| is_same_or_child(&key, existing))
            {
                selected.push((key, path));
            }
        }
        Ok(selected.into_iter().map(|(_, path)| path).collect())
    }

    pub fn default_roots() -> Vec<PathBuf> {
        let mask = unsafe { GetLogicalDrives() };
        (0_u8..26)
            .filter(|i| mask & (1_u32 << i) != 0)
            .filter_map(|i| {
                let path = format!("{}:\\", char::from(b'A' + i));
                let root = wide(OsStr::new(&path));
                // DRIVE_FIXED = 3 (winbase.h); no WindowsProgramming feature required.
                (unsafe { GetDriveTypeW(root.as_ptr()) } == 3).then(|| path.into())
            })
            .collect()
    }

    pub fn scan(
        roots: &[PathBuf],
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
    ) -> Result<Index, String> {
        scan_with_exclusions(roots, &[], cancel, progress)
    }

    pub fn scan_with_exclusions(
        roots: &[PathBuf],
        excluded_folders: &[PathBuf],
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
    ) -> Result<Index, String> {
        scan_with_unreadable(roots, excluded_folders, cancel, progress, |_| {}, false)
    }

    fn scan_with_unreadable(
        roots: &[PathBuf],
        excluded_folders: &[PathBuf],
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
        mut unreadable: impl FnMut(&Path),
        background: bool,
    ) -> Result<Index, String> {
        let started = Instant::now();
        let mut pacer = ScanPacer::new(background);
        let roots = normalize_roots(roots)?;
        let exclusions = FolderExclusions::new(excluded_folders);
        let mut index = Index {
            roots: roots.iter().map(|p| display_path(p)).collect(),
            excluded_folders: exclusions.paths().to_vec(),
            built_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            ..Index::default()
        };
        let mut pending = Vec::new();
        for root in roots {
            if exclusions.matches(&root) {
                continue;
            }
            let id = index.folders.len() as u32;
            index.folders.push(display_path(&root));
            pending.push((root, id));
        }
        let mut last_progress = Instant::now();
        progress(ScanProgress {
            entries: 0,
            folders: index.folders.len(),
            skipped: 0,
            elapsed_ms: 0,
        });
        while let Some((directory, parent)) = pending.pop() {
            pacer.checkpoint(cancel)?;
            let pattern = wide(directory.join("*").as_os_str());
            let mut data: WIN32_FIND_DATAW = unsafe { zeroed() };
            let mut handle = unsafe {
                FindFirstFileExW(
                    pattern.as_ptr(),
                    FindExInfoBasic,
                    (&mut data as *mut WIN32_FIND_DATAW).cast(),
                    FindExSearchNameMatch,
                    null(),
                    FIND_FIRST_EX_LARGE_FETCH,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let code = unsafe { GetLastError() };
                // Some filesystem providers do not support the large-buffer flag.
                if code == ERROR_INVALID_PARAMETER || code == ERROR_NOT_SUPPORTED {
                    handle = unsafe {
                        FindFirstFileExW(
                            pattern.as_ptr(),
                            FindExInfoBasic,
                            (&mut data as *mut WIN32_FIND_DATAW).cast(),
                            FindExSearchNameMatch,
                            null(),
                            0,
                        )
                    };
                }
            }
            if handle == INVALID_HANDLE_VALUE {
                let code = unsafe { GetLastError() };
                if code != ERROR_FILE_NOT_FOUND
                    && code != ERROR_PATH_NOT_FOUND
                    && code != ERROR_NO_MORE_FILES
                {
                    index.skipped += 1;
                    unreadable(&directory);
                }
            } else {
                let search = FindHandle(handle);
                let mut checked = 0usize;
                loop {
                    if checked & 255 == 0 {
                        pacer.checkpoint(cancel)?;
                    }
                    checked += 1;
                    let length = data
                        .cFileName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(data.cFileName.len());
                    let name_wide = &data.cFileName[..length];
                    if name_wide != [b'.' as u16] && name_wide != [b'.' as u16, b'.' as u16] {
                        let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                        let child = (is_dir || !exclusions.paths().is_empty())
                            .then(|| directory.join(OsString::from_wide(name_wide)));
                        if child.as_ref().is_some_and(|path| exclusions.matches(path)) {
                            // Still advance FindNextFile below; never traverse
                            // or publish an excluded folder or its contents.
                        } else {
                            let is_reparse =
                                data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                            index.entries.push(Entry {
                                name: String::from_utf16_lossy(name_wide),
                                parent,
                                size: (!is_dir).then_some(
                                    (u64::from(data.nFileSizeHigh) << 32)
                                        | u64::from(data.nFileSizeLow),
                                ),
                                created: timestamp(data.ftCreationTime),
                                modified: timestamp(data.ftLastWriteTime),
                                is_dir,
                            });
                            if is_dir {
                                if is_reparse {
                                    // Keep the junction/symlink searchable, but never follow it.
                                    index.skipped += 1;
                                } else {
                                    let id = u32::try_from(index.folders.len()).map_err(|_| {
                                        "폴더 수가 지원 한도를 초과했습니다.".to_owned()
                                    })?;
                                    let child = child.expect("directories have a child path");
                                    index.folders.push(display_path(&child));
                                    pending.push((child, id));
                                }
                            }
                        }
                    }
                    if index.entries.len() & 127 == 0 && cancel.load(Ordering::Relaxed) {
                        return Err("인덱싱을 취소했습니다.".into());
                    }
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        progress(ScanProgress {
                            entries: index.entries.len(),
                            folders: index.folders.len(),
                            skipped: index.skipped,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        });
                        last_progress = Instant::now();
                    }
                    if unsafe { FindNextFileW(search.0, &mut data) } == 0 {
                        if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
                            index.skipped += 1;
                            unreadable(&directory);
                        }
                        break;
                    }
                }
            }
            if last_progress.elapsed() >= Duration::from_millis(100) {
                progress(ScanProgress {
                    entries: index.entries.len(),
                    folders: index.folders.len(),
                    skipped: index.skipped,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
                last_progress = Instant::now();
            }
        }
        index.compact();
        index.scan_ms = started.elapsed().as_millis() as u64;
        progress(ScanProgress {
            entries: index.entries.len(),
            folders: index.folders.len(),
            skipped: index.skipped,
            elapsed_ms: index.scan_ms,
        });
        Ok(index)
    }

    /// Recover lost watcher notifications without elevation. Keep old records
    /// under directories that cannot currently be read (ACLs/sharing errors).
    pub fn rescan(
        previous: &Index,
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
    ) -> Result<Index, String> {
        let roots = previous.roots.iter().map(PathBuf::from).collect::<Vec<_>>();
        let excluded = previous
            .excluded_folders
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let mut unreadable = std::collections::HashSet::new();
        let current = scan_with_unreadable(
            &roots,
            &excluded,
            cancel,
            progress,
            |path| {
                unreadable.insert(root_key(path));
            },
            true,
        )?;
        preserve_unreadable(previous, current, &unreadable, cancel)
    }

    fn preserve_unreadable(
        previous: &Index,
        mut current: Index,
        unreadable: &std::collections::HashSet<String>,
        cancel: &AtomicBool,
    ) -> Result<Index, String> {
        use std::collections::{HashMap, HashSet};
        if unreadable.is_empty() {
            return Ok(current);
        }
        let is_unreadable = |path: &str| {
            unreadable.contains(path)
                || path
                    .match_indices('\\')
                    .any(|(at, _)| unreadable.contains(&path[..at]))
        };
        // Only retain a path map for unreadable subtrees, not the entire drive.
        let mut current_ids = HashMap::new();
        let mut existing: HashMap<u32, HashSet<String>> = HashMap::new();
        for (id, path) in current.folders.iter().enumerate() {
            let key = root_key(Path::new(&path));
            if is_unreadable(&key) {
                current_ids.insert(key, id as u32);
                existing.insert(id as u32, HashSet::new());
            }
        }
        for entry in current.entries.iter() {
            if let Some(names) = existing.get_mut(&entry.parent) {
                names.insert(entry.name.to_lowercase());
            }
        }
        let exclusions = FolderExclusions::from_strings(&current.excluded_folders);
        let mut remap = HashMap::new();
        for (id, path) in previous.folders.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("인덱싱을 취소했습니다.".into());
            }
            let key = root_key(Path::new(&path));
            if is_unreadable(&key) && !exclusions.matches(Path::new(&path)) {
                let new_id = *current_ids.entry(key).or_insert_with(|| {
                    let new_id = current.folders.len() as u32;
                    current.folders.push(path);
                    new_id
                });
                remap.insert(id as u32, new_id);
            }
        }
        for (row, entry) in previous.entries.iter().enumerate() {
            if row & 127 == 0 && cancel.load(Ordering::Relaxed) {
                return Err("인덱싱을 취소했습니다.".into());
            }
            if let Some(&parent) = remap.get(&entry.parent) {
                if exclusions.matches(Path::new(&previous.full_path(row))) {
                    continue;
                }
                if !existing
                    .entry(parent)
                    .or_default()
                    .insert(entry.name.to_lowercase())
                {
                    continue;
                }
                let mut entry = entry.to_owned();
                entry.parent = parent;
                current.entries.push(entry);
            }
        }
        current.compact();
        Ok(current)
    }

    fn api_path(path: &Path) -> PathBuf {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\?\") {
            path.to_owned()
        } else if let Some(tail) = text.strip_prefix(r"\\") {
            PathBuf::from(format!(r"\\?\UNC\{tail}"))
        } else {
            PathBuf::from(format!(r"\\?\{text}"))
        }
    }

    enum CurrentEntry {
        Found(WIN32_FIND_DATAW),
        Missing,
        // An ACL/sharing error says nothing about whether the item was deleted.
        // Preserve its previous record and let a later notification refresh it.
        Unreadable,
    }

    fn classify_find_error(code: u32) -> Result<CurrentEntry, String> {
        match code {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(CurrentEntry::Missing),
            5 | 32 | 33 => Ok(CurrentEntry::Unreadable),
            _ => Err(format!(
                "변경된 항목을 읽을 수 없습니다: {}",
                error_text(code)
            )),
        }
    }

    fn find_one(path: &Path) -> Result<CurrentEntry, String> {
        let pattern = wide(api_path(path).as_os_str());
        let mut data: WIN32_FIND_DATAW = unsafe { zeroed() };
        let raw = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                (&mut data as *mut WIN32_FIND_DATAW).cast(),
                FindExSearchNameMatch,
                null(),
                0,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            classify_find_error(code)
        } else {
            let _handle = FindHandle(raw);
            Ok(CurrentEntry::Found(data))
        }
    }

    /// Cheap preflight for the app: access and root-directory timestamp changes
    /// need neither an index clone nor a search-engine rebuild.
    pub fn has_relevant_changes(index: &Index, events: &[notify::Event]) -> bool {
        use notify::{EventKind, event::ModifyKind};
        let roots: Vec<String> = index.roots.iter().map(|p| root_key(Path::new(p))).collect();
        let exclusions = FolderExclusions::from_strings(&index.excluded_folders);
        events.iter().any(|event| {
            if event.need_rescan() {
                return true;
            }
            if matches!(event.kind, EventKind::Access(_)) {
                return false;
            }
            if matches!(event.kind, EventKind::Any | EventKind::Other) || event.paths.is_empty() {
                return true;
            }
            event.paths.iter().any(|path| {
                let Ok(path) = std::path::absolute(path) else {
                    return true;
                };
                let key = root_key(&path);
                !exclusions.matches(&path)
                    && roots.iter().any(|root| is_same_or_child(&key, root))
                    && !(roots.contains(&key)
                        && matches!(event.kind,
                        EventKind::Modify(kind) if !matches!(kind, ModifyKind::Name(_))))
            })
        })
    }

    /// Worker-only preflight: repeated timestamp/data notifications whose
    /// indexed metadata is unchanged must not discard the engine's sort cache.
    /// Structural or uncertain notifications conservatively require an update.
    pub fn metadata_changes_needed(index: &Index, events: &[notify::Event]) -> bool {
        use notify::{EventKind, event::ModifyKind};
        use std::collections::HashMap;
        let roots: Vec<String> = index.roots.iter().map(|p| root_key(Path::new(p))).collect();
        let mut current: HashMap<u32, HashMap<String, CurrentEntry>> = HashMap::new();
        for event in events {
            if event.need_rescan() {
                return true;
            }
            match event.kind {
                EventKind::Access(_) => continue,
                EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_)) => {}
                _ => return true,
            }
            if event.paths.is_empty() {
                return true;
            }
            for path in &event.paths {
                let Ok(path) = std::path::absolute(path) else {
                    return true;
                };
                let key = root_key(&path);
                if roots.contains(&key) || !roots.iter().any(|r| is_same_or_child(&key, r)) {
                    continue;
                }
                let Some((parent, name)) = key.rsplit_once('\\') else {
                    return true;
                };
                let Some(id) = index.folders.find_path(parent) else {
                    return true;
                };
                let value = match find_one(&path) {
                    Ok(CurrentEntry::Unreadable) => continue,
                    Ok(value) => value,
                    Err(_) => return true,
                };
                current
                    .entry(id)
                    .or_default()
                    .insert(name.to_owned(), value);
            }
        }
        if current.is_empty() {
            return false;
        }
        for entry in index.entries.iter() {
            let Some(names) = current.get_mut(&entry.parent) else {
                continue;
            };
            let Some(value) = names.remove(entry.name.to_lowercase().as_str()) else {
                continue;
            };
            match value {
                CurrentEntry::Found(data) => {
                    let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let size = (!is_dir).then_some(
                        (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                    );
                    if is_dir != entry.is_dir
                        || size != entry.size
                        || timestamp(data.ftCreationTime) != entry.created
                        || timestamp(data.ftLastWriteTime) != entry.modified
                    {
                        return true;
                    }
                }
                CurrentEntry::Missing => return true,
                CurrentEntry::Unreadable => {}
            }
        }
        // New files sometimes arrive with metadata-only notifications. A
        // missing filesystem item with no indexed record needs no change.
        current
            .values()
            .flat_map(|names| names.values())
            .any(|v| matches!(v, CurrentEntry::Found(_)))
    }

    /// Apply a debounced watcher batch. Ordinary file edits query only that
    /// file; directory creation/moves enumerate only the affected subtree.
    /// Return an error for uncertain/overflow events so the UI can request a
    /// rebuild rather than silently leave a stale index.
    pub fn apply_changes(index: &Index, events: &[notify::Event]) -> Result<Index, String> {
        apply_changes_owned(index.clone(), events)
    }

    pub fn apply_changes_owned(index: Index, events: &[notify::Event]) -> Result<Index, String> {
        apply_changes_with(index, events, find_one)
    }

    pub fn apply_changes_cancellable(
        index: &Index,
        events: &[notify::Event],
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
    ) -> Result<Index, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("인덱싱을 취소했습니다.".into());
        }
        apply_changes_controlled(index.clone(), events, find_one, cancel, progress, true)
    }

    fn apply_changes_with(
        index: Index,
        events: &[notify::Event],
        lookup: impl Fn(&Path) -> Result<CurrentEntry, String>,
    ) -> Result<Index, String> {
        apply_changes_controlled(
            index,
            events,
            lookup,
            &AtomicBool::new(false),
            |_| {},
            false,
        )
    }

    fn apply_changes_controlled(
        index: Index,
        events: &[notify::Event],
        lookup: impl Fn(&Path) -> Result<CurrentEntry, String>,
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
        background: bool,
    ) -> Result<Index, String> {
        use notify::{EventKind, event::ModifyKind};
        use std::collections::{HashMap, HashSet};

        struct Refresh {
            key: String,
            path: PathBuf,
            recursive: bool,
            unreadable: bool,
        }

        let roots: Vec<String> = index.roots.iter().map(|p| root_key(Path::new(p))).collect();
        let mut collected: HashMap<String, Refresh> = HashMap::new();
        let exclusions = FolderExclusions::from_strings(&index.excluded_folders);
        let excluded_folders: Vec<PathBuf> =
            index.excluded_folders.iter().map(PathBuf::from).collect();
        for event in events {
            if event.need_rescan() {
                return Err("파일 변경 알림이 누락되어 다시 인덱싱해야 합니다.".into());
            }
            let recursive = match event.kind {
                EventKind::Access(_) => continue,
                EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Name(_)) => true,
                EventKind::Modify(_) => false,
                EventKind::Any | EventKind::Other => {
                    return Err("파일 변경 종류를 확인할 수 없어 다시 인덱싱해야 합니다.".into());
                }
            };
            if event.paths.is_empty() {
                return Err("파일 변경 경로가 누락되어 다시 인덱싱해야 합니다.".into());
            }
            for path in &event.paths {
                let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
                let path = PathBuf::from(display_path(&absolute));
                if exclusions.matches(&path) {
                    continue;
                }
                let key = root_key(&path);
                if !roots.iter().any(|root| is_same_or_child(&key, root)) {
                    continue;
                }
                if roots.iter().any(|root| root == &key) {
                    // Updating a directory's timestamp does not change its children.
                    if !recursive {
                        continue;
                    }
                }
                collected
                    .entry(key.clone())
                    .and_modify(|old| {
                        old.recursive |= recursive;
                        old.path = path.clone();
                    })
                    .or_insert(Refresh {
                        key,
                        path,
                        recursive,
                        unreadable: false,
                    });
            }
        }
        let mut refreshes: Vec<Refresh> = collected.into_values().collect();
        refreshes.sort_unstable_by(|a, b| a.key.len().cmp(&b.key.len()).then(a.key.cmp(&b.key)));
        let mut selected: Vec<Refresh> = Vec::with_capacity(refreshes.len());
        let mut recursive_ancestors = HashSet::new();
        for refresh in refreshes {
            if !refresh
                .key
                .match_indices('\\')
                .any(|(at, _)| recursive_ancestors.contains(&refresh.key[..at]))
            {
                if refresh.recursive {
                    recursive_ancestors.insert(refresh.key.clone());
                }
                selected.push(refresh);
            }
        }
        if selected.is_empty() {
            return Ok(index);
        }

        // Query current disk state even for remove notifications: a replacement
        // file may already have been created at the same path in this batch.
        let mut additions = Vec::new();
        let mut scanned = 0usize;
        let started = Instant::now();
        let mut pacer = ScanPacer::new(background);
        for refresh in &mut selected {
            pacer.checkpoint(cancel)?;
            let is_root = roots.contains(&refresh.key);
            let data = if is_root {
                WIN32_FIND_DATAW {
                    dwFileAttributes: FILE_ATTRIBUTE_DIRECTORY,
                    ..unsafe { zeroed() }
                }
            } else {
                match lookup(&refresh.path)? {
                    CurrentEntry::Found(data) => data,
                    CurrentEntry::Missing => {
                        refresh.recursive = true;
                        continue;
                    }
                    CurrentEntry::Unreadable => {
                        refresh.unreadable = true;
                        continue;
                    }
                }
            };
            let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let is_reparse = data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if !is_dir || is_reparse {
                // Replacing a directory with a file must remove its old children.
                refresh.recursive = true;
            }
            let subtree = if is_dir && !is_reparse && refresh.recursive {
                let mut unreadable = HashSet::new();
                match scan_with_unreadable(
                    &[refresh.path.clone()],
                    &excluded_folders,
                    cancel,
                    |mut p| {
                        p.entries += scanned;
                        p.elapsed_ms = started.elapsed().as_millis() as u64;
                        progress(p);
                    },
                    |path| {
                        unreadable.insert(root_key(path));
                    },
                    background,
                ) {
                    Ok(current) => {
                        scanned += current.len();
                        Some(preserve_unreadable(&index, current, &unreadable, cancel)?)
                    }
                    Err(error) => {
                        if cancel.load(Ordering::Relaxed) || is_root {
                            return Err(error);
                        }
                        refresh.unreadable = true;
                        continue;
                    }
                }
            } else {
                None
            };
            additions.push((refresh.path.clone(), data, subtree));
        }
        let unreadable = selected.iter().filter(|r| r.unreadable).count() as u64;
        selected.retain(|r| !r.unreadable);
        if selected.is_empty() {
            let mut unchanged = index;
            unchanged.skipped += unreadable;
            return Ok(unchanged);
        }

        // File edits/creates/deletes and directory timestamp updates do not
        // change the folder tree. Keep these frequent batches independent of
        // the total length of every indexed path: looking up a few parents
        // avoids materializing hundreds of thousands of strings and maps.
        let has_directory_subtree = additions.iter().any(|(path, data, subtree)| {
            subtree.is_some()
                || (data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    && selected.iter().any(|r| r.recursive && r.path == *path))
        });
        if !has_directory_subtree {
            let mut names: HashMap<u32, HashSet<&str>> = HashMap::new();
            let mut recursive_names: HashMap<u32, HashSet<&str>> = HashMap::new();
            let mut parents_found = true;
            for refresh in &selected {
                let Some((parent, name)) = refresh.key.rsplit_once('\\') else {
                    parents_found = false;
                    break;
                };
                let Some(id) = index.folders.find_path(parent) else {
                    parents_found = false;
                    break;
                };
                names.entry(id).or_default().insert(name);
                if refresh.recursive {
                    recursive_names.entry(id).or_default().insert(name);
                }
            }
            let removes_directory = parents_found
                && !recursive_names.is_empty()
                && index.entries.iter().any(|e| {
                    e.is_dir
                        && recursive_names
                            .get(&e.parent)
                            .is_some_and(|set| set.contains(e.name.to_lowercase().as_str()))
                });
            if parents_found && !removes_directory {
                let addition_parents: Option<Vec<u32>> = additions
                    .iter()
                    .map(|(path, _, _)| {
                        path.parent()
                            .and_then(|p| index.folders.find_path(&display_path(p)))
                    })
                    .collect();
                if let Some(addition_parents) = addition_parents {
                    let mut updated = index;
                    updated.entries.retain(|e| {
                        !names
                            .get(&e.parent)
                            .is_some_and(|set| set.contains(e.name.to_lowercase().as_str()))
                    });
                    for ((_, data, _), parent) in additions.into_iter().zip(addition_parents) {
                        let len = data
                            .cFileName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(data.cFileName.len());
                        let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                        updated.entries.push(Entry {
                            name: String::from_utf16_lossy(&data.cFileName[..len]),
                            parent,
                            size: (!is_dir).then_some(
                                (u64::from(data.nFileSizeHigh) << 32)
                                    | u64::from(data.nFileSizeLow),
                            ),
                            created: timestamp(data.ftCreationTime),
                            modified: timestamp(data.ftLastWriteTime),
                            is_dir,
                        });
                    }
                    updated.skipped += unreadable;
                    updated.built_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    updated.compact();
                    return Ok(updated);
                }
            }
        }

        let folder_keys: Vec<String> = index
            .folders
            .iter()
            .map(|p| root_key(Path::new(&p)))
            .collect();
        let recursive_keys: HashSet<&str> = selected
            .iter()
            .filter(|r| r.recursive)
            .map(|r| r.key.as_str())
            .collect();
        // Test ancestors against a hash set once per folder. Retaining millions
        // of entries must not multiply work by the number of watcher events.
        let recursive_removed: Vec<bool> = folder_keys
            .iter()
            .map(|key| {
                recursive_keys.contains(key.as_str())
                    || key
                        .match_indices('\\')
                        .any(|(position, _)| recursive_keys.contains(&key[..position]))
            })
            .collect();
        let old_folder_ids: HashMap<&str, u32> = folder_keys
            .iter()
            .enumerate()
            .map(|(id, key)| (key.as_str(), id as u32))
            .collect();
        let mut exact_names: HashMap<u32, HashSet<&str>> = HashMap::new();
        for refresh in &selected {
            if let Some((parent, name)) = refresh.key.rsplit_once('\\') {
                if let Some(&id) = old_folder_ids.get(parent) {
                    exact_names.entry(id).or_default().insert(name);
                }
            }
        }
        let mut updated = index;
        updated.entries.retain(|entry| {
            !recursive_removed[entry.parent as usize]
                && !exact_names
                    .get(&entry.parent)
                    .is_some_and(|names| names.contains(entry.name.to_lowercase().as_str()))
        });
        let mut folder_ids = HashMap::new();
        let mut reusable_folder_ids = HashMap::new();
        for (id, key) in folder_keys.into_iter().enumerate() {
            if recursive_removed[id] {
                reusable_folder_ids.insert(key, id as u32);
            } else {
                folder_ids.insert(key, id as u32);
            }
        }

        fn intern_folder(
            path: &str,
            folders: &mut crate::model::FolderStore,
            ids: &mut HashMap<String, u32>,
            reusable: &mut HashMap<String, u32>,
        ) -> Result<u32, String> {
            let key = root_key(Path::new(path));
            if let Some(&id) = ids.get(&key) {
                return Ok(id);
            }
            // Reuse the old public ID when a refreshed subtree contains the
            // same folder. Duplicate path IDs would make later file moves or
            // deletes target a stale ID instead of its current records.
            if let Some(id) = reusable.remove(&key) {
                ids.insert(key, id);
                return Ok(id);
            }
            let id = u32::try_from(folders.len())
                .map_err(|_| "폴더 수가 지원 한도를 초과했습니다.".to_owned())?;
            folders.push(path.to_owned());
            ids.insert(key, id);
            Ok(id)
        }

        for (path, data, subtree) in additions {
            let is_root = roots.contains(&root_key(&path));
            if !is_root {
                let parent = path
                    .parent()
                    .ok_or_else(|| "변경 경로의 상위 폴더가 없습니다.".to_owned())?;
                let parent_key = root_key(parent);
                // No indexed parent means this is inside a junction we deliberately
                // did not traverse, or a concurrent parent move. Never follow it.
                let Some(&parent_id) = folder_ids.get(&parent_key) else {
                    return Err("상위 폴더 변경이 누락되어 다시 인덱싱해야 합니다.".into());
                };
                let name_len = data
                    .cFileName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(data.cFileName.len());
                let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                updated.entries.push(Entry {
                    name: String::from_utf16_lossy(&data.cFileName[..name_len]),
                    parent: parent_id,
                    size: (!is_dir).then_some(
                        (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                    ),
                    created: timestamp(data.ftCreationTime),
                    modified: timestamp(data.ftLastWriteTime),
                    is_dir,
                });
            }
            let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            if is_dir && data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                intern_folder(
                    &display_path(&path),
                    &mut updated.folders,
                    &mut folder_ids,
                    &mut reusable_folder_ids,
                )?;
            }
            if let Some(subtree) = subtree {
                let remap: Vec<u32> = subtree
                    .folders
                    .iter()
                    .map(|p| {
                        intern_folder(
                            &p,
                            &mut updated.folders,
                            &mut folder_ids,
                            &mut reusable_folder_ids,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                for entry in subtree.entries.iter() {
                    if updated.len() & 4095 == 0 {
                        pacer.checkpoint(cancel)?;
                    }
                    let mut entry = entry.to_owned();
                    entry.parent = remap[entry.parent as usize];
                    updated.entries.push(entry);
                }
                updated.skipped += subtree.skipped;
            }
        }
        updated.skipped += unreadable;
        updated.compact();
        updated.built_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Ok(updated)
    }

    /// Read-only speed probe. No journal is created, deleted, or modified.
    pub fn probe_ntfs(root: &Path) -> Result<NtfsProbe, String> {
        let started = Instant::now();
        let canonical = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
        let path = display_path(&canonical);
        let bytes = path.as_bytes();
        if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
            return Err("NTFS 검사는 로컬 드라이브에서만 지원합니다.".into());
        }
        let volume_root = wide(OsStr::new(&format!("{}:\\", char::from(bytes[0]))));
        let mut filesystem = [0_u16; 32];
        let got_filesystem = unsafe {
            GetVolumeInformationW(
                volume_root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        };
        if got_filesystem == 0 {
            return Err(error_text(unsafe { GetLastError() }));
        }
        let fs_len = filesystem
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(filesystem.len());
        if !String::from_utf16_lossy(&filesystem[..fs_len]).eq_ignore_ascii_case("NTFS") {
            return Err("NTFS 드라이브가 아닙니다.".into());
        }
        let device = wide(OsStr::new(&format!(r"\\.\{}:", char::from(bytes[0]))));
        let raw = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "볼륨 열기 실패 (관리자 권한이 필요할 수 있음): {}",
                error_text(code)
            ));
        }
        let handle = VolumeHandle(raw);
        let mut returned = 0_u32;
        let mut journal = [0_u8; 128];
        let journal_available = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                null(),
                0,
                journal.as_mut_ptr().cast(),
                journal.len() as u32,
                &mut returned,
                null_mut(),
            ) != 0
        };
        let mut input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut entries = 0_u64;
        loop {
            let ok = unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_ENUM_USN_DATA,
                    (&input as *const MFT_ENUM_DATA_V0).cast(),
                    size_of::<MFT_ENUM_DATA_V0>() as u32,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(format!("MFT 열거 실패: {}", error_text(code)));
            }
            let length = returned as usize;
            if length < 8 {
                return Err("MFT 응답이 올바르지 않습니다.".into());
            }
            let next = u64::from_le_bytes(buffer[..8].try_into().unwrap());
            let mut offset = 8;
            while offset < length {
                if length - offset < 8 {
                    return Err("MFT 레코드가 잘렸습니다.".into());
                }
                let record_len =
                    u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
                if record_len < 8 || record_len > length - offset {
                    return Err("MFT 레코드 길이가 올바르지 않습니다.".into());
                }
                entries += 1;
                offset += record_len;
            }
            if next <= input.StartFileReferenceNumber {
                return Err("MFT 열거가 진행되지 않았습니다.".into());
            }
            input.StartFileReferenceNumber = next;
        }
        Ok(NtfsProbe {
            entries,
            elapsed_ms: started.elapsed().as_millis() as u64,
            journal_available,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicU64;

        static TEST_ID: AtomicU64 = AtomicU64::new(0);

        struct TempDirectory(PathBuf);
        impl TempDirectory {
            fn new() -> Self {
                let unique = TEST_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("quickfind-indexer-{}-{unique}", std::process::id()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn scoped_recovery_updates_one_root_and_keeps_other_roots_and_exclusions() {
            let temp = TempDirectory::new();
            let a = temp.0.join("a");
            let b = temp.0.join("b");
            let excluded = a.join("excluded");
            std::fs::create_dir_all(&excluded).unwrap();
            std::fs::create_dir(&b).unwrap();
            std::fs::write(a.join("before.txt"), b"old").unwrap();
            std::fs::write(b.join("untouched.txt"), b"old").unwrap();
            let previous = scan_with_exclusions(
                &[a.clone(), b.clone()],
                &[excluded.clone()],
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            std::fs::rename(a.join("before.txt"), a.join("after.txt")).unwrap();
            std::fs::write(a.join("new.txt"), b"new").unwrap();
            std::fs::write(excluded.join("hidden.txt"), b"hidden").unwrap();
            std::fs::write(
                b.join("untouched.txt"),
                b"changed on disk but outside scope",
            )
            .unwrap();
            let events =
                [
                    notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Name(
                        notify::event::RenameMode::Any,
                    )))
                    .add_path(a.clone()),
                ];
            let current =
                apply_changes_cancellable(&previous, &events, &AtomicBool::new(false), |_| {})
                    .unwrap();
            assert_eq!(current.roots, previous.roots);
            assert_eq!(current.excluded_folders, previous.excluded_folders);
            assert_eq!(
                current
                    .entries
                    .iter()
                    .find(|e| e.name == "untouched.txt")
                    .unwrap()
                    .size,
                Some(3)
            );
            assert!(current.entries.iter().any(|e| e.name == "after.txt"));
            assert!(current.entries.iter().any(|e| e.name == "new.txt"));
            assert!(
                !current
                    .entries
                    .iter()
                    .any(|e| e.name == "before.txt" || e.name == "hidden.txt" || e.name == "a")
            );
            assert!(
                apply_changes_cancellable(&previous, &events, &AtomicBool::new(true), |_| {})
                    .is_err()
            );
            // IDs reused by a root refresh must still support subsequent edits.
            std::fs::remove_file(a.join("new.txt")).unwrap();
            let removed = apply_changes(
                &current,
                &[
                    notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                        .add_path(a.join("new.txt")),
                ],
            )
            .unwrap();
            assert!(!removed.entries.iter().any(|e| e.name == "new.txt"));
        }

        #[test]
        fn recovery_preserves_only_unreadable_subtrees_and_current_metadata() {
            let folders = vec![
                r"C:\data".into(),
                r"C:\data\locked".into(),
                r"C:\data\locked\nested".into(),
                r"C:\data\locked2".into(),
                r"C:\data\locked\excluded".into(),
            ];
            let entry = |name: &str, parent, size| Entry {
                name: name.into(),
                parent,
                size: Some(size),
                created: Some(1),
                modified: Some(2),
                is_dir: false,
            };
            let previous = Index {
                folders: folders.clone().into(),
                entries: [
                    entry("old.txt", 1, 1),
                    entry("fresh.txt", 1, 1),
                    entry("nested.txt", 2, 1),
                    entry("deleted.txt", 0, 1),
                    entry("sibling-deleted.txt", 3, 1),
                    entry("hidden.txt", 4, 1),
                ]
                .into_iter()
                .collect(),
                ..Index::default()
            };
            let current = Index {
                folders: folders.into(),
                excluded_folders: vec![r"C:\data\locked\excluded".into()],
                entries: [entry("FRESH.txt", 1, 10), entry("new.txt", 1, 2)]
                    .into_iter()
                    .collect(),
                ..Index::default()
            };
            let recovered = preserve_unreadable(
                &previous,
                current,
                &std::collections::HashSet::from([r"c:\data\locked".into()]),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(recovered.len(), 4);
            assert_eq!(
                recovered
                    .entries
                    .iter()
                    .filter(|e| e.name.eq_ignore_ascii_case("fresh.txt"))
                    .count(),
                1
            );
            assert_eq!(
                recovered
                    .entries
                    .iter()
                    .find(|e| e.name == "FRESH.txt")
                    .unwrap()
                    .size,
                Some(10)
            );
            assert!(recovered.entries.iter().any(|e| e.name == "old.txt"));
            assert!(recovered.entries.iter().any(|e| e.name == "nested.txt"));
        }

        #[test]
        fn recovery_reconciles_creates_renames_deletes_and_exclusions() {
            let temp = TempDirectory::new();
            let excluded = temp.0.join("excluded");
            std::fs::create_dir(&excluded).unwrap();
            std::fs::write(temp.0.join("before.txt"), b"before").unwrap();
            std::fs::write(temp.0.join("delete.txt"), b"delete").unwrap();
            let previous = scan_with_exclusions(
                &[temp.0.clone()],
                &[excluded.clone()],
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            std::fs::rename(temp.0.join("before.txt"), temp.0.join("after.txt")).unwrap();
            std::fs::remove_file(temp.0.join("delete.txt")).unwrap();
            std::fs::write(excluded.join("hidden.txt"), b"hidden").unwrap();
            for n in 0..5000 {
                std::fs::write(temp.0.join(format!("new-{n:05}.txt")), b"new").unwrap();
            }
            let current = rescan(&previous, &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(current.len(), 5001);
            assert!(current.entries.iter().any(|e| e.name == "after.txt"));
            assert!(current.entries.iter().all(|e| {
                !["before.txt", "delete.txt", "hidden.txt", "excluded"].contains(&e.name)
            }));
            assert_eq!(previous.len(), 2);
            assert!(rescan(&previous, &AtomicBool::new(true), |_| {}).is_err());
        }

        #[test]
        fn exclusions_survive_watcher_subtree_refresh_and_moves() {
            use notify::{
                Event, EventKind,
                event::{CreateKind, ModifyKind, RenameMode},
            };
            let temp = TempDirectory::new();
            let parent = temp.0.join("자료");
            let excluded = parent.join("제외");
            let sibling = parent.join("제외2");
            std::fs::create_dir_all(excluded.join("하위")).unwrap();
            std::fs::create_dir(&sibling).unwrap();
            std::fs::write(excluded.join("하위\\hidden.txt"), b"hidden").unwrap();
            std::fs::write(sibling.join("visible.txt"), b"visible").unwrap();
            let roots = [temp.0.clone()];
            let mut index =
                scan_with_exclusions(&roots, &[excluded.clone()], &AtomicBool::new(false), |_| {})
                    .unwrap();
            let rules = FolderExclusions::new(&[excluded.clone()]);
            assert!(
                index
                    .entries
                    .iter()
                    .any(|entry| entry.name == "visible.txt")
            );
            assert!((0..index.len()).all(|row| !rules.matches(Path::new(&index.full_path(row)))));
            let added = excluded.join("new.txt");
            std::fs::write(&added, b"new").unwrap();
            let events = [Event::new(EventKind::Create(CreateKind::File)).add_path(added)];
            assert!(!has_relevant_changes(&index, &events));
            let old_count = index.len();
            index = apply_changes_owned(index, &events).unwrap();
            assert_eq!(index.len(), old_count);
            // Refreshing an included ancestor must not bring excluded children back.
            index = apply_changes_owned(
                index,
                &[Event::new(EventKind::Create(CreateKind::Folder)).add_path(parent.clone())],
            )
            .unwrap();
            assert_eq!(index.len(), old_count);
            assert_eq!(index.excluded_folders, rules.paths());
            assert!((0..index.len()).all(|row| !rules.matches(Path::new(&index.full_path(row)))));
            let old = sibling.join("visible.txt");
            let new = excluded.join("moved.txt");
            std::fs::rename(&old, &new).unwrap();
            index = apply_changes_owned(
                index,
                &[
                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                        .add_path(old)
                        .add_path(new),
                ],
            )
            .unwrap();
            assert!(
                !index
                    .entries
                    .iter()
                    .any(|entry| entry.name == "visible.txt" || entry.name == "moved.txt")
            );
            let restored = scan(&roots, &AtomicBool::new(false), |_| {}).unwrap();
            assert!(
                restored
                    .entries
                    .iter()
                    .any(|entry| entry.name == "hidden.txt")
            );
            assert!(restored.entries.iter().any(|entry| entry.name == "new.txt"));
            assert!(
                restored
                    .entries
                    .iter()
                    .any(|entry| entry.name == "moved.txt")
            );
            assert!(std::fs::read(excluded.join("하위\\hidden.txt")).is_ok());
        }

        #[test]
        fn scanner_preserves_korean_names_metadata_and_deduplicates_roots() {
            let temp = TempDirectory::new();
            let child = temp.0.join("한글 폴더");
            std::fs::create_dir(&child).unwrap();
            let filename = child.join("보고서_2026.pdf");
            std::fs::write(&filename, b"hello world").unwrap();
            let index = scan(
                &[child.clone(), temp.0.clone(), temp.0.clone()],
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            assert_eq!(index.roots.len(), 1);
            assert_eq!(index.entries.len(), 2);
            let row = index
                .entries
                .iter()
                .position(|e| e.name == "보고서_2026.pdf")
                .unwrap();
            let entry = index.entry(row);
            assert_eq!(entry.size, Some(11));
            assert!(entry.created.is_some());
            assert!(entry.modified.is_some());
            assert!(!entry.is_dir);
            assert_eq!(PathBuf::from(index.full_path(row)), filename);
            assert_eq!(index.entries.iter().find(|e| e.is_dir).unwrap().size, None);
        }

        #[test]
        fn cancelled_scan_does_not_return_an_incomplete_index() {
            let temp = TempDirectory::new();
            assert!(scan(&[temp.0.clone()], &AtomicBool::new(true), |_| {}).is_err());
        }

        #[test]
        fn protected_file_does_not_abort_a_batch_or_remove_its_previous_record() {
            use notify::{
                EventKind,
                event::{DataChange, ModifyKind},
            };
            let temp = TempDirectory::new();
            let protected = temp.0.join("protected.txt");
            let normal = temp.0.join("normal.txt");
            std::fs::write(&protected, b"old").unwrap();
            std::fs::write(&normal, b"old").unwrap();
            let index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            std::fs::write(&normal, b"new contents").unwrap();
            let kind = EventKind::Modify(ModifyKind::Data(DataChange::Content));
            let updated = apply_changes_with(
                index.clone(),
                &[event(kind, &[protected.clone(), normal.clone()])],
                |path| {
                    if path == protected {
                        classify_find_error(5)
                    } else {
                        find_one(path)
                    }
                },
            )
            .unwrap();
            assert_eq!(updated.entries.len(), 2);
            assert_eq!(
                updated
                    .entries
                    .iter()
                    .find(|e| e.name == "protected.txt")
                    .unwrap()
                    .size,
                Some(3)
            );
            assert_eq!(
                updated
                    .entries
                    .iter()
                    .find(|e| e.name == "normal.txt")
                    .unwrap()
                    .size,
                Some(12)
            );
            assert_eq!(updated.skipped, index.skipped + 1);
            assert!(matches!(
                classify_find_error(32).unwrap(),
                CurrentEntry::Unreadable
            ));
            assert!(matches!(
                classify_find_error(33).unwrap(),
                CurrentEntry::Unreadable
            ));
        }

        #[test]
        fn scanner_lists_junction_without_recursing_into_it() {
            let temp = TempDirectory::new();
            let child = temp.0.join("child");
            std::fs::create_dir(&child).unwrap();
            std::fs::write(child.join("only-once.txt"), b"x").unwrap();
            let link = child.join("cycle");
            // Directory junctions require neither Developer Mode nor elevation.
            let output = std::process::Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&link)
                .arg(&temp.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "junction creation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(index.entries.len(), 3);
            assert_eq!(
                index
                    .entries
                    .iter()
                    .filter(|e| e.name == "only-once.txt")
                    .count(),
                1
            );
            assert_eq!(index.skipped, 1);
            // Explicitly remove the link, then the ordinary temporary contents.
            std::fs::remove_dir(&link).unwrap();
        }

        #[test]
        fn overlap_check_respects_directory_boundary() {
            assert!(is_same_or_child(r"c:\foo\child", r"c:\foo"));
            assert!(!is_same_or_child(r"c:\foobar", r"c:\foo"));
            assert!(is_same_or_child(r"c:\foo", "c:"));
            assert_eq!(
                timestamp(FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0
                }),
                None
            );
        }

        fn event(kind: notify::EventKind, paths: &[PathBuf]) -> notify::Event {
            let mut event = notify::Event::new(kind);
            for path in paths {
                event = event.add_path(path.clone());
            }
            event
        }

        #[test]
        fn incremental_create_modify_rename_and_remove_are_consistent() {
            use notify::{
                EventKind,
                event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode},
            };
            let temp = TempDirectory::new();
            let mut index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            let original = temp.0.join("원본");
            std::fs::create_dir(&original).unwrap();
            let file = original.join("보고서.txt");
            std::fs::write(&file, b"one").unwrap();
            index = apply_changes(
                &index,
                &[
                    event(EventKind::Create(CreateKind::Folder), &[original.clone()]),
                    event(EventKind::Create(CreateKind::File), &[file.clone()]),
                ],
            )
            .unwrap();
            assert_eq!(index.entries.len(), 2);
            std::fs::write(&file, b"updated file").unwrap();
            index = apply_changes(
                &index,
                &[event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                    &[file.clone()],
                )],
            )
            .unwrap();
            assert_eq!(
                index
                    .entries
                    .iter()
                    .find(|e| e.name == "보고서.txt")
                    .unwrap()
                    .size,
                Some(12)
            );
            let moved = temp.0.join("이동됨");
            std::fs::rename(&original, &moved).unwrap();
            index = apply_changes(
                &index,
                &[event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                    &[original.clone(), moved.clone()],
                )],
            )
            .unwrap();
            assert_eq!(index.entries.len(), 2);
            let row = index
                .entries
                .iter()
                .position(|e| e.name == "보고서.txt")
                .unwrap();
            assert_eq!(
                PathBuf::from(index.full_path(row)),
                moved.join("보고서.txt")
            );
            std::fs::remove_dir_all(&moved).unwrap();
            index = apply_changes(
                &index,
                &[event(EventKind::Remove(RemoveKind::Folder), &[moved])],
            )
            .unwrap();
            assert!(index.entries.is_empty());
        }

        #[test]
        fn incremental_metadata_event_does_not_rewalk_directory_and_access_is_ignored() {
            use notify::{
                EventKind,
                event::{AccessKind, MetadataKind, ModifyKind},
            };
            let temp = TempDirectory::new();
            let child = temp.0.join("child");
            std::fs::create_dir(&child).unwrap();
            let index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            std::fs::write(child.join("not-announced.txt"), b"x").unwrap();
            let updated = apply_changes(
                &index,
                &[event(
                    EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
                    &[child.clone()],
                )],
            )
            .unwrap();
            assert_eq!(updated.entries.len(), 1);
            let updated = apply_changes(
                &updated,
                &[event(EventKind::Access(AccessKind::Read), &[child])],
            )
            .unwrap();
            assert_eq!(updated.entries.len(), 1);
            assert!(!has_relevant_changes(
                &updated,
                &[event(
                    EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
                    &[temp.0.clone()],
                )]
            ));
        }

        #[test]
        fn file_only_updates_preserve_folder_ids_and_directory_replacement_removes_children() {
            use notify::{
                EventKind,
                event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode},
            };
            let temp = TempDirectory::new();
            let child = temp.0.join("child");
            std::fs::create_dir(&child).unwrap();
            std::fs::write(child.join("nested.txt"), b"nested").unwrap();
            let mut index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            let folders = index.folders.len();
            let original = temp.0.join("new.txt");
            std::fs::write(&original, b"new").unwrap();
            index = apply_changes_owned(
                index,
                &[event(
                    EventKind::Create(CreateKind::File),
                    &[original.clone()],
                )],
            )
            .unwrap();
            assert_eq!(index.folders.len(), folders);
            assert_eq!(index.entries.len(), 3);
            let renamed = temp.0.join("renamed.txt");
            std::fs::rename(&original, &renamed).unwrap();
            index = apply_changes_owned(
                index,
                &[event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                    &[original, renamed.clone()],
                )],
            )
            .unwrap();
            assert_eq!(index.folders.len(), folders);
            assert_eq!(index.entries.len(), 3);
            assert!(index.entries.iter().any(|e| e.name == "renamed.txt"));
            std::fs::remove_file(&renamed).unwrap();
            index = apply_changes_owned(
                index,
                &[event(EventKind::Remove(RemoveKind::File), &[renamed])],
            )
            .unwrap();
            assert_eq!(index.entries.len(), 2);
            assert_eq!(index.folders.len(), folders);
            std::fs::remove_dir_all(&child).unwrap();
            std::fs::write(&child, b"replacement").unwrap();
            index = apply_changes_owned(
                index,
                &[event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                    &[child],
                )],
            )
            .unwrap();
            assert_eq!(index.entries.len(), 1);
            assert_eq!(index.entry(0).name, "child");
            assert!(!index.entry(0).is_dir);
        }

        #[test]
        fn unchanged_metadata_notifications_preserve_the_existing_engine() {
            use notify::{
                EventKind,
                event::{DataChange, MetadataKind, ModifyKind},
            };
            let temp = TempDirectory::new();
            let file = temp.0.join("unchanged.txt");
            std::fs::write(&file, b"same").unwrap();
            let index = scan(&[temp.0.clone()], &AtomicBool::new(false), |_| {}).unwrap();
            let modified = event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                &[file.clone()],
            );
            assert!(!metadata_changes_needed(&index, &[modified.clone()]));
            let metadata = event(
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
                &[file.clone()],
            );
            assert!(!metadata_changes_needed(&index, &[metadata]));
            std::fs::write(&file, b"different length").unwrap();
            assert!(metadata_changes_needed(&index, &[modified]));
            assert!(metadata_changes_needed(
                &index,
                &[notify::Event::new(EventKind::Any)]
            ));
        }
    }
}

#[cfg(windows)]
pub use win::{
    apply_changes, apply_changes_cancellable, apply_changes_owned, default_roots,
    has_relevant_changes, metadata_changes_needed, probe_ntfs, rescan, scan, scan_with_exclusions,
};

#[cfg(not(windows))]
pub fn rescan(_: &Index, _: &AtomicBool, _: impl Fn(ScanProgress)) -> Result<Index, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn default_roots() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn scan(_: &[PathBuf], _: &AtomicBool, _: impl Fn(ScanProgress)) -> Result<Index, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn scan_with_exclusions(
    _: &[PathBuf],
    _: &[PathBuf],
    _: &AtomicBool,
    _: impl Fn(ScanProgress),
) -> Result<Index, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn probe_ntfs(_: &Path) -> Result<NtfsProbe, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn apply_changes(_: &Index, _: &[notify::Event]) -> Result<Index, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn apply_changes_owned(_: Index, _: &[notify::Event]) -> Result<Index, String> {
    Err("Windows 전용 프로그램입니다.".into())
}

#[cfg(not(windows))]
pub fn has_relevant_changes(_: &Index, _: &[notify::Event]) -> bool {
    false
}

#[cfg(not(windows))]
pub fn metadata_changes_needed(_: &Index, _: &[notify::Event]) -> bool {
    true
}
