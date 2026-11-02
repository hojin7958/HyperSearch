//! Native owner-data Win32 controls: no GPU context, texture atlas or per-row widgets.
#![allow(unsafe_op_in_unsafe_fn)]
use crate::{
    cache,
    engine::{EntryKind, SearchEngine, SearchMode, SearchOptions, SortField},
    indexer,
    model::Index,
    platform::{self, Shell, ShellEvent},
    settings::Settings,
    updater,
};
use chrono::{DateTime, Local};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, MoveFileW},
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
        Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_PRIORITY_BELOW_NORMAL,
        },
    },
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

// Keep existing window/mutex identifiers compatible with earlier releases.
const CLASS: &str = "QuickFind.MainWindow";
const SETTINGS_CLASS: &str = "QuickFind.SettingsWindow";
const WORK: u32 = WM_APP + 30;
const SHELL_EVENT: u32 = WM_APP + 31;
const HEADER_CHANGED: u32 = WM_APP + 32;
const WATCH_WORK: u32 = WM_APP + 33;
const ACCESSIBILITY_CHANGED: u32 = WM_APP + 34;
const SAVE_TIMER: usize = 1;
const CHANGE_TIMER: usize = 2;
const CACHE_TIMER: usize = 3;
const STARTUP_SYNC_TIMER: usize = 4;
const ACCESSIBILITY_TIMER: usize = 5;
const RELEASE_TIMER: usize = 6;
const QUERY: usize = 100;
const REGEX: usize = 101;
const PATH: usize = 102;
const CASE: usize = 103;
const KIND: usize = 104;
const REFRESH: usize = 105;
const SETTINGS: usize = 106;
const FAST: usize = 107;
const RESULTS: usize = 108;
const SAVE_SETTINGS: usize = 200;
const HOTKEY_EDIT: usize = 201;
const ROOTS_EDIT: usize = 202;
const FONT_EDIT: usize = 203;
const AUTO_FAST: usize = 204;
const ADD_ROOT: usize = 205;
const REMOVE_ROOT: usize = 206;
const QUERY_FRAME: usize = 207;
const EXCLUDED_EDIT: usize = 220;
const ADD_EXCLUDED: usize = 221;
const REMOVE_EXCLUDED: usize = 222;
const CHECK_RELEASE: usize = 230;
const AUTO_RELEASE: usize = 224;
const RELEASE_STATUS: usize = 225;
const BG: u32 = 0x00201c19;
const INPUT_BG: u32 = 0x002b2622;
const FG: u32 = 0x00eee9e6;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn cache_path() -> PathBuf {
    platform::local_data_dir().join("index.bin")
}
fn normalized(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

enum AppEvent {
    Ready {
        generation: u64,
        engine: Arc<SearchEngine>,
        cached: bool,
    },
    Failed {
        generation: u64,
        error: String,
    },
    Search {
        sequence: u64,
        revision: u64,
        result: Result<(Vec<u32>, f64), String>,
        restore: Option<ViewPosition>,
    },
    Updated {
        generation: u64,
        revision: u64,
        result: Result<Arc<SearchEngine>, String>,
        changed: bool,
        failed_refresh: bool,
    },
    UpdateProgress {
        generation: u64,
        revision: u64,
        entries: usize,
    },
    Prepared(Box<PreparedUpdate>),
    IconReady {
        key: String,
        icon: i32,
        images: isize,
    },
    Deleted {
        paths: Vec<String>,
        result: Result<bool, String>,
    },
    Pasted(crate::file_clipboard::PasteResult),
    ReleaseChecked {
        manual: bool,
        result: Result<Option<updater::Release>, String>,
    },
    ReleaseDownloaded(Result<updater::StagedUpdate, String>),
    Error(String),
}
struct SearchRequest {
    sequence: u64,
    revision: u64,
    engine: Arc<SearchEngine>,
    query: String,
    options: SearchOptions,
    restore: Option<SavedView>,
}
struct PrepareRequest {
    generation: u64,
    search: SearchRequest,
}
struct PreparedUpdate {
    generation: u64,
    revision: u64,
    sequence: u64,
    engine: Arc<SearchEngine>,
    result: Result<(Vec<u32>, f64), String>,
    view: Option<SavedView>,
    restore: Option<ViewPosition>,
}
#[derive(Clone, PartialEq, Eq)]
struct RowTarget {
    name: String,
    parent: String,
}
#[derive(Clone)]
struct SavedView {
    top: usize,
    row_height: i32,
    selected: Option<RowTarget>,
    selection: Arc<[u128]>,
    selection_parents: Arc<[String]>,
    selection_mark: Option<RowTarget>,
    anchor: Option<RowTarget>,
    settings: Settings,
}
struct ViewPosition {
    top: usize,
    row_height: i32,
    selected: Option<usize>,
    selection: Vec<usize>,
    selection_mark: Option<usize>,
}
// Selection fingerprints avoid retaining a second full index or millions of
// path Strings during a refresh. They are UI identities, never file targets.
fn selection_identity(parent: &str, name: &str) -> u128 {
    let mut a = std::hash::DefaultHasher::new();
    (0u8, parent, name).hash(&mut a);
    let mut b = std::hash::DefaultHasher::new();
    (1u8, parent, name).hash(&mut b);
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}
const MAX_FROZEN_COPY_PATHS: usize = 256;
const MAX_FROZEN_COPY_BYTES: usize = 512 * 1024;
struct FrozenCopies {
    paths: HashMap<usize, CachedRow>,
    bytes: usize,
}
impl Default for FrozenCopies {
    fn default() -> Self {
        Self {
            paths: HashMap::new(),
            bytes: 128 * 1024,
        }
    }
}
impl FrozenCopies {
    fn insert(&mut self, item: usize, row: CachedRow) -> bool {
        if self.paths.contains_key(&item) {
            return true;
        }
        if self.paths.len() >= MAX_FROZEN_COPY_PATHS
            || self.bytes.saturating_add(row.byte_len()) > MAX_FROZEN_COPY_BYTES
        {
            return false;
        }
        self.bytes += row.byte_len();
        self.paths.insert(item, row);
        true
    }
}
#[derive(Default)]
struct CachedRow {
    text: [String; 5],
    icon: i32,
    path: String,
    is_dir: bool,
}
impl CachedRow {
    fn byte_len(&self) -> usize {
        self.path.capacity() + self.text.iter().map(String::capacity).sum::<usize>()
    }
    fn capture(index: &Index, row: usize) -> Option<Self> {
        if row >= index.len() {
            return None;
        }
        let entry = index.entry(row);
        Some(Self {
            text: [
                entry.name.to_string(),
                index.parent_path(row),
                if entry.is_dir {
                    "—".into()
                } else {
                    format_size(entry.size)
                },
                format_date(entry.created),
                format_date(entry.modified),
            ],
            icon: 0,
            path: index.full_path(row),
            is_dir: entry.is_dir,
        })
    }
}
impl SavedView {
    fn same_selection(&self, other: &Self) -> bool {
        self.matches(&other.settings)
            && self.selected == other.selected
            && self.selection == other.selection
            && self.selection_mark == other.selection_mark
    }
    fn matches(&self, settings: &Settings) -> bool {
        let old = &self.settings;
        old.query == settings.query
            && old.mode == settings.mode
            && old.match_path == settings.match_path
            && old.case_sensitive == settings.case_sensitive
            && old.kind == settings.kind
            && old.sort == settings.sort
            && old.descending == settings.descending
    }
    fn locate(&self, engine: &SearchEngine, rows: &[u32]) -> ViewPosition {
        let index = engine.index();
        let locate = |target: &Option<RowTarget>| {
            target.as_ref().and_then(|target| {
                let parent = index.folders.find_path(&target.parent)?;
                rows.iter().position(|&row| {
                    let entry = index.entry(row as usize);
                    entry.parent == parent && entry.name == target.name
                })
            })
        };
        ViewPosition {
            top: locate(&self.anchor)
                .unwrap_or(self.top)
                .min(rows.len().saturating_sub(1)),
            row_height: self.row_height,
            selected: locate(&self.selected),
            selection_mark: locate(&self.selection_mark),
            selection: if self.selection.is_empty() {
                Vec::new()
            } else {
                let parent_cache: HashMap<u32, &str> = self
                    .selection_parents
                    .iter()
                    .filter_map(|parent| {
                        index
                            .folders
                            .find_path(parent)
                            .map(|id| (id, parent.as_str()))
                    })
                    .collect();
                rows.iter()
                    .enumerate()
                    .filter_map(|(position, &row)| {
                        let entry = index.entry(row as usize);
                        let parent = parent_cache.get(&entry.parent)?;
                        self.selection
                            .binary_search(&selection_identity(parent, entry.name))
                            .is_ok()
                            .then_some(position)
                    })
                    .collect()
            },
        }
    }
}
enum CacheRequest {
    Save(Arc<Index>),
    Retire(Option<Arc<SearchEngine>>, Arc<Vec<u32>>),
    Flush(Sender<()>),
    Stop,
}

/// Coalesce saves only within the same side of a flush barrier. The elevated
/// indexer must never be followed by an older ordinary snapshot still queued
/// in this writer. Stop also finishes its preceding save before exiting.
fn run_cache_requests(receiver: Receiver<CacheRequest>, mut save: impl FnMut(&Index)) {
    'writer: while let Ok(request) = receiver.recv() {
        match request {
            CacheRequest::Save(mut index) => loop {
                match receiver.try_recv() {
                    Ok(CacheRequest::Save(newest)) => index = newest,
                    Ok(CacheRequest::Flush(reply)) => {
                        save(&index);
                        let _ = reply.send(());
                        continue 'writer;
                    }
                    Ok(CacheRequest::Stop) => {
                        save(&index);
                        break 'writer;
                    }
                    Ok(CacheRequest::Retire(engine, rows)) => {
                        drop(engine);
                        drop(rows);
                    }
                    Err(_) => {
                        save(&index);
                        continue 'writer;
                    }
                }
            },
            CacheRequest::Flush(reply) => {
                let _ = reply.send(());
            }
            CacheRequest::Stop => break,
            CacheRequest::Retire(engine, rows) => {
                drop(engine);
                drop(rows);
            }
        }
    }
}

fn flush_cache_before_fast(
    writer: &Sender<CacheRequest>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (reply, received) = mpsc::channel();
    writer
        .send(CacheRequest::Flush(reply))
        .map_err(|_| "인덱스 저장 작업이 종료되었습니다.".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("인덱싱을 취소했습니다.".into());
        }
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("인덱스 저장 작업이 종료되었습니다.".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout)
                if started.elapsed() >= Duration::from_secs(30) =>
            {
                return Err(
                    "이전 인덱스 저장이 완료되지 않았습니다. 잠시 후 다시 실행하세요.".into(),
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[derive(Clone)]
struct EventSender {
    tx: SyncSender<AppEvent>,
    hwnd: isize,
    wake_pending: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
impl EventSender {
    fn send(&self, event: AppEvent) {
        let mut event = event;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            match self.tx.try_send(event) {
                Ok(()) => {
                    self.wake();
                    return;
                }
                Err(mpsc::TrySendError::Full(pending)) => {
                    event = pending;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return,
            }
        }
    }
    fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            unsafe {
                PostMessageW(self.hwnd as HWND, WORK, 0, 0);
            }
        }
    }
}

const MAX_WATCH_PATHS: usize = 4096;
const MAX_WATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIRTY_FOLDERS: usize = 256;
const FULL_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
fn watch_key(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase())
}

fn background_priority() {
    unsafe {
        if SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) == 0 {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

/// Keep recovery sorting out of the foreground search pool, including its
/// parallel tasks. Windows background mode lowers CPU, I/O and memory priority.
fn background_work<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        rayon::ThreadPoolBuilder::new()
            .num_threads(available.saturating_sub(1).clamp(1, 2))
            .thread_name(|n| format!("recovery-{n}"))
            .start_handler(|_| background_priority())
            .build()
            .expect("background pool")
    })
    .install(work)
}

#[derive(Default)]
struct WatchBatch {
    paths: HashMap<PathBuf, bool>,
    // Fold only comparison keys. I/O must retain the original UTF-16 path:
    // Unicode lowercasing can change the actual filename (e.g. capital I-dot).
    originals: HashMap<PathBuf, PathBuf>,
    bytes: usize,
    overflow: bool,
    notified: bool,
    problem: Option<String>,
    started: Option<Instant>,
    retry_after: Option<Instant>,
    compacted: bool,
    reconnect: bool,
    full_scan_after: Option<Instant>,
    roots: Vec<PathBuf>,
}
impl WatchBatch {
    fn parent_scope(&self, path: &Path) -> PathBuf {
        let parent = path.parent().unwrap_or(path);
        let key = watch_key(path);
        if let Some(root) = self
            .roots
            .iter()
            .find(|root| key.starts_with(watch_key(root)))
        {
            if !watch_key(parent).starts_with(watch_key(root)) {
                return root.clone();
            }
        }
        parent.to_owned()
    }
    fn insert_scope(&mut self, path: PathBuf) {
        let key = watch_key(&path);
        if key.ancestors().any(|p| self.paths.get(p) == Some(&true)) {
            return;
        }
        self.paths.retain(|p, _| !p.starts_with(&key));
        self.originals.retain(|p, _| !p.starts_with(&key));
        self.paths.insert(key.clone(), true);
        self.originals.insert(key, path);
    }
    fn compact_paths(&mut self) {
        self.compacted = true;
        let paths = std::mem::take(&mut self.paths);
        let mut originals = std::mem::take(&mut self.originals);
        // A directory-create notification is often delivered as Create(Any).
        // Descendant notifications prove it is a directory: keep that scope
        // rather than accidentally widening a new build folder to the root.
        let mut directories = std::collections::HashSet::new();
        for path in paths.keys() {
            for ancestor in path.ancestors().skip(1) {
                if paths.contains_key(ancestor) {
                    directories.insert(ancestor.to_owned());
                }
            }
        }
        for (key, _) in paths {
            let path = originals.remove(&key).unwrap_or_else(|| key.clone());
            let scope = if directories.contains(&key) {
                path
            } else {
                self.parent_scope(&path)
            };
            self.insert_scope(scope);
        }
        self.bound_scopes();
    }
    fn bound_scopes(&mut self) {
        loop {
            self.bytes = self
                .paths
                .keys()
                .map(|p| p.as_os_str().as_encoded_bytes().len())
                .sum::<usize>()
                * 2
                + self
                    .originals
                    .values()
                    .map(|p| p.as_os_str().as_encoded_bytes().len())
                    .sum::<usize>();
            if self.paths.len() <= MAX_DIRTY_FOLDERS && self.bytes <= MAX_WATCH_BYTES {
                break;
            }
            // Widen only one deepest scope at a time; unrelated drives and
            // sibling trees remain separate. Never discard a changed path.
            let parent = self
                .paths
                .keys()
                .filter_map(|p| {
                    let original = self.originals.get(p).unwrap_or(p);
                    let parent = self.parent_scope(original);
                    (watch_key(&parent) != *p).then(|| (p.components().count(), parent))
                })
                .max_by_key(|(depth, _)| *depth)
                .map(|(_, p)| p);
            if let Some(parent) = parent {
                self.insert_scope(parent);
            } else {
                self.overflow = true;
                self.paths.clear();
                self.originals.clear();
                self.bytes = 0;
                break;
            }
        }
    }
    fn add(&mut self, event: Event) -> bool {
        if self.overflow {
            return false;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        self.started.get_or_insert_with(Instant::now);
        if event.need_rescan()
            || matches!(event.kind, EventKind::Any | EventKind::Other)
            || event.paths.is_empty()
        {
            self.overflow = true;
            self.reconnect = true;
            self.paths.clear();
            self.originals.clear();
            self.bytes = 0;
        } else {
            let recursive = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            );
            for path in event.paths {
                let key = watch_key(&path);
                if self.compacted {
                    // A timestamp change on an already covered directory must
                    // not promote that directory's scope to its parent.
                    if key.ancestors().any(|p| self.paths.get(p) == Some(&true)) {
                        continue;
                    }
                    self.insert_scope(self.parent_scope(&path));
                    self.bound_scopes();
                    continue;
                }
                if let Some(old) = self.paths.get_mut(&key) {
                    *old |= recursive;
                    continue;
                }
                let bytes = path.as_os_str().as_encoded_bytes().len()
                    + key.as_os_str().as_encoded_bytes().len() * 2;
                if self.paths.len() >= MAX_WATCH_PATHS
                    || self.bytes.saturating_add(bytes) > MAX_WATCH_BYTES
                {
                    self.compact_paths();
                    if !key.ancestors().any(|p| self.paths.get(p) == Some(&true)) {
                        self.insert_scope(self.parent_scope(&path));
                    }
                    self.bound_scopes();
                    continue;
                }
                self.bytes += bytes;
                self.paths.insert(key.clone(), recursive);
                self.originals.insert(key, path);
            }
        }
        if self.notified {
            false
        } else {
            self.notified = true;
            true
        }
    }
}
#[derive(Clone, Default)]
struct WatchInbox(Arc<Mutex<WatchBatch>>);
impl WatchInbox {
    fn fail(&self, error: String, hwnd: isize) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.paths.clear();
        state.originals.clear();
        state.bytes = 0;
        state.overflow = true;
        state.reconnect = true;
        state.started.get_or_insert_with(Instant::now);
        state.problem = Some(error.chars().take(512).collect());
        let wake = !state.notified;
        state.notified = true;
        drop(state);
        if wake {
            unsafe {
                PostMessageW(hwnd as HWND, WATCH_WORK, 0, 0);
            }
        }
    }
    fn enqueue(&self, event: Event, hwnd: isize) {
        let wake = self.0.lock().unwrap_or_else(|p| p.into_inner()).add(event);
        if wake {
            unsafe {
                PostMessageW(hwnd as HWND, WATCH_WORK, 0, 0);
            }
        }
    }
    fn is_empty(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.paths.is_empty() && !state.overflow
    }
    fn reset(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let roots = std::mem::take(&mut state.roots);
        *state = WatchBatch {
            roots,
            ..WatchBatch::default()
        };
    }
    fn delay_ms(&self, foreground: bool) -> u32 {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(deadline) = state.retry_after {
            return deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, u32::MAX as u128) as u32;
        }
        if state.overflow {
            return state.full_scan_after.map_or(1, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, u32::MAX as u128) as u32
            });
        }
        let target = if state.compacted {
            if foreground { 2000 } else { 5000 }
        } else if foreground {
            900u32
        } else {
            15_000u32
        };
        let elapsed = state.started.map_or(0, |started| {
            started.elapsed().as_millis().min(u32::MAX as u128) as u32
        });
        target.saturating_sub(elapsed).max(1)
    }
    fn take_work(&self) -> (Result<Vec<Event>, String>, bool, bool, bool) {
        // Consume an overflow too: new notifications must accumulate while
        // the recovery scan is running, and wake the next update afterwards.
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = std::mem::take(&mut *guard);
        guard.full_scan_after = state.full_scan_after;
        guard.roots = state.roots.clone();
        drop(guard);
        let full = state.overflow
            || state
                .roots
                .iter()
                .any(|root| state.paths.get(&watch_key(root)) == Some(&true));
        if state.overflow {
            return (
                Err(state
                    .problem
                    .clone()
                    .unwrap_or_else(|| "파일 변경 알림이 누락되었습니다.".into())),
                true,
                state.reconnect,
                true,
            );
        }
        (
            Ok(state
                .paths
                .into_iter()
                .map(|(path, recursive)| {
                    let path = state.originals.remove(&path).unwrap_or(path);
                    let kind = if recursive {
                        notify::event::ModifyKind::Name(notify::event::RenameMode::Any)
                    } else {
                        notify::event::ModifyKind::Data(notify::event::DataChange::Any)
                    };
                    Event::new(EventKind::Modify(kind)).add_path(path)
                })
                .collect()),
            state.compacted,
            state.reconnect,
            full,
        )
    }
    #[cfg(test)]
    fn take(&self) -> Result<Vec<Event>, String> {
        self.take_work().0
    }
    fn retry_rescan(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.paths.clear();
        state.originals.clear();
        state.bytes = 0;
        state.overflow = true;
        state.notified = true;
        state.retry_after = Some(Instant::now() + Duration::from_secs(30));
    }
    fn recovery_finished(&self) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .full_scan_after = Some(Instant::now() + FULL_RECOVERY_INTERVAL);
    }
    fn requires_full_scan(&self, roots: &[PathBuf]) -> bool {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.overflow
            || roots.iter().any(|root| {
                state.paths.get(&PathBuf::from(
                    root.to_string_lossy().replace('/', "\\").to_lowercase(),
                )) == Some(&true)
            })
    }
    fn full_scan_delay_ms(&self) -> u32 {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .full_scan_after
            .map_or(1, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, u32::MAX as u128) as u32
            })
    }
}

/// The watcher is created, reattached and destroyed on its owning worker.
/// Dropping this handle never joins notify's I/O thread on the UI thread.
struct WatcherHandle {
    stop: Sender<()>,
    ready: Arc<AtomicBool>,
}
impl Drop for WatcherHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}
fn wait_for_watcher(ready: Option<&AtomicBool>, cancel: &AtomicBool) -> Result<(), String> {
    let started = Instant::now();
    while ready.is_some_and(|ready| !ready.load(Ordering::Acquire)) {
        if cancel.load(Ordering::Relaxed) {
            return Err("인덱싱을 취소했습니다.".into());
        }
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("변경 감시 연결 시간이 초과되었습니다.".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The UI keeps the previous immutable snapshot available for searching.
/// Expensive copies, disk reads and recovery all happen on the update worker.
fn refresh_snapshot(
    engine: Arc<SearchEngine>,
    events: Result<Vec<Event>, String>,
    cancel: &AtomicBool,
    progress: impl Fn(indexer::ScanProgress),
) -> Result<(Arc<SearchEngine>, bool), String> {
    if let Ok(events) = events {
        if !indexer::has_relevant_changes(engine.index(), &events)
            || !indexer::metadata_changes_needed(engine.index(), &events)
        {
            return Ok((engine, false));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("인덱싱을 취소했습니다.".into());
        }
        if let Ok(index) =
            indexer::apply_changes_cancellable(engine.index(), &events, cancel, &progress)
        {
            return Ok((Arc::new(SearchEngine::new(Arc::new(index))), true));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("인덱싱을 취소했습니다.".into());
        }
        // A missing parent (e.g. a directory moved during delivery) needs only
        // its nearest indexed ancestor, not every configured drive.
        let mut scopes = WatchBatch::default();
        for path in events.iter().flat_map(|event| &event.paths) {
            if let Some(parent) = path.ancestors().skip(1).find(|parent| {
                engine
                    .index()
                    .folders
                    .find_path(&parent.to_string_lossy())
                    .is_some()
            }) {
                scopes.insert_scope(parent.to_owned());
            } else {
                return Err("변경 경로의 검색 범위를 확인할 수 없습니다.".into());
            }
        }
        let recovery: Vec<_> = scopes
            .originals
            .into_values()
            .map(|path| {
                Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::Any,
                )))
                .add_path(path)
            })
            .collect();
        let index =
            indexer::apply_changes_cancellable(engine.index(), &recovery, cancel, progress)?;
        return Ok((Arc::new(SearchEngine::new(Arc::new(index))), true));
    }
    // Overflow, lost parents and uncertain events are repaired automatically.
    // Keep the original snapshot if recovery fails; never load an older cache.
    let index = indexer::rescan(engine.index(), cancel, progress)?;
    Ok((Arc::new(SearchEngine::new(Arc::new(index))), true))
}

struct App {
    hwnd: HWND,
    query: HWND,
    query_frame: HWND,
    highlighter: crate::highlight::Highlighter,
    list: HWND,
    status: HWND,
    regex: HWND,
    path: HWND,
    case: HWND,
    kind: HWND,
    settings_window: HWND,
    hotkey_edit: HWND,
    roots_edit: HWND,
    excluded_edit: HWND,
    font_edit: HWND,
    auto_fast: HWND,
    auto_release: HWND,
    release_busy: bool,
    release_text: String,
    available_release: Option<updater::Release>,
    staged_release: Option<updater::StagedUpdate>,
    settings_error: HWND,
    hotkey_paused: bool,
    font: HFONT,
    background: HBRUSH,
    input_background: HBRUSH,
    settings: Settings,
    shell: Option<Shell>,
    events: EventSender,
    event_rx: Receiver<AppEvent>,
    search_tx: Sender<SearchRequest>,
    prepare_tx: Sender<PrepareRequest>,
    icon_tx: SyncSender<String>,
    cache_tx: Sender<CacheRequest>,
    cache_thread: Option<std::thread::JoinHandle<()>>,
    engine: Option<Arc<SearchEngine>>,
    staged_engine: Option<Arc<SearchEngine>>,
    staged_cache_dirty: bool,
    staged_message: String,
    startup_sync_pending: bool,
    prepared_update: Option<Box<PreparedUpdate>>,
    full_recovery: bool,
    rows: Arc<Vec<u32>>,
    accessibility: Option<crate::accessibility::Accessibility>,
    accessibility_epoch: u64,
    rows_revision: u64,
    list_frozen: bool,
    saved_view: Option<SavedView>,
    display_total: usize,
    status_text: RefCell<String>,
    context_menu_active: bool,
    drag_active: bool,
    file_operation_in_progress: bool,
    file_refresh_pending: bool,
    exit_after_file_operation: bool,
    discard_cache_on_exit: bool,
    refresh_after_operation: Option<bool>,
    frozen_copies: FrozenCopies,
    sequence: u64,
    revision: u64,
    generation: u64,
    cancel: Arc<AtomicBool>,
    indexing: bool,
    updating: bool,
    searching: bool,
    elapsed: f64,
    index_message: String,
    notice: String,
    watcher: Option<WatcherHandle>,
    pending: WatchInbox,
    cache_dirty: bool,
    cache_save_scheduled: bool,
    icons: HashMap<String, i32>,
    pending_icons: HashSet<String>,
    icon_list: isize,
    initializing: bool,
    rename_target: Option<String>,
}

unsafe fn dialog_key_message(window: HWND, msg: &MSG) -> bool {
    (WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message)
        && !window.is_null()
        && unsafe { IsWindowVisible(window) != 0 && IsWindowEnabled(window) != 0 }
        && (msg.hwnd == window || unsafe { IsChild(window, msg.hwnd) != 0 })
}

pub fn run(settings: Settings) -> Result<(), String> {
    unsafe {
        struct ComGuard(bool);
        impl Drop for ComGuard {
            fn drop(&mut self) {
                if self.0 {
                    unsafe {
                        CoUninitialize();
                    }
                }
            }
        }
        let _com = ComGuard(CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) >= 0);
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let common = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&common);
        let instance = GetModuleHandleW(null());
        let background = CreateSolidBrush(BG);
        let classes: [(&str, WNDPROC); 2] = [
            (CLASS, Some(main_proc)),
            (SETTINGS_CLASS, Some(settings_proc)),
        ];
        for (name, procedure) in classes {
            let class_name = wide(name);
            let class = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: procedure,
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hIcon: platform::app_icon(false),
                hbrBackground: background,
                lpszClassName: class_name.as_ptr(),
                ..zeroed()
            };
            RegisterClassW(&class);
        }
        let (tx, rx) = mpsc::sync_channel(4);
        let (search_tx, _) = mpsc::channel();
        let (prepare_tx, _) = mpsc::channel();
        let (icon_tx, _) = mpsc::sync_channel(64);
        let (cache_tx, _) = mpsc::channel();
        let pos = settings
            .window_pos
            .unwrap_or([CW_USEDEFAULT as f32, CW_USEDEFAULT as f32]);
        let mut app = Box::new(App {
            hwnd: null_mut(),
            query: null_mut(),
            query_frame: null_mut(),
            highlighter: crate::highlight::Highlighter::default(),
            list: null_mut(),
            status: null_mut(),
            regex: null_mut(),
            path: null_mut(),
            case: null_mut(),
            kind: null_mut(),
            settings_window: null_mut(),
            hotkey_edit: null_mut(),
            roots_edit: null_mut(),
            excluded_edit: null_mut(),
            font_edit: null_mut(),
            auto_fast: null_mut(),
            auto_release: null_mut(),
            release_busy: false,
            release_text: format!("현재 v{}", updater::VERSION),
            available_release: None,
            staged_release: None,
            settings_error: null_mut(),
            hotkey_paused: false,
            font: null_mut(),
            background,
            input_background: CreateSolidBrush(INPUT_BG),
            settings,
            shell: None,
            events: EventSender {
                tx,
                hwnd: 0,
                wake_pending: Arc::new(AtomicBool::new(false)),
                stopped: Arc::new(AtomicBool::new(false)),
            },
            event_rx: rx,
            search_tx,
            prepare_tx,
            icon_tx,
            cache_tx,
            cache_thread: None,
            engine: None,
            staged_engine: None,
            staged_cache_dirty: false,
            staged_message: String::new(),
            startup_sync_pending: false,
            prepared_update: None,
            full_recovery: false,
            rows: Arc::new(Vec::new()),
            accessibility: None,
            accessibility_epoch: 0,
            rows_revision: 0,
            list_frozen: false,
            saved_view: None,
            display_total: 0,
            status_text: RefCell::new(String::new()),
            context_menu_active: false,
            drag_active: false,
            file_operation_in_progress: false,
            file_refresh_pending: false,
            exit_after_file_operation: false,
            discard_cache_on_exit: false,
            refresh_after_operation: None,
            frozen_copies: FrozenCopies::default(),
            sequence: 0,
            revision: 0,
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            indexing: false,
            updating: false,
            searching: false,
            elapsed: 0.0,
            index_message: "인덱스 준비 중".into(),
            notice: String::new(),
            watcher: None,
            pending: WatchInbox::default(),
            cache_dirty: false,
            cache_save_scheduled: false,
            icons: HashMap::new(),
            pending_icons: HashSet::new(),
            icon_list: 0,
            initializing: true,
            rename_target: None,
        });
        let hwnd = CreateWindowExW(
            0,
            wide(CLASS).as_ptr(),
            wide("Hyperfinder - 고속파일 검색, *파일명*.xlsx 확장자검색가능, 단축키설정, 폴더더블클릭 폴더탐색기열기, 파일명클릭(1) 경로복사, 파일명 더블클릭(2) 파일실행").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            pos[0] as i32,
            pos[1] as i32,
            app.settings.window_size[0] as i32,
            app.settings.window_size[1] as i32,
            null_mut(),
            null_mut(),
            instance,
            app.as_mut() as *mut App as _,
        );
        if hwnd.is_null() {
            return Err(format!("창 생성 실패: {}", GetLastError()));
        }
        SendMessageW(
            hwnd,
            WM_SETICON,
            ICON_SMALL as usize,
            platform::app_icon(true) as isize,
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            ICON_BIG as usize,
            platform::app_icon(false) as isize,
        );
        app.events.hwnd = hwnd as isize;
        app.initialize();
        ShowWindow(
            hwnd,
            if app.settings.maximized {
                SW_SHOWMAXIMIZED
            } else {
                SW_SHOW
            },
        );
        UpdateWindow(hwnd);
        SetFocus(app.query);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if dialog_key_message(app.settings_window, &msg) {
                if GetFocus() == app.hotkey_edit {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    continue;
                }
                if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize {
                    DestroyWindow(app.settings_window);
                    continue;
                }
                if IsDialogMessageW(app.settings_window, &msg) != 0 {
                    continue;
                }
            } else if dialog_key_message(app.hwnd, &msg)
                && (app.keyboard(&msg)
                    || (app.rename_target.is_none() && IsDialogMessageW(app.hwnd, &msg) != 0))
            {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        app.events.stopped.store(true, Ordering::Release);
        app.cancel.store(true, Ordering::Relaxed);
        app.watcher = None;
        let _ = app.settings.save();
        if app.cache_dirty {
            if let Some(engine) = &app.engine {
                let _ = app
                    .cache_tx
                    .send(CacheRequest::Save(engine.index().clone()));
            }
        }
        let _ = app.cache_tx.send(CacheRequest::Stop);
        if let Some(worker) = app.cache_thread.take() {
            let _ = worker.join();
        }
        // A failed refresh after deleting must not resurrect stale results on
        // the next launch. Remove only our derived cache, after its writer stops.
        let cache_cleanup = if app.discard_cache_on_exit {
            std::fs::remove_file(cache_path()).or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        } else {
            Ok(())
        };
        app.shell = None;
        DeleteObject(app.font);
        DeleteObject(app.input_background);
        DeleteObject(app.background);
        cache_cleanup.map_err(|error| format!("이전 검색 캐시를 정리할 수 없습니다: {error}"))?;
    }
    Ok(())
}

unsafe extern "system" fn main_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if let Some(result) = crate::shell_menu::handle_message(message, w, l) {
        return result;
    }
    if message == WM_NCCREATE {
        let create = &*(l as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        (*(create.lpCreateParams as *mut App)).hwnd = hwnd;
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, message, w, l);
    }
    let app = &mut *ptr;
    match message {
        WM_CREATE => {
            app.create_controls();
            0
        }
        WM_SIZE => {
            app.layout();
            if w != SIZE_MINIMIZED as usize && !app.initializing {
                app.defer_save();
            }
            0
        }
        WM_MOVE | WM_EXITSIZEMOVE => {
            if !app.initializing {
                app.defer_save();
            }
            DefWindowProcW(hwnd, message, w, l)
        }
        WM_DPICHANGED => {
            let r = &*(l as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            app.install_font();
            app.layout();
            0
        }
        WM_GETMINMAXINFO => {
            let m = &mut *(l as *mut MINMAXINFO);
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            m.ptMinTrackSize = POINT {
                x: app.minimum_window_width(),
                y: (320.0 * scale) as i32,
            };
            0
        }
        WM_COMMAND => {
            app.command((w & 0xffff) as usize, ((w >> 16) & 0xffff) as u32);
            0
        }
        WM_NOTIFY => app.notify(l),
        WM_CLOSE => {
            app.hide();
            0
        }
        WM_DESTROY => {
            app.abort_list_refresh();
            let _ = app.settings.save();
            PostQuitMessage(0);
            0
        }
        WM_ERASEBKGND => {
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            FillRect(w as HDC, &r, app.background);
            1
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            SetTextColor(w as HDC, FG);
            SetBkColor(w as HDC, INPUT_BG);
            app.input_background as LRESULT
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            SetTextColor(w as HDC, FG);
            let input = l as HWND == app.query_frame;
            SetBkColor(w as HDC, if input { INPUT_BG } else { BG });
            if input {
                app.input_background as LRESULT
            } else {
                app.background as LRESULT
            }
        }
        WM_DRAWITEM => {
            app.draw_button(l);
            1
        }
        WM_MEASUREITEM if w == KIND => {
            let measure = &mut *(l as *mut MEASUREITEMSTRUCT);
            measure.itemHeight = (app.settings.font_size * GetDpiForWindow(hwnd).max(96) as f32
                / 72.0)
                .ceil() as u32
                + 8;
            1
        }
        WM_TIMER => {
            KillTimer(hwnd, w);
            match w {
                SAVE_TIMER => {
                    app.remember_geometry();
                    let _ = app.settings.save();
                }
                CHANGE_TIMER => {
                    app.apply_pending();
                    app.finish_file_refresh();
                }
                CACHE_TIMER => {
                    app.cache_save_scheduled = false;
                    app.save_cache();
                }
                ACCESSIBILITY_TIMER => {
                    if let Some(accessibility) = &app.accessibility {
                        accessibility.notify_changed();
                    }
                }
                RELEASE_TIMER if app.settings.check_updates_on_startup => app.check_release(false),
                STARTUP_SYNC_TIMER if app.settings.fast_index => {
                    if app.indexing
                        || app.updating
                        || app.file_operation_in_progress
                        || app.context_menu_active
                        || app.drag_active
                        || app.rename_target.is_some()
                    {
                        SetTimer(hwnd, STARTUP_SYNC_TIMER, 500, None);
                    } else {
                        app.start_scan_mode(false, false, true);
                    }
                }
                _ => {}
            }
            0
        }
        WORK => {
            app.handle_events();
            0
        }
        ACCESSIBILITY_CHANGED => {
            SetTimer(hwnd, ACCESSIBILITY_TIMER, 50, None);
            0
        }
        SHELL_EVENT => {
            app.handle_shell();
            0
        }
        HEADER_CHANGED => {
            app.defer_save();
            0
        }
        WATCH_WORK => {
            app.schedule_changes();
            app.update_status();
            0
        }
        WM_ACTIVATE => {
            app.schedule_changes();
            DefWindowProcW(hwnd, message, w, l)
        }
        message if message == WM_APP + 3 => {
            app.show();
            0
        }
        message if message == WM_APP + 4 => {
            app.request_exit();
            0
        }
        #[cfg(test)]
        message if message == WM_APP + 60 => {
            let engine = *Box::from_raw(l as *mut Arc<SearchEngine>);
            app.accept_ready(engine, false);
            0
        }
        #[cfg(test)]
        message if message == WM_APP + 64 => {
            let indexing = app.indexing;
            app.indexing = w != 0;
            *(l as *mut Result<Vec<crate::file_clipboard::DragFile>, String>) =
                app.prepare_drag_files();
            app.indexing = indexing;
            0
        }
        #[cfg(test)]
        message if message == WM_APP + 65 => {
            if w == 2 {
                return app.prepared_update.is_some() as isize;
            }
            app.drag_active = w != 0;
            app.file_operation_in_progress = w != 0;
            if w == 0 {
                app.publish_update();
            }
            0
        }
        _ => DefWindowProcW(hwnd, message, w, l),
    }
}

impl App {
    unsafe fn control(&self, class: &str, text: &str, style: u32, id: usize, parent: HWND) -> HWND {
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            parent,
            id as _,
            GetModuleHandleW(null()),
            null(),
        );
        if !self.font.is_null() {
            SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
        }
        hwnd
    }
    unsafe fn create_controls(&mut self) {
        self.install_font();
        self.query_frame = self.control("Static", "", WS_BORDER, QUERY_FRAME, self.hwnd);
        self.query = self.control(
            "Edit",
            &self.settings.query,
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            QUERY,
            self.hwnd,
        );
        SendMessageW(
            self.query,
            EM_SETCUEBANNER,
            0,
            wide("파일·폴더 검색  ·  *보고서*  /  *.pdf").as_ptr() as isize,
        );
        SetWindowSubclass(self.query, Some(query_subclass), QUERY, 0);
        self.regex = self.control(
            "Button",
            "정규식",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            REGEX,
            self.hwnd,
        );
        self.path = self.control(
            "Button",
            "전체 경로",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            PATH,
            self.hwnd,
        );
        self.case = self.control(
            "Button",
            "대소문자",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            CASE,
            self.hwnd,
        );
        for button in [self.regex, self.path, self.case] {
            SetWindowTheme(button, wide("").as_ptr(), wide("").as_ptr());
        }
        SendMessageW(
            self.regex,
            BM_SETCHECK,
            (self.settings.mode == SearchMode::Regex) as usize,
            0,
        );
        SendMessageW(self.path, BM_SETCHECK, self.settings.match_path as usize, 0);
        SendMessageW(
            self.case,
            BM_SETCHECK,
            self.settings.case_sensitive as usize,
            0,
        );
        self.kind = self.control(
            "ComboBox",
            "",
            WS_TABSTOP
                | CBS_DROPDOWNLIST as u32
                | CBS_OWNERDRAWFIXED as u32
                | CBS_HASSTRINGS as u32
                | WS_VSCROLL,
            KIND,
            self.hwnd,
        );
        SetWindowTheme(self.kind, wide("DarkMode_Explorer").as_ptr(), null());
        SetWindowSubclass(
            self.kind,
            Some(combo_subclass),
            1,
            self as *mut Self as usize,
        );
        for text in ["파일 + 폴더", "파일", "폴더"] {
            SendMessageW(self.kind, CB_ADDSTRING, 0, wide(text).as_ptr() as isize);
        }
        SendMessageW(
            self.kind,
            CB_SETCURSEL,
            match self.settings.kind {
                EntryKind::All => 0,
                EntryKind::Files => 1,
                EntryKind::Folders => 2,
            },
            0,
        );
        self.control(
            "Button",
            "새로고침",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            REFRESH,
            self.hwnd,
        );
        self.control(
            "Button",
            "전체 재색인",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            FAST,
            self.hwnd,
        );
        self.control(
            "Button",
            "설정",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            SETTINGS,
            self.hwnd,
        );
        self.status = self.control("Static", "인덱스 준비 중", 0x00004000, 110, self.hwnd);
        self.list = self.control(
            "SysListView32",
            "",
            WS_TABSTOP
                | WS_BORDER
                | LVS_REPORT
                | LVS_OWNERDATA
                | LVS_SHOWSELALWAYS
                | LVS_EDITLABELS
                | LVS_SHAREIMAGELISTS,
            RESULTS,
            self.hwnd,
        );
        SendMessageW(
            self.list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
        );
        SendMessageW(self.list, LVM_SETTEXTCOLOR, 0, FG as isize);
        SendMessageW(self.list, LVM_SETTEXTBKCOLOR, 0, BG as isize);
        SendMessageW(self.list, LVM_SETBKCOLOR, 0, BG as isize);
        SetWindowTheme(self.list, wide("DarkMode_Explorer").as_ptr(), null());
        let header = SendMessageW(self.list, LVM_GETHEADER, 0, 0) as HWND;
        SetWindowTheme(header, wide("DarkMode_ItemsView").as_ptr(), null());
        SetWindowSubclass(
            self.list,
            Some(list_subclass),
            1,
            self as *mut Self as usize,
        );
        self.accessibility = Some(crate::accessibility::Accessibility::new(self.list as usize));
        for (i, title) in ["이름", "폴더 경로", "크기", "생성일", "수정일"]
            .iter()
            .enumerate()
        {
            let mut text = wide(title);
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                fmt: if i == 2 { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                cx: self.settings.column_widths[i],
                pszText: text.as_mut_ptr(),
                ..zeroed()
            };
            SendMessageW(
                self.list,
                LVM_INSERTCOLUMNW,
                i,
                &column as *const _ as isize,
            );
        }
        let dark: i32 = 1;
        DwmSetWindowAttribute(
            self.hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const _ as _,
            size_of::<i32>() as u32,
        );
        self.icon_for("", true);
        self.sort_indicator();
        self.layout();
    }
    unsafe fn install_font(&mut self) {
        let dpi = GetDpiForWindow(self.hwnd).max(96);
        let height = -(self.settings.font_size * dpi as f32 / 72.0).round() as i32;
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("맑은 고딕").as_ptr(),
        );
        let old = self.font;
        self.font = font;
        for hwnd in [
            self.query,
            self.list,
            self.status,
            self.regex,
            self.path,
            self.case,
            self.kind,
        ] {
            if !hwnd.is_null() {
                SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
            }
        }
        for id in [REFRESH, FAST, SETTINGS] {
            let child = GetDlgItem(self.hwnd, id as i32);
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, font as usize, 1);
            }
        }
        if !old.is_null() {
            DeleteObject(old);
        }
    }
    unsafe fn toolbar_metrics(&self) -> ([i32; 7], i32) {
        let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
        let s = |x: i32| (x as f32 * scale).round() as i32;
        let dc = GetDC(self.hwnd);
        let old = SelectObject(dc, self.font);
        let mut metrics: TEXTMETRICW = zeroed();
        GetTextMetricsW(dc, &mut metrics);
        let widths = [
            ("정규식", 20),
            ("전체 경로", 20),
            ("대소문자", 20),
            ("파일 + 폴더", 30),
            ("새로고침", 14),
            ("전체 재색인", 14),
            ("설정", 14),
        ]
        .map(|(label, extra)| {
            let text = wide(label);
            let mut size: SIZE = zeroed();
            GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
            size.cx + s(extra)
        });
        SelectObject(dc, old);
        ReleaseDC(self.hwnd, dc);
        (widths, metrics.tmHeight)
    }
    unsafe fn minimum_window_width(&self) -> i32 {
        let dpi = GetDpiForWindow(self.hwnd).max(96);
        let s = |x: i32| (x as f32 * dpi as f32 / 96.0).round() as i32;
        if self.font.is_null() {
            return s(800);
        }
        let (widths, _) = self.toolbar_metrics();
        let mut rect = RECT {
            right: widths.iter().sum::<i32>() + s(6) * 2 + s(4) * 7 + s(160),
            ..zeroed()
        };
        AdjustWindowRectExForDpi(&mut rect, WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, 0, 0, dpi);
        (rect.right - rect.left).max(s(800))
    }
    unsafe fn layout(&self) {
        if self.list.is_null() {
            return;
        }
        let mut r: RECT = zeroed();
        GetClientRect(self.hwnd, &mut r);
        let dpi = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
        let s = |x: i32| (x as f32 * dpi).round() as i32;
        let pad = s(6);
        let gap = s(4);
        let width = r.right;
        let (widths, text_height) = self.toolbar_metrics();
        let minimum_client = widths.iter().sum::<i32>() + pad * 2 + gap * 7 + s(160);
        if width < minimum_client {
            // A larger saved font still needs one row with readable labels and
            // room to type. Grow only a window too narrow for its actual font.
            let mut outer: RECT = zeroed();
            GetWindowRect(self.hwnd, &mut outer);
            if SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                outer.right - outer.left + minimum_client - width,
                outer.bottom - outer.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) != 0
            {
                return;
            }
        }
        let query_height = (text_height + s(2)).max(s(14));
        let frame_height = s(28).max(query_height + s(8));
        let query_width = width - pad * 2 - gap * 7 - widths.iter().sum::<i32>();
        MoveWindow(self.query_frame, pad, pad, query_width, frame_height, 1);
        MoveWindow(
            self.query,
            pad + s(5),
            pad + (frame_height - query_height) / 2,
            (query_width - s(10)).max(1),
            query_height,
            1,
        );
        let mut x = pad + query_width + gap;
        for (i, (control, control_width)) in [
            self.regex,
            self.path,
            self.case,
            self.kind,
            GetDlgItem(self.hwnd, REFRESH as i32),
            GetDlgItem(self.hwnd, FAST as i32),
            GetDlgItem(self.hwnd, SETTINGS as i32),
        ]
        .into_iter()
        .zip(widths)
        .enumerate()
        {
            MoveWindow(
                control,
                x,
                pad,
                control_width,
                if i == 3 { s(240) } else { frame_height },
                1,
            );
            if i == 3 {
                let mut combo: RECT = zeroed();
                GetWindowRect(control, &mut combo);
                MoveWindow(
                    control,
                    x,
                    pad + (frame_height - (combo.bottom - combo.top)) / 2,
                    control_width,
                    s(240),
                    1,
                );
            }
            x += control_width + gap;
        }
        let list_top = pad + frame_height + s(6);
        MoveWindow(
            self.list,
            pad,
            list_top,
            width - pad * 2,
            (r.bottom - list_top - s(30)).max(20),
            1,
        );
        MoveWindow(
            self.status,
            pad,
            r.bottom - s(25),
            width - pad * 2,
            s(22),
            1,
        );
    }
    unsafe fn initialize(&mut self) {
        let (icon_tx, icon_rx) = mpsc::sync_channel::<String>(64);
        self.icon_tx = icon_tx;
        let icon_events = self.events.clone();
        std::thread::Builder::new()
            .name("shell-icons".into())
            .spawn(move || {
                let com = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
                while let Ok(key) = icon_rx.recv() {
                    let directory = key == "<folder>";
                    let lookup = if directory {
                        "folder"
                    } else if key.starts_with('.') {
                        &key
                    } else {
                        "file"
                    };
                    let mut info: SHFILEINFOW = unsafe { zeroed() };
                    let images = unsafe {
                        SHGetFileInfoW(
                            wide(lookup).as_ptr(),
                            if directory {
                                FILE_ATTRIBUTE_DIRECTORY
                            } else {
                                FILE_ATTRIBUTE_NORMAL
                            },
                            &mut info,
                            size_of::<SHFILEINFOW>() as u32,
                            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
                        )
                    };
                    icon_events.send(AppEvent::IconReady {
                        key,
                        icon: if images == 0 { 0 } else { info.iIcon },
                        images: images as isize,
                    });
                }
                if com >= 0 {
                    unsafe {
                        CoUninitialize();
                    }
                }
            })
            .expect("icon worker");
        for key in ["<folder>", "<file>"] {
            self.pending_icons.insert(key.into());
            let _ = self.icon_tx.try_send(key.into());
        }
        let (search_tx, search_rx) = mpsc::channel::<SearchRequest>();
        self.search_tx = search_tx;
        let tx = self.events.clone();
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let mut session = crate::engine::SearchSession::default();
                while let Ok(mut request) = search_rx.recv() {
                    while let Ok(newest) = search_rx.try_recv() {
                        request = newest;
                    }
                    let mut restore = None;
                    let result = session
                        .search(&request.engine, &request.query, &request.options)
                        .map(|r| {
                            restore = request
                                .restore
                                .as_ref()
                                .map(|view| view.locate(&request.engine, &r.rows));
                            (r.rows, r.elapsed_ms)
                        });
                    tx.send(AppEvent::Search {
                        sequence: request.sequence,
                        revision: request.revision,
                        result,
                        restore,
                    });
                }
            })
            .expect("search worker");
        let (prepare_tx, prepare_rx) = mpsc::channel::<PrepareRequest>();
        self.prepare_tx = prepare_tx;
        let tx = self.events.clone();
        std::thread::Builder::new()
            .name("prepare-update".into())
            .spawn(move || {
                background_priority();
                let mut session = crate::engine::SearchSession::default();
                while let Ok(mut request) = prepare_rx.recv() {
                    while let Ok(newest) = prepare_rx.try_recv() {
                        request = newest;
                    }
                    let search = request.search;
                    let mut restore = None;
                    let result = background_work(|| {
                        session
                            .search(&search.engine, &search.query, &search.options)
                            .map(|r| {
                                restore = search
                                    .restore
                                    .as_ref()
                                    .map(|view| view.locate(&search.engine, &r.rows));
                                (r.rows, r.elapsed_ms)
                            })
                    });
                    tx.send(AppEvent::Prepared(Box::new(PreparedUpdate {
                        generation: request.generation,
                        revision: search.revision,
                        sequence: search.sequence,
                        engine: search.engine,
                        result,
                        view: search.restore,
                        restore,
                    })));
                }
            })
            .expect("prepare worker");
        let (cache_tx, cache_rx) = mpsc::channel();
        self.cache_tx = cache_tx;
        let tx = self.events.clone();
        self.cache_thread = std::thread::Builder::new()
            .name("cache-writer".into())
            .spawn(move || {
                background_priority();
                run_cache_requests(cache_rx, |index| {
                    if let Err(error) = cache::save(&cache_path(), index) {
                        tx.send(AppEvent::Error(format!("인덱스 저장: {error}")));
                    }
                });
            })
            .ok();
        match Shell::new_native(self.hwnd as isize, SHELL_EVENT, &self.settings.hotkey) {
            Ok(shell) => {
                if let Some(error) = &shell.hotkey_error {
                    self.notice = error.clone();
                }
                self.shell = Some(shell);
            }
            Err(error) => self.notice = error,
        }
        self.initializing = false;
        if self.settings.check_updates_on_startup {
            SetTimer(self.hwnd, RELEASE_TIMER, 5000, None);
        }
        self.start_scan(true, false);
        self.update_status();
    }
    unsafe fn start_scan(&mut self, use_cache: bool, rebuild: bool) {
        // Explicit refresh supersedes an automatic startup synchronization.
        KillTimer(self.hwnd, STARTUP_SYNC_TIMER);
        self.startup_sync_pending = false;
        self.start_scan_mode(use_cache, rebuild, false);
    }
    unsafe fn start_scan_mode(&mut self, use_cache: bool, rebuild: bool, background: bool) {
        if self.updating || self.file_operation_in_progress {
            self.refresh_after_operation =
                Some(self.refresh_after_operation.unwrap_or(false) || rebuild);
            self.notice = "진행 중인 파일 작업을 마친 뒤 새로고침합니다.".into();
            self.update_status();
            return;
        }
        self.pending.reset();
        if self.watcher.is_none() {
            self.configure_watcher();
        }
        if self.cache_dirty {
            if let Some(engine) = &self.engine {
                if self
                    .cache_tx
                    .send(CacheRequest::Save(engine.index().clone()))
                    .is_ok()
                {
                    // Include the last ordinary update before the barrier.
                    // Closing during elevation must not queue it again after
                    // the helper has already begun writing its newer cache.
                    self.cache_dirty = false;
                }
            }
        }
        if self.indexing {
            self.cancel.store(true, Ordering::Relaxed);
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        self.indexing = true;
        self.updating = false;
        EnableWindow(GetDlgItem(self.hwnd, REFRESH as i32), 0);
        EnableWindow(GetDlgItem(self.hwnd, FAST as i32), 0);
        self.index_message = if rebuild {
            "전체 재색인 중 · 관리자 권한 확인"
        } else {
            "변경분 동기화 중 · 관리자 권한 확인"
        }
        .into();
        let cancel = self.cancel.clone();
        let roots = self.settings.roots.clone();
        let excluded_folders = self.settings.excluded_folders.clone();
        let generation = self.generation;
        let tx = self.events.clone();
        let cache_writer = self.cache_tx.clone();
        let watcher_ready = self.watcher.as_ref().map(|watcher| watcher.ready.clone());
        std::thread::Builder::new()
            .name("indexing".into())
            .spawn(move || {
                if background {
                    background_priority();
                }
                if let Err(error) = wait_for_watcher(watcher_ready.as_deref(), &cancel) {
                    tx.send(AppEvent::Failed { generation, error });
                    return;
                }
                if use_cache && flush_cache_before_fast(&cache_writer, &cancel).is_ok() {
                    if let Ok(index) =
                        cache::load_with_exclusions(&cache_path(), &roots, &excluded_folders)
                    {
                        let engine = Arc::new(SearchEngine::new(Arc::new(index)));
                        tx.send(AppEvent::Ready {
                            generation,
                            engine,
                            cached: true,
                        });
                        return;
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let result = flush_cache_before_fast(&cache_writer, &cancel)
                    .and_then(|_| {
                        platform::run_fast_index(
                            &roots,
                            &excluded_folders,
                            &cache_path(),
                            rebuild,
                            background,
                        )
                    })
                    .and_then(|_| {
                        cache::load_with_exclusions(&cache_path(), &roots, &excluded_folders)
                    });
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                match result {
                    Ok(index) => tx.send(AppEvent::Ready {
                        generation,
                        engine: Arc::new(SearchEngine::new(Arc::new(index))),
                        cached: false,
                    }),
                    Err(error) => tx.send(AppEvent::Failed { generation, error }),
                }
            })
            .expect("index worker");
        self.update_status();
    }
    unsafe fn configure_watcher(&mut self) {
        let previous = self.watcher.take();
        let pending = self.pending.clone();
        let hwnd = self.hwnd as isize;
        let ignored = platform::local_data_dir();
        let watch_roots = self.settings.roots.clone();
        self.pending
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .roots = watch_roots.clone();
        let roots: Vec<String> = self.settings.roots.iter().map(|p| normalized(p)).collect();
        let exclusions = crate::exclusions::FolderExclusions::new(&self.settings.excluded_folders);
        let (stop, stopped) = mpsc::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        self.watcher = Some(WatcherHandle { stop, ready });
        let retry = pending.clone();
        let messages = self.events.clone();
        std::thread::Builder::new().name("watcher-owner".into()).spawn(move || {
        // Notification delivery remains prompt; only setup/teardown runs here.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL); }
        let watch_exclusions = exclusions.clone();
        let watcher = RecommendedWatcher::new(
            move |event: notify::Result<Event>| match event {
                Ok(mut event) => {
                    if matches!(event.kind, EventKind::Access(_)) {
                        return;
                    }
                    event
                        .paths
                        .retain(|p| !p.starts_with(&ignored) && !exclusions.matches(p));
                    if !event.need_rescan()
                        && matches!(event.kind,EventKind::Modify(kind) if !matches!(kind,notify::event::ModifyKind::Name(_)))
                    {
                        event.paths.retain(|p| !roots.contains(&normalized(p)));
                    }
                    if event.need_rescan()
                        || matches!(event.kind, EventKind::Any | EventKind::Other)
                        || !event.paths.is_empty()
                    {
                        pending.enqueue(event, hwnd);
                    }
                }
                Err(e) => pending.fail(format!("변경 감지: {e}"), hwnd),
            },
            notify::Config::default(),
        );
        let mut problem = None;
        let watcher = match watcher {
            Ok(mut watcher) => {
                for root in &watch_roots {
                    if watch_exclusions.matches(root) {
                        continue;
                    }
                    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                        problem = Some(format!("변경 감지: {} · {e}", root.display()));
                    }
                }
                Some(watcher)
            }
            Err(e) => { problem = Some(format!("변경 감지: {e}")); None },
        };
        if let Some(error) = problem {
            retry.fail(error.clone(), hwnd);
            retry.retry_rescan();
            messages.send(AppEvent::Error(format!("{error} · 30초 후 자동 재시도")));
        }
        // Overlap the connections until the replacement is ready. Its scan
        // waits for this flag; setup and notify::Drop never block the UI.
        worker_ready.store(true, Ordering::Release);
        drop(previous);
        let _ = stopped.recv();
        drop(watcher);
        }).expect("watcher owner");
    }
    unsafe fn submit_search(&mut self) {
        self.settings.query = control_text(self.query);
        self.settings.match_path = SendMessageW(self.path, BM_GETCHECK, 0, 0) != 0;
        self.settings.case_sensitive = SendMessageW(self.case, BM_GETCHECK, 0, 0) != 0;
        self.settings.mode = if SendMessageW(self.regex, BM_GETCHECK, 0, 0) != 0 {
            SearchMode::Regex
        } else if self.settings.query.contains(['*', '?']) {
            SearchMode::Wildcard
        } else {
            SearchMode::Plain
        };
        self.settings.kind = match SendMessageW(self.kind, CB_GETCURSEL, 0, 0) {
            1 => EntryKind::Files,
            2 => EntryKind::Folders,
            _ => EntryKind::All,
        };
        self.highlighter = crate::highlight::Highlighter::new(
            &self.settings.query,
            self.settings.mode,
            self.settings.case_sensitive,
        );
        self.defer_save();
        self.sequence += 1;
        let Some(engine) = self.engine.clone() else {
            return;
        };
        self.searching = true;
        self.notice.clear();
        let options = SearchOptions {
            mode: self.settings.mode,
            match_path: self.settings.match_path,
            case_sensitive: self.settings.case_sensitive,
            kind: self.settings.kind,
            sort: self.settings.sort,
            descending: self.settings.descending,
        };
        let _ = self.search_tx.send(SearchRequest {
            sequence: self.sequence,
            revision: self.revision,
            engine,
            query: self.settings.query.clone(),
            options,
            restore: self
                .saved_view
                .as_ref()
                .filter(|view| view.matches(&self.settings))
                .cloned(),
        });
        if self.staged_engine.is_some() {
            self.prepare_update();
        }
    }
    fn search_options(&self) -> SearchOptions {
        SearchOptions {
            mode: self.settings.mode,
            match_path: self.settings.match_path,
            case_sensitive: self.settings.case_sensitive,
            kind: self.settings.kind,
            sort: self.settings.sort,
            descending: self.settings.descending,
        }
    }
    unsafe fn capture_view(&self) -> Option<SavedView> {
        let engine = self.engine.as_ref()?;
        if !self.rows_current() {
            return None;
        }
        let index = engine.index();
        let target = |position: usize| {
            self.rows.get(position).map(|&row| RowTarget {
                name: index.entry(row as usize).name.to_owned(),
                parent: index.parent_path(row as usize),
            })
        };
        let top = SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
        let mut selection = Vec::new();
        let mut parents = HashMap::new();
        for item in self.selected_items() {
            if let Some(&row) = self.rows.get(item) {
                let entry = index.entry(row as usize);
                let parent = parents
                    .entry(entry.parent)
                    .or_insert_with(|| index.parent_path(row as usize));
                selection.push(selection_identity(parent, entry.name));
            }
        }
        selection.sort_unstable();
        let mut rect: RECT = zeroed();
        rect.left = LVIR_BOUNDS as i32;
        let row_height = if SendMessageW(
            self.list,
            LVM_GETITEMRECT,
            top,
            &mut rect as *mut _ as isize,
        ) != 0
        {
            (rect.bottom - rect.top).max(1)
        } else {
            22
        };
        Some(SavedView {
            top,
            row_height,
            selected: self.selected().and_then(target),
            selection: selection.into(),
            selection_parents: parents.into_values().collect::<Vec<_>>().into(),
            selection_mark: usize::try_from(SendMessageW(self.list, LVM_GETSELECTIONMARK, 0, 0))
                .ok()
                .and_then(target),
            anchor: if top > 0 { target(top) } else { None },
            settings: self.settings.clone(),
        })
    }
    unsafe fn prepare_update(&mut self) {
        let Some(engine) = self.staged_engine.clone() else {
            return;
        };
        self.prepared_update = None;
        let _ = self.prepare_tx.send(PrepareRequest {
            generation: self.generation,
            search: SearchRequest {
                sequence: self.sequence,
                revision: self.revision,
                engine,
                query: self.settings.query.clone(),
                options: self.search_options(),
                restore: self.capture_view(),
            },
        });
    }
    unsafe fn publish_update(&mut self) {
        if self.rename_target.is_some()
            || self.context_menu_active
            || self.drag_active
            || self.file_operation_in_progress
            || self.list_frozen
        {
            if self.prepared_update.is_some() {
                SetTimer(self.hwnd, CHANGE_TIMER, 100, None);
            }
            return;
        }
        let Some(mut prepared) = self.prepared_update.take() else {
            return;
        };
        if prepared.generation != self.generation
            || prepared.revision != self.revision
            || prepared.sequence != self.sequence
        {
            return;
        }
        let current_view = self.capture_view();
        if !match (&prepared.view, &current_view) {
            (Some(old), Some(current)) => old.same_selection(current),
            (None, None) => true,
            _ => false,
        } {
            self.prepare_update();
            return;
        }
        // Continuous scrolling must not keep restarting preparation forever.
        // Apply the user's latest scroll delta to the already relocated anchor.
        if let (Some(position), Some(old), Some(current)) =
            (&mut prepared.restore, &prepared.view, &current_view)
        {
            let max_row = prepared
                .result
                .as_ref()
                .map_or(0, |(rows, _)| rows.len().saturating_sub(1));
            position.top = (position.top as i64 + current.top as i64 - old.top as i64)
                .clamp(0, max_row as i64) as usize;
            position.row_height = current.row_height;
        }
        // Old searches, scrolling and file actions stayed live throughout the
        // scan AND the replacement search. Publish matching IDs and rows together.
        let (rows, elapsed) = match prepared.result {
            Ok(result) => result,
            Err(error) => {
                self.notice = error;
                (Vec::new(), 0.0)
            }
        };
        SendMessageW(self.list, WM_SETREDRAW, 0, 0);
        let old_engine = self.engine.replace(prepared.engine);
        let old_rows = std::mem::replace(&mut self.rows, Arc::new(rows));
        self.accessibility_epoch += 1;
        self.staged_engine = None;
        self.revision += 1;
        self.rows_revision = self.revision;
        self.sequence += 1;
        self.searching = false;
        self.updating = false;
        self.elapsed = elapsed;
        self.display_total = self.engine.as_ref().map_or(0, |e| e.index().len());
        self.index_message = std::mem::take(&mut self.staged_message);
        self.cache_dirty |= self.staged_cache_dirty;
        self.staged_cache_dirty = false;
        SendMessageW(
            self.list,
            LVM_SETITEMCOUNT,
            self.rows.len(),
            (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize,
        );
        self.finish_list_refresh(prepared.restore);
        let _ = self
            .cache_tx
            .send(CacheRequest::Retire(old_engine, old_rows));
        if self.cache_dirty {
            self.schedule_cache();
        }
        self.schedule_startup_sync();
        if !self.pending.is_empty() {
            self.schedule_changes();
        }
        self.update_status();
    }
    unsafe fn submit_automatic_search(&mut self) {
        let notice = std::mem::take(&mut self.notice);
        self.submit_search();
        self.notice = notice;
    }
    unsafe fn schedule_startup_sync(&mut self) {
        if self.startup_sync_pending {
            self.startup_sync_pending = false;
            if self.settings.fast_index {
                SetTimer(self.hwnd, STARTUP_SYNC_TIMER, 1000, None);
            }
        }
    }
    unsafe fn accept_ready(&mut self, engine: Arc<SearchEngine>, cached: bool) {
        self.indexing = false;
        EnableWindow(GetDlgItem(self.hwnd, REFRESH as i32), 1);
        EnableWindow(GetDlgItem(self.hwnd, FAST as i32), 1);
        let message = if cached {
            if cache_path().with_extension("ntfs.bin").is_file() {
                "저장 목록 · 고속 인덱싱으로 변경 동기화"
            } else {
                "저장 목록 · 새로고침으로 변경 동기화"
            }
        } else {
            "인덱스 최신"
        }
        .to_owned();
        self.startup_sync_pending = cached && self.settings.fast_index;
        if self.engine.is_some() {
            // This includes the second Ready after automatic startup replay.
            // Keep the displayed snapshot live until its replacement is sorted.
            self.updating = true;
            self.staged_engine = Some(engine);
            self.staged_cache_dirty = false; // The helper already saved it.
            self.staged_message = message;
            self.index_message = "동기화 결과 준비 중 · 기존 목록 검색 가능".into();
            self.prepare_update();
        } else {
            self.index_message = message;
            self.freeze_list();
            self.engine = Some(engine);
            self.revision += 1;
            self.submit_automatic_search();
        }
        if !self.pending.is_empty() {
            self.schedule_changes();
        }
    }
    unsafe fn handle_events(&mut self) {
        self.events.wake_pending.store(false, Ordering::Release);
        let started = Instant::now();
        let mut handled = 0;
        while handled < 32 && started.elapsed() < Duration::from_millis(4) {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            handled += 1;
            match event {
                AppEvent::Ready {
                    generation,
                    engine,
                    cached,
                } if generation == self.generation => {
                    self.accept_ready(engine, cached);
                }
                AppEvent::Failed { generation, error } if generation == self.generation => {
                    self.indexing = false;
                    EnableWindow(GetDlgItem(self.hwnd, REFRESH as i32), 1);
                    EnableWindow(GetDlgItem(self.hwnd, FAST as i32), 1);
                    self.notice = format!("인덱싱 실패: {error}");
                    self.abort_list_refresh();
                    if !self.pending.is_empty() {
                        self.schedule_changes();
                    }
                }
                AppEvent::Search {
                    sequence,
                    revision,
                    result,
                    restore,
                } if sequence == self.sequence && revision == self.revision => {
                    self.searching = false;
                    SendMessageW(self.list, WM_SETREDRAW, 0, 0);
                    match result {
                        Ok((rows, elapsed)) => {
                            self.rows = Arc::new(rows);
                            self.elapsed = elapsed;
                        }
                        Err(error) => {
                            self.rows = Arc::new(Vec::new());
                            self.notice = error;
                        }
                    }
                    self.accessibility_epoch += 1;
                    self.rows_revision = self.revision;
                    self.display_total = self
                        .engine
                        .as_ref()
                        .map_or(self.display_total, |engine| engine.index().len());
                    SendMessageW(
                        self.list,
                        LVM_SETITEMCOUNT,
                        self.rows.len(),
                        (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize,
                    );
                    self.finish_list_refresh(restore);
                    self.schedule_startup_sync();
                }
                AppEvent::Updated {
                    generation,
                    revision,
                    result,
                    changed,
                    failed_refresh,
                } if generation == self.generation && revision == self.revision => {
                    if self.full_recovery {
                        self.pending.recovery_finished();
                        self.full_recovery = false;
                    }
                    if failed_refresh && self.file_refresh_pending {
                        self.discard_cache_on_exit = true;
                    }
                    match result {
                        Ok(engine) => {
                            if changed {
                                self.staged_engine = Some(engine);
                                self.staged_cache_dirty = true;
                                self.staged_message = "인덱스 최신".into();
                                self.index_message =
                                    "새 검색 결과 준비 중 · 기존 목록 검색 가능".into();
                                self.prepare_update();
                            } else {
                                self.updating = false;
                                self.index_message = "인덱스 최신".into();
                            }
                        }
                        Err(error) => {
                            self.updating = false;
                            self.notice = format!("변경 반영: {error} · 30초 후 자동 재시도");
                            self.abort_list_refresh();
                            self.pending.retry_rescan();
                            if self.file_refresh_pending {
                                self.discard_cache_on_exit = true;
                                self.file_refresh_pending = false;
                            }
                        }
                    }
                    if !self.pending.is_empty() {
                        self.schedule_changes();
                    }
                }
                AppEvent::Prepared(prepared)
                    if prepared.generation == self.generation
                        && prepared.revision == self.revision
                        && prepared.sequence == self.sequence
                        && self
                            .staged_engine
                            .as_ref()
                            .is_some_and(|engine| Arc::ptr_eq(engine, &prepared.engine)) =>
                {
                    self.prepared_update = Some(prepared);
                    self.publish_update();
                }
                AppEvent::UpdateProgress {
                    generation,
                    revision,
                    entries,
                } if generation == self.generation
                    && revision == self.revision
                    && self.updating =>
                {
                    self.index_message =
                        format!("대량 변경 자동 복구 중 · {entries}개 확인 · 기존 목록 검색 가능");
                }
                AppEvent::IconReady { key, icon, images } => {
                    self.pending_icons.remove(&key);
                    if self.icons.len() < 2048 {
                        self.icons.insert(key, icon);
                    }
                    if images != 0 && self.icon_list == 0 {
                        self.icon_list = images;
                        SendMessageW(self.list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images);
                    }
                    RedrawWindow(self.list, null(), null_mut(), RDW_INVALIDATE);
                }
                AppEvent::Deleted { paths, result } => {
                    self.file_operation_in_progress = false;
                    self.file_refresh_pending = true;
                    self.notice = match result {
                        Ok(true) => format!("삭제 완료 · {}개 항목", paths.len()),
                        Ok(false) => "삭제를 취소했습니다.".into(),
                        Err(error) => format!("삭제 실패: {error}"),
                    };
                    // Reconcile even a cancelled/failed folder operation: some
                    // children may already have been removed before it stopped.
                    for path in paths {
                        self.pending.enqueue(
                            Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
                                .add_path(PathBuf::from(path)),
                            self.hwnd as isize,
                        );
                    }
                    self.schedule_changes();
                }
                AppEvent::Pasted(outcome) => {
                    self.file_operation_in_progress = false;
                    self.notice = match outcome.completed {
                        Ok(true) => "파일 붙여넣기 완료".into(),
                        Ok(false) => "붙여넣기를 취소했습니다.".into(),
                        Err(error) => error,
                    };
                    if !outcome.attempted {
                        // An empty/changed clipboard or rejected destination
                        // changed no files, so there is no folder to rescan.
                        continue;
                    }
                    self.file_refresh_pending = true;
                    // The operation may have stopped after moving some items.
                    // Reconcile both ends, including the case where watchers
                    // were stopped by an exit request during the operation.
                    for source in &outcome.sources {
                        self.pending.enqueue(
                            Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
                                .add_path(source.clone()),
                            self.hwnd as isize,
                        );
                        if let Some(name) = source.file_name() {
                            self.pending.enqueue(
                                Event::new(EventKind::Create(notify::event::CreateKind::Any))
                                    .add_path(outcome.destination.join(name)),
                                self.hwnd as isize,
                            );
                        }
                    }
                    if !self
                        .settings
                        .roots
                        .iter()
                        .any(|root| normalized(root) == normalized(&outcome.destination))
                    {
                        self.pending.enqueue(
                            Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
                                notify::event::RenameMode::Any,
                            )))
                            .add_path(outcome.destination),
                            self.hwnd as isize,
                        );
                    }
                    self.schedule_changes();
                }
                AppEvent::ReleaseChecked { manual, result } => {
                    self.release_busy = false;
                    match result {
                        Ok(Some(release)) => {
                            self.release_text = format!("새 버전 v{} 사용 가능", release.version);
                            self.notice = format!(
                                "새 버전 v{}가 있습니다. 설정에서 업데이트하세요.",
                                release.version
                            );
                            self.available_release = Some(release);
                            self.refresh_release_controls();
                            if manual {
                                self.download_release();
                            }
                        }
                        Ok(None) => {
                            self.release_text =
                                format!("현재 v{} · 최신 버전입니다", updater::VERSION)
                        }
                        Err(error) => {
                            self.release_text = format!("현재 v{} · 확인 실패", updater::VERSION);
                            if manual {
                                self.release_error(&error);
                            }
                        }
                    }
                    self.refresh_release_controls();
                }
                AppEvent::ReleaseDownloaded(result) => {
                    self.release_busy = false;
                    match result {
                        Ok(staged) => {
                            self.staged_release = Some(staged);
                            self.release_text = "다운로드 완료 · 설치 준비됨".into();
                            self.install_release();
                        }
                        Err(error) => {
                            self.release_text = "다운로드 실패 · 다시 시도하세요".into();
                            self.release_error(&error);
                        }
                    }
                    self.refresh_release_controls();
                }
                AppEvent::Error(error) => self.notice = error,
                _ => {}
            }
        }
        if handled == 32 || started.elapsed() >= Duration::from_millis(4) {
            self.events.wake();
        }
        if handled > 0 {
            self.update_status();
        }
        if !self.updating
            && !self.indexing
            && !self.file_operation_in_progress
            && !self.exit_after_file_operation
        {
            if let Some(rebuild) = self.refresh_after_operation.take() {
                self.start_scan(false, rebuild);
            }
        }
        self.finish_file_refresh();
    }
    unsafe fn finish_file_refresh(&mut self) {
        if self.file_operation_in_progress || self.indexing || self.updating || self.list_frozen {
            return;
        }
        if self.file_refresh_pending && self.pending.is_empty() {
            self.file_refresh_pending = false;
        }
        if self.exit_after_file_operation {
            if self.file_refresh_pending {
                self.apply_pending();
                if !self.updating && self.pending.is_empty() {
                    self.file_refresh_pending = false;
                }
            }
            if !self.file_refresh_pending {
                self.request_exit();
            }
        }
    }
    fn rows_current(&self) -> bool {
        !self.list_frozen && self.rows_revision == self.revision && self.engine.is_some()
    }
    unsafe fn freeze_list(&mut self) {
        if self.list_frozen || self.rows.is_empty() {
            return;
        }
        let Some(engine) = &self.engine else {
            return;
        };
        if self.rows_revision != self.revision {
            return;
        }
        let target = |position: usize| {
            self.rows.get(position).and_then(|&row| {
                if row as usize >= engine.index().len() {
                    return None;
                }
                let entry = engine.index().entry(row as usize);
                Some(RowTarget {
                    name: entry.name.to_string(),
                    parent: engine.index().parent_path(row as usize),
                })
            })
        };
        let top = SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
        let selected = self.selected();
        let mut selection = Vec::new();
        let mut parents = HashMap::new();
        for item in self.selected_items() {
            if let Some(&row) = self.rows.get(item) {
                let entry = engine.index().entry(row as usize);
                let parent = parents
                    .entry(entry.parent)
                    .or_insert_with(|| engine.index().parent_path(row as usize));
                selection.push(selection_identity(parent, entry.name));
            }
        }
        selection.sort_unstable();
        let mut copies = FrozenCopies::default();
        let visible = SendMessageW(self.list, LVM_GETCOUNTPERPAGE, 0, 0).max(1) as usize + 2;
        let end = top
            .saturating_add(visible.min(MAX_FROZEN_COPY_PATHS - 1))
            .min(self.rows.len());
        for item in selected.into_iter().chain(top..end) {
            let row = self.rows[item] as usize;
            if row < engine.index().len() {
                if let Some(row) = CachedRow::capture(engine.index(), row) {
                    if !copies.insert(item, row) {
                        break;
                    }
                }
            }
        }
        let mut rect: RECT = zeroed();
        rect.left = LVIR_BOUNDS as i32;
        let row_height = if SendMessageW(
            self.list,
            LVM_GETITEMRECT,
            top,
            &mut rect as *mut _ as isize,
        ) != 0
        {
            (rect.bottom - rect.top).max(1)
        } else {
            22
        };
        self.saved_view = Some(SavedView {
            top,
            row_height,
            selected: selected.and_then(target),
            selection: selection.into(),
            selection_parents: parents.into_values().collect::<Vec<_>>().into(),
            selection_mark: usize::try_from(SendMessageW(self.list, LVM_GETSELECTIONMARK, 0, 0))
                .ok()
                .and_then(target),
            anchor: if top > 0 { target(top) } else { None },
            settings: self.settings.clone(),
        });
        self.display_total = engine.index().len();
        for row in copies.paths.values_mut() {
            row.icon = self.icon_for(&row.text[0], row.is_dir);
        }
        self.frozen_copies = copies;
        self.list_frozen = true;
    }
    unsafe fn finish_list_refresh(&mut self, restore: Option<ViewPosition>) {
        SetTimer(self.hwnd, ACCESSIBILITY_TIMER, 50, None);
        SendMessageW(self.list, WM_SETREDRAW, 0, 0);
        let clear = LVITEMW {
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: 0,
            ..zeroed()
        };
        SendMessageW(
            self.list,
            LVM_SETITEMSTATE,
            usize::MAX,
            &clear as *const _ as isize,
        );
        if let Some(view) = restore.as_ref() {
            SendMessageW(
                self.list,
                LVM_SETSELECTIONMARK,
                0,
                view.selection_mark
                    .or(view.selected)
                    .map_or(-1, |i| i as isize),
            );
            let state = LVITEMW {
                stateMask: LVIS_SELECTED,
                state: LVIS_SELECTED,
                ..zeroed()
            };
            if view.selection.len() > self.rows.len() / 2 {
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    usize::MAX,
                    &state as *const _ as isize,
                );
                let mut selected = view.selection.iter().copied().peekable();
                for item in 0..self.rows.len() {
                    if selected.peek() == Some(&item) {
                        selected.next();
                    } else {
                        SendMessageW(
                            self.list,
                            LVM_SETITEMSTATE,
                            item,
                            &clear as *const _ as isize,
                        );
                    }
                }
            } else {
                for &item in &view.selection {
                    if item < self.rows.len() {
                        SendMessageW(
                            self.list,
                            LVM_SETITEMSTATE,
                            item,
                            &state as *const _ as isize,
                        );
                    }
                }
            }
        }
        if let Some(selected) = restore
            .as_ref()
            .and_then(|view| view.selected)
            .filter(|&row| row < self.rows.len())
        {
            let state = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                selected,
                &state as *const _ as isize,
            );
        }
        if !self.rows.is_empty() {
            let top = restore
                .as_ref()
                .map_or(0, |view| view.top)
                .min(self.rows.len() - 1);
            SendMessageW(self.list, LVM_ENSUREVISIBLE, top, 0);
            let current = SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0).max(0) as i64;
            let height = restore.as_ref().map_or(22, |view| view.row_height) as i64;
            let offset =
                ((top as i64 - current) * height).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            if offset != 0 {
                SendMessageW(self.list, LVM_SCROLL, 0, offset as isize);
            }
        }
        self.saved_view = None;
        self.frozen_copies = FrozenCopies::default();
        self.list_frozen = false;
        if IsWindow(self.list) != 0 {
            SendMessageW(self.list, WM_SETREDRAW, 1, 0);
            RedrawWindow(
                self.list,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }
    unsafe fn abort_list_refresh(&mut self) {
        if !self.list_frozen {
            return;
        }
        if self.engine.is_none() || self.rows_revision != self.revision {
            self.rows = Arc::new(Vec::new());
            self.accessibility_epoch += 1;
            self.rows_revision = self.revision;
            SendMessageW(
                self.list,
                LVM_SETITEMCOUNT,
                0,
                (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize,
            );
        }
        self.finish_list_refresh(None);
    }
    unsafe fn apply_pending(&mut self) {
        self.publish_update();
        if self.rename_target.is_some()
            || self.context_menu_active
            || self.drag_active
            || self.file_operation_in_progress
            || self.list_frozen
        {
            SetTimer(self.hwnd, CHANGE_TIMER, 900, None);
            return;
        }
        if self.indexing || self.updating || self.pending.is_empty() {
            return;
        }
        if self.pending.requires_full_scan(&self.settings.roots) {
            let delay = self.pending.full_scan_delay_ms();
            if delay > 1 {
                SetTimer(self.hwnd, CHANGE_TIMER, delay, None);
                return;
            }
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let (events, compacted, reconnect, full) = self.pending.take_work();
        self.full_recovery = full;
        if reconnect {
            // Reattach failed/overflowed watchers before the scan so changes
            // arriving during recovery cannot fall through an unwatched gap.
            self.configure_watcher();
        }
        self.updating = true;
        self.index_message = if events.is_err() {
            "대량 변경 자동 복구 중 · 기존 목록 검색 가능"
        } else if compacted {
            "대량 변경 폴더 복구 중 · 기존 목록 검색 가능"
        } else {
            "변경 반영 중 · 기존 목록 검색 가능"
        }
        .into();
        self.update_status();
        let generation = self.generation;
        let revision = self.revision;
        let tx = self.events.clone();
        let cancel = self.cancel.clone();
        let watcher_ready = self.watcher.as_ref().map(|watcher| watcher.ready.clone());
        std::thread::Builder::new()
            .name("index-update".into())
            .spawn(move || {
                background_priority();
                let result = wait_for_watcher(watcher_ready.as_deref(), &cancel).and_then(|_| {
                    background_work(|| {
                        refresh_snapshot(engine, events, &cancel, |progress| {
                            tx.send(AppEvent::UpdateProgress {
                                generation,
                                revision,
                                entries: progress.entries,
                            });
                        })
                    })
                });
                let changed = result.as_ref().is_ok_and(|(_, changed)| *changed);
                let failed_refresh = result.is_err();
                tx.send(AppEvent::Updated {
                    generation,
                    revision,
                    result: result.map(|(engine, _)| engine),
                    changed,
                    failed_refresh,
                });
            })
            .expect("update worker");
    }
    unsafe fn schedule_cache(&mut self) {
        // A deadline, not a sliding debounce: frequent publications must not
        // postpone persistence indefinitely.
        if !self.cache_save_scheduled {
            self.cache_save_scheduled = SetTimer(self.hwnd, CACHE_TIMER, 15000, None) != 0;
        }
    }
    unsafe fn save_cache(&mut self) {
        // Ordinary recovery never mutates the published snapshot. Persist it
        // even if another update is in flight, so busy roots cannot starve saves.
        // The elevated indexer still needs its exclusive writer barrier.
        if self.indexing {
            if self.cache_dirty {
                self.schedule_cache();
            }
            return;
        }
        if self.cache_dirty {
            if let Some(engine) = &self.engine {
                let _ = self
                    .cache_tx
                    .send(CacheRequest::Save(engine.index().clone()));
                self.cache_dirty = false;
            }
        }
    }
    unsafe fn command(&mut self, id: usize, notification: u32) {
        if self.initializing {
            return;
        }
        match id {
            QUERY if notification == EN_CHANGE => self.submit_search(),
            REGEX | PATH | CASE if notification == BN_CLICKED => self.submit_search(),
            KIND if notification == CBN_SELCHANGE => self.submit_search(),
            REFRESH if notification == BN_CLICKED && !self.indexing => {
                self.start_scan(false, false)
            }
            FAST if notification == BN_CLICKED && !self.indexing => self.start_scan(false, true),
            SETTINGS if notification == BN_CLICKED => self.show_settings(),
            _ => {}
        }
    }
    unsafe fn notify(&mut self, l: LPARAM) -> LRESULT {
        let header = &*(l as *const NMHDR);
        if header.code == HDN_ENDTRACKW {
            self.defer_save();
        }
        if header.hwndFrom != self.list {
            return 0;
        }
        match header.code {
            LVN_GETDISPINFOW => {
                if self.list_frozen {
                    let display = &mut *(l as *mut NMLVDISPINFOW);
                    let cached = self.frozen_copies.paths.get(&(display.item.iItem as usize));
                    if display.item.mask & LVIF_TEXT != 0 {
                        let text = cached
                            .and_then(|row| row.text.get(display.item.iSubItem as usize))
                            .map_or("", String::as_str);
                        write_text(display.item.pszText, display.item.cchTextMax, text);
                    }
                    if display.item.mask & LVIF_IMAGE != 0 {
                        display.item.iImage = cached.map_or(0, |row| row.icon);
                    }
                    return 0;
                }
                if !self.rows_current() {
                    return 0;
                }
                let display = &mut *(l as *mut NMLVDISPINFOW);
                let Some(&row) = self.rows.get(display.item.iItem as usize) else {
                    return 0;
                };
                let Some(engine) = &self.engine else {
                    return 0;
                };
                let index = engine.index();
                if row as usize >= index.len() {
                    return 0;
                }
                let entry = index.entry(row as usize);
                if display.item.mask & LVIF_TEXT != 0 {
                    let text = match display.item.iSubItem {
                        0 => entry.name.to_string(),
                        1 => index.parent_path(row as usize),
                        2 => {
                            if entry.is_dir {
                                "—".into()
                            } else {
                                format_size(entry.size)
                            }
                        }
                        3 => format_date(entry.created),
                        4 => format_date(entry.modified),
                        _ => String::new(),
                    };
                    write_text(display.item.pszText, display.item.cchTextMax, &text);
                }
                if display.item.mask & LVIF_IMAGE != 0 {
                    let name = entry.name.to_string();
                    let dir = entry.is_dir;
                    display.item.iImage = self.icon_for(&name, dir);
                }
                0
            }
            LVN_COLUMNCLICK => {
                let notification = &*(l as *const NMLISTVIEW);
                let field = match notification.iSubItem {
                    0 => SortField::Name,
                    1 => SortField::Path,
                    2 => SortField::Size,
                    3 => SortField::Created,
                    _ => SortField::Modified,
                };
                if self.settings.sort == field {
                    self.settings.descending = !self.settings.descending;
                } else {
                    self.settings.sort = field;
                    self.settings.descending = matches!(
                        field,
                        SortField::Size | SortField::Created | SortField::Modified
                    );
                }
                self.sort_indicator();
                self.submit_search();
                0
            }
            LVN_ITEMCHANGED | LVN_ODSTATECHANGED => {
                SetTimer(self.hwnd, ACCESSIBILITY_TIMER, 50, None);
                if !self.list_frozen {
                    self.update_status();
                }
                0
            }
            NM_CLICK => {
                let n = &*(l as *const NMITEMACTIVATE);
                if n.iItem >= 0
                    && GetKeyState(VK_CONTROL as i32) >= 0
                    && GetKeyState(VK_SHIFT as i32) >= 0
                    && SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) <= 1
                {
                    self.copy(n.iItem as usize, false);
                }
                0
            }
            NM_DBLCLK => {
                let n = &*(l as *const NMITEMACTIVATE);
                if n.iItem >= 0 {
                    match n.iSubItem {
                        0 => self.open(n.iItem as usize, false),
                        1 => self.open_parent(n.iItem as usize),
                        _ => {}
                    }
                }
                0
            }
            NM_RCLICK => {
                self.context_menu();
                0
            }
            LVN_BEGINDRAG => {
                let notification = &*(l as *const NMLISTVIEW);
                if notification.iItem >= 0 {
                    self.drag_files();
                }
                0
            }
            LVN_BEGINLABELEDITW => {
                if self.indexing || self.updating || !self.rows_current() {
                    return 1;
                }
                let notification = &*(l as *const NMLVDISPINFOW);
                if self.rename_target.is_none() {
                    self.rename_target = self.result_path(notification.item.iItem as usize);
                }
                self.rename_target.is_none() as isize
            }
            LVN_ENDLABELEDITW => {
                let notification = &*(l as *const NMLVDISPINFOW);
                let original = self.rename_target.take();
                if let Some(original) = original {
                    if !notification.item.pszText.is_null() {
                        let mut length = 0;
                        while length < 32768 && *notification.item.pszText.add(length) != 0 {
                            length += 1;
                        }
                        let name = String::from_utf16_lossy(std::slice::from_raw_parts(
                            notification.item.pszText,
                            length,
                        ));
                        self.rename_file(&original, &name);
                    }
                }
                if !self.pending.is_empty() {
                    SetTimer(self.hwnd, CHANGE_TIMER, 1, None);
                }
                // Text comes from the index; its change notification repaints
                // the row after a successful filesystem operation.
                0
            }
            NM_CUSTOMDRAW => {
                let draw = &mut *(l as *mut NMLVCUSTOMDRAW);
                if draw.nmcd.dwDrawStage == CDDS_PREPAINT {
                    return CDRF_NOTIFYITEMDRAW as isize;
                }
                if draw.nmcd.dwDrawStage == CDDS_ITEMPREPAINT {
                    draw.clrText = FG;
                    draw.clrTextBk = if draw.nmcd.dwItemSpec % 2 == 0 {
                        BG
                    } else {
                        0x003b3028
                    };
                    return CDRF_NOTIFYSUBITEMDRAW as isize;
                }
                if draw.nmcd.dwDrawStage == CDDS_ITEMPREPAINT | CDDS_SUBITEM
                    && matches!(draw.iSubItem, 0 | 1)
                {
                    self.draw_search_cell(draw);
                    return CDRF_SKIPDEFAULT as isize;
                }
                CDRF_DODEFAULT as isize
            }
            _ => 0,
        }
    }
    unsafe fn draw_search_cell(&mut self, draw: &NMLVCUSTOMDRAW) {
        let item = draw.nmcd.dwItemSpec;
        let column = draw.iSubItem as usize;
        let (text, icon, name, parent) = if self.list_frozen {
            let Some(row) = self.frozen_copies.paths.get(&item) else {
                return;
            };
            (
                row.text[column].clone(),
                row.icon,
                row.text[0].clone(),
                row.text[1].clone(),
            )
        } else {
            let Some(&row) = self.rows.get(item) else {
                return;
            };
            let Some(engine) = &self.engine else {
                return;
            };
            if self.rows_revision != self.revision || row as usize >= engine.index().len() {
                return;
            }
            let entry = engine.index().entry(row as usize);
            let text = if column == 0 {
                entry.name.to_owned()
            } else {
                engine.index().parent_path(row as usize)
            };
            let name = entry.name.to_owned();
            let dir = entry.is_dir;
            let parent = engine.index().parent_path(row as usize);
            (
                text,
                if column == 0 {
                    self.icon_for(&name, dir)
                } else {
                    0
                },
                name,
                parent,
            )
        };
        let dc = draw.nmcd.hdc;
        let saved = SaveDC(dc);
        let mut cell = RECT {
            left: LVIR_BOUNDS as i32,
            top: column as i32,
            ..zeroed()
        };
        SendMessageW(
            self.list,
            LVM_GETSUBITEMRECT,
            item,
            &mut cell as *mut _ as isize,
        );
        if column == 0 {
            cell.right = cell.left + SendMessageW(self.list, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
        }
        IntersectClipRect(dc, cell.left, cell.top, cell.right, cell.bottom);
        let state = SendMessageW(
            self.list,
            LVM_GETITEMSTATE,
            item,
            (LVIS_SELECTED | LVIS_FOCUSED) as isize,
        ) as u32;
        let background = if state & LVIS_SELECTED != 0 {
            0x0065492f
        } else if item % 2 == 0 {
            BG
        } else {
            0x003b3028
        };
        let brush = CreateSolidBrush(background);
        FillRect(dc, &cell, brush);
        DeleteObject(brush);
        SelectObject(dc, self.font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, FG);
        let pad = (6 * GetDpiForWindow(self.list).max(96) / 96) as i32;
        let mut label = cell;
        if column == 0 {
            let mut icon_rect = RECT {
                left: LVIR_ICON as i32,
                ..zeroed()
            };
            SendMessageW(
                self.list,
                LVM_GETITEMRECT,
                item,
                &mut icon_rect as *mut _ as isize,
            );
            if self.icon_list != 0 {
                let mut cx = 0;
                let mut cy = 0;
                ImageList_GetIconSize(self.icon_list as _, &mut cx, &mut cy);
                ImageList_Draw(
                    self.icon_list as _,
                    icon,
                    dc,
                    icon_rect.left,
                    cell.top + (cell.bottom - cell.top - cy) / 2,
                    ILD_TRANSPARENT,
                );
            }
            label.left = icon_rect.right + pad / 2;
        } else {
            label.left += pad;
        }
        label.right -= pad;
        if label.right <= label.left {
            RestoreDC(dc, saved);
            return;
        }
        let original: Vec<u16> = text.encode_utf16().collect();
        let mut rendered = original.clone();
        rendered.resize(original.len() + 5, 0);
        let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | DT_MODIFYSTRING;
        let mut rect = label;
        DrawTextExW(
            dc,
            rendered.as_mut_ptr(),
            original.len() as i32,
            &mut rect,
            flags,
            null_mut(),
        );
        let length = rendered
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(rendered.len());
        let mut visible = original
            .iter()
            .zip(&rendered[..length])
            .take_while(|(a, b)| a == b)
            .count();
        if length < original.len() && rendered[..length].ends_with(&[46, 46, 46]) {
            visible = visible.min(length.saturating_sub(3));
        }
        let mut metrics: TEXTMETRICW = zeroed();
        GetTextMetricsW(dc, &mut metrics);
        for span in self
            .highlighter
            .cell_spans(&name, &parent, self.settings.match_path, column)
        {
            let end = span.end.min(visible);
            if span.start >= end {
                continue;
            }
            let mut start_size: SIZE = zeroed();
            let mut end_size: SIZE = zeroed();
            GetTextExtentPoint32W(dc, original.as_ptr(), span.start as i32, &mut start_size);
            GetTextExtentPoint32W(dc, original.as_ptr(), end as i32, &mut end_size);
            let top = label.top + (label.bottom - label.top - metrics.tmHeight) / 2;
            let highlight = RECT {
                left: label.left + start_size.cx,
                top,
                right: (label.left + end_size.cx).min(label.right),
                bottom: top + metrics.tmHeight,
            };
            let region_saved = SaveDC(dc);
            IntersectClipRect(
                dc,
                highlight.left,
                highlight.top,
                highlight.right,
                highlight.bottom,
            );
            let mark = CreateSolidBrush(0x005bd4ff);
            FillRect(dc, &highlight, mark);
            DeleteObject(mark);
            SetTextColor(dc, 0x00201c19);
            let mut rect = label;
            DrawTextW(
                dc,
                rendered.as_ptr(),
                length as i32,
                &mut rect,
                DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            RestoreDC(dc, region_saved);
        }
        RestoreDC(dc, saved);
    }
    unsafe fn icon_for(&mut self, name: &str, dir: bool) -> i32 {
        let key = if dir {
            "<folder>".into()
        } else {
            Path::new(name)
                .extension()
                .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "<file>".into())
        };
        if let Some(&icon) = self.icons.get(&key) {
            return icon;
        }
        // Shell providers and Office icon handlers can load DLLs or touch disk.
        // Never invoke them in owner-data callbacks or while painting a frame.
        if self.icons.len() < 2048
            && !self.pending_icons.contains(&key)
            && self.icon_tx.try_send(key.clone()).is_ok()
        {
            self.pending_icons.insert(key);
        }
        self.icons
            .get(if dir { "<folder>" } else { "<file>" })
            .copied()
            .unwrap_or(0)
    }
    unsafe fn sort_indicator(&self) {
        let header = SendMessageW(self.list, LVM_GETHEADER, 0, 0) as HWND;
        let sorted = match self.settings.sort {
            SortField::Name => 0,
            SortField::Path => 1,
            SortField::Size => 2,
            SortField::Created => 3,
            SortField::Modified => 4,
        };
        for i in 0..5 {
            let mut item = HDITEMW {
                mask: HDI_FORMAT,
                ..zeroed()
            };
            SendMessageW(header, HDM_GETITEMW, i, &mut item as *mut _ as isize);
            item.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if i == sorted {
                item.fmt |= if self.settings.descending {
                    HDF_SORTDOWN
                } else {
                    HDF_SORTUP
                };
            }
            SendMessageW(header, HDM_SETITEMW, i, &item as *const _ as isize);
        }
    }
    unsafe fn selected(&self) -> Option<usize> {
        let focused = SendMessageW(
            self.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            (LVNI_FOCUSED | LVNI_SELECTED) as isize,
        );
        if focused >= 0 {
            return Some(focused as usize);
        }
        let selected = SendMessageW(
            self.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_SELECTED as isize,
        );
        (selected >= 0).then_some(selected as usize)
    }
    unsafe fn selected_items(&self) -> Vec<usize> {
        let count = SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0).max(0) as usize;
        let mut result = Vec::with_capacity(count);
        let mut item = usize::MAX;
        loop {
            let next = SendMessageW(self.list, LVM_GETNEXTITEM, item, LVNI_SELECTED as isize);
            if next < 0 {
                break;
            }
            item = next as usize;
            result.push(item);
        }
        result
    }
    unsafe fn selected_paths(&self) -> Result<Vec<String>, String> {
        if !self.rows_current() {
            return Err("목록 갱신이 끝난 뒤 다시 선택하세요.".into());
        }
        let count = SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0);
        if count > 4096 {
            return Err("파일 작업은 한 번에 4,096개까지 가능합니다. 나누어 선택하세요.".into());
        }
        let mut paths = Vec::new();
        let mut bytes = 0;
        for item in self.selected_items() {
            let path = self
                .result_path(item)
                .ok_or("목록이 변경되었습니다. 다시 선택하세요.")?;
            bytes += path.len();
            if bytes > 2 * 1024 * 1024 {
                return Err("경로 합계가 2MiB를 초과했습니다. 나누어 선택하세요.".into());
            }
            paths.push(path);
        }
        Ok(paths)
    }
    fn result_path(&self, item: usize) -> Option<String> {
        if !self.rows_current() {
            return None;
        }
        self.rows.get(item).and_then(|row| {
            self.engine.as_ref().and_then(|engine| {
                ((*row as usize) < engine.index().len())
                    .then(|| engine.index().full_path(*row as usize))
            })
        })
    }
    unsafe fn copy(&mut self, item: usize, force: bool) {
        if !force && crate::file_clipboard::has_files() {
            self.notice = "파일 클립보드 유지 · Ctrl+V: 붙여넣기 · Ctrl+Shift+C: 경로 복사".into();
            self.update_status();
            return;
        }
        let path = if self.list_frozen {
            self.frozen_copies
                .paths
                .get(&item)
                .map(|row| row.path.clone())
        } else {
            self.result_path(item)
        };
        if let Some(path) = path {
            self.notice = match platform::copy_path(&path) {
                Ok(()) => format!("경로 복사됨 · {path}"),
                Err(e) => e,
            };
            self.update_status();
        }
    }
    unsafe fn open(&mut self, item: usize, reveal: bool) {
        if let Some(path) = self.result_path(item) {
            let result = if reveal {
                platform::reveal_path(&path)
            } else {
                platform::open_path(&path)
            };
            if let Err(e) = result {
                self.notice = e;
                self.update_status();
            }
        }
    }
    unsafe fn open_parent(&mut self, item: usize) {
        let Some(path) = self.result_path(item) else {
            return;
        };
        let Some(parent) = Path::new(&path).parent() else {
            return;
        };
        let parent = parent.to_string_lossy().into_owned();
        let events = self.events.clone();
        std::thread::spawn(move || {
            if let Err(error) = platform::reveal_path(&parent) {
                events.send(AppEvent::Error(error));
            }
        });
    }
    unsafe fn prepare_drag_files(&self) -> Result<Vec<crate::file_clipboard::DragFile>, String> {
        if self.file_operation_in_progress
            || self.context_menu_active
            || self.rename_target.is_some()
            || self.searching
            || !self.rows_current()
        {
            return Err("파일 작업과 검색 결과 표시가 끝난 뒤 다시 선택해 주세요.".into());
        }
        let paths = self.selected_paths()?;
        Ok(self
            .selected_items()
            .into_iter()
            .zip(&paths)
            .map(|(item, path)| {
                let entry = self
                    .engine
                    .as_ref()
                    .unwrap()
                    .index()
                    .entry(self.rows[item] as usize);
                crate::file_clipboard::DragFile {
                    path: path.clone(),
                    is_dir: entry.is_dir,
                    size: entry.size,
                    created: entry.created,
                    modified: entry.modified,
                }
            })
            .collect())
    }
    unsafe fn drag_files(&mut self) {
        // Background work does not invalidate the displayed snapshot. Capture
        // selected paths/metadata now; drag_active defers any publication while
        // the Shell's modal drag loop is running.
        let files = match self.prepare_drag_files() {
            Ok(files) if !files.is_empty() => files,
            Ok(_) => return,
            Err(error) => {
                self.notice = error;
                self.update_status();
                return;
            }
        };
        let paths: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
        self.drag_active = true;
        self.file_operation_in_progress = true;
        ReleaseCapture();
        let result = crate::file_clipboard::drag_many(self.list as isize, &files);
        self.drag_active = false;
        self.file_operation_in_progress = false;
        self.notice = match result {
            Ok(true) => format!("파일 드롭 전달 완료 · {}개 항목", paths.len()),
            Ok(false) => "파일 드래그를 취소했습니다.".into(),
            Err(error) => error,
        };
        // A target can move some files before cancelling or failing. Recheck
        // captured source paths even if the watcher stopped for an exit request.
        self.file_refresh_pending = true;
        for path in paths {
            self.pending.enqueue(
                Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
                    .add_path(PathBuf::from(path)),
                self.hwnd as isize,
            );
        }
        self.schedule_changes();
        self.update_status();
        self.finish_file_refresh();
        if !self.exit_after_file_operation
            && let Some(rebuild) = self.refresh_after_operation.take()
        {
            self.start_scan(false, rebuild);
        }
    }
    unsafe fn copy_file(&mut self, item: usize, cut: bool) {
        if self.file_operation_in_progress || self.searching || !self.rows_current() {
            self.notice = "파일 작업과 목록 갱신이 끝난 뒤 다시 선택해 주세요.".into();
            self.update_status();
            return;
        }
        let paths = match self.selected_paths() {
            Ok(paths) if !paths.is_empty() => paths,
            Ok(_) => return,
            Err(error) => {
                self.notice = error;
                self.update_status();
                return;
            }
        };
        let _ = item;
        self.notice = match crate::file_clipboard::copy_many(&paths, cut) {
            Ok(()) if cut => format!("잘라내기 준비 · {}개 항목", paths.len()),
            Ok(()) => format!("파일 복사됨 · {}개 항목", paths.len()),
            Err(error) => error,
        };
        self.update_status();
    }
    unsafe fn paste_files(&mut self, item: usize) {
        if SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) != 1 {
            self.notice = "붙여넣을 폴더나 그 폴더의 파일 하나를 선택하세요.".into();
            self.update_status();
            return;
        }
        if self.file_operation_in_progress
            || self.updating
            || self.searching
            || !self.rows_current()
        {
            self.notice = "파일 작업과 목록 갱신이 끝난 뒤 다시 선택해 주세요.".into();
            self.update_status();
            return;
        }
        let Some(path) = self.result_path(item) else {
            return;
        };
        // A folder row is the destination itself. For a file row use its
        // containing folder, matching the folder-path column.
        let destination = if Path::new(&path).is_dir() {
            PathBuf::from(&path)
        } else if let Some(parent) = Path::new(&path).parent() {
            parent.to_owned()
        } else {
            return;
        };
        let sequence = crate::file_clipboard::sequence();
        let owner = self.hwnd as isize;
        let events = self.events.clone();
        self.file_operation_in_progress = true;
        self.notice = format!("파일 붙여넣기 중 · {}", destination.display());
        self.update_status();
        if let Err(error) = std::thread::Builder::new()
            .name("file-paste".into())
            .spawn(move || {
                events.send(AppEvent::Pasted(crate::file_clipboard::paste(
                    owner,
                    destination,
                    sequence,
                )));
            })
        {
            self.file_operation_in_progress = false;
            self.notice = format!("붙여넣기를 시작할 수 없습니다: {error}");
            self.update_status();
        }
    }
    unsafe fn context_menu(&mut self) {
        if self.file_operation_in_progress {
            return;
        }
        let Some(item) = self.selected() else {
            return;
        };
        let Some(path) = self.result_path(item) else {
            return;
        };
        let mut point: POINT = zeroed();
        GetCursorPos(&mut point);
        let paths = match self.selected_paths() {
            Ok(paths) => paths,
            Err(error) => {
                self.notice = error;
                self.update_status();
                return;
            }
        };
        self.context_menu_active = true;
        let action = crate::shell_menu::show_many(self.hwnd as isize, &paths, point.x, point.y);
        self.context_menu_active = false;
        if !self.pending.is_empty() {
            self.schedule_changes();
        }
        match action {
            Ok(crate::shell_menu::MenuAction::OpenLocation) => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    if let Err(error) = platform::reveal_path(&path) {
                        events.send(AppEvent::Error(error));
                    }
                });
            }
            Ok(crate::shell_menu::MenuAction::CopyPath) => {
                self.notice = match platform::copy_path(&paths.join("\r\n")) {
                    Ok(()) => format!("경로 복사됨 · {}개 항목", paths.len()),
                    Err(error) => error,
                };
                self.update_status();
            }
            Ok(crate::shell_menu::MenuAction::Rename) => {
                if self.result_path(item).as_deref() == Some(&path) {
                    self.begin_rename(item);
                } else {
                    self.notice = "목록이 변경되었습니다. 파일을 다시 선택하세요.".into();
                    self.update_status();
                }
            }
            Ok(crate::shell_menu::MenuAction::CopyFiles) => self.copy_file(item, false),
            Ok(crate::shell_menu::MenuAction::CutFiles) => self.copy_file(item, true),
            Ok(crate::shell_menu::MenuAction::DeleteFiles) => self.delete_file(item),
            Err(error) => {
                self.notice = error;
                self.update_status();
            }
            _ => {}
        }
    }
    unsafe fn begin_rename(&mut self, item: usize) {
        if SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) > 1 {
            self.notice = "이름을 변경할 항목 하나를 선택하세요.".into();
            self.update_status();
            return;
        }
        if self.indexing || self.updating || self.file_operation_in_progress {
            return;
        }
        self.rename_target = self.result_path(item);
        if self.rename_target.is_none() {
            return;
        }
        SetFocus(self.list);
        let edit = SendMessageW(self.list, LVM_EDITLABELW, item, 0) as HWND;
        if edit.is_null() {
            self.rename_target = None;
        }
    }
    unsafe fn rename_file(&mut self, original: &str, name: &str) {
        if name.is_empty()
            || matches!(name, "." | "..")
            || name.ends_with([' ', '.'])
            || name
                .chars()
                .any(|ch| ch < ' ' || "\\/:*?\"<>|".contains(ch))
        {
            self.notice = "파일 이름에 사용할 수 없는 문자가 있습니다.".into();
            self.update_status();
            return;
        }
        let target = Path::new(original).with_file_name(name);
        if target == Path::new(original) {
            return;
        }
        let target_text = target.to_string_lossy().into_owned();
        // MoveFileW fails if a different file already has the requested name;
        // it never replaces that file, even if it appears during this call.
        if MoveFileW(wide(original).as_ptr(), wide(&target_text).as_ptr()) == 0 {
            self.notice = format!(
                "이름 변경 실패: {}",
                std::io::Error::from_raw_os_error(GetLastError() as i32)
            );
        } else {
            self.notice = format!("이름 변경됨 · {name}");
            let change = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )))
            .add_path(PathBuf::from(original))
            .add_path(target);
            self.pending.enqueue(change, self.hwnd as isize);
        }
        self.update_status();
    }
    unsafe fn delete_file(&mut self, item: usize) {
        if self.file_operation_in_progress || self.context_menu_active {
            return;
        }
        if self.indexing || self.updating || self.searching || !self.rows_current() {
            self.notice = "목록 갱신이 끝난 뒤 다시 선택해 주세요.".into();
            self.update_status();
            return;
        }
        // Capture the original path before entering a modal message loop. A
        // later search or index update must never change the deletion target.
        let paths = match self.selected_paths() {
            Ok(paths) if !paths.is_empty() => paths,
            Ok(_) => return,
            Err(error) => {
                self.notice = error;
                self.update_status();
                return;
            }
        };
        let _ = item;
        self.file_operation_in_progress = true;
        self.context_menu_active = true;
        let preview = paths
            .iter()
            .take(8)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        let more = if paths.len() > 8 {
            format!("\n외 {}개 항목", paths.len() - 8)
        } else {
            String::new()
        };
        let message = format!(
            "선택한 {}개 항목을 삭제하시겠습니까?\n\n{preview}{more}\n\n폴더는 내부 항목도 함께 삭제됩니다.\n가능한 경우 휴지통으로 보냅니다. 영구 삭제가 필요하면 Windows에서 다시 확인합니다.",
            paths.len()
        );
        let answer = MessageBoxW(
            self.hwnd,
            wide(&message).as_ptr(),
            wide("Hyperfinder · 삭제 확인").as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        );
        self.context_menu_active = false;
        if !self.pending.is_empty() {
            self.schedule_changes();
        }
        if answer != IDYES || self.exit_after_file_operation {
            self.file_operation_in_progress = false;
            if self.exit_after_file_operation {
                self.request_exit();
            }
            return;
        }
        self.notice = format!("삭제 중 · {}개 항목", paths.len());
        self.update_status();
        let events = self.events.clone();
        let owner = self.hwnd as isize;
        if let Err(error) = std::thread::Builder::new()
            .name("file-delete".into())
            .spawn(move || {
                let result = platform::delete_paths(owner, &paths);
                events.send(AppEvent::Deleted { paths, result });
            })
        {
            self.file_operation_in_progress = false;
            self.notice = format!("삭제 작업을 시작할 수 없습니다: {error}");
            self.update_status();
        }
    }
    unsafe fn keyboard(&mut self, msg: &MSG) -> bool {
        if msg.message != WM_KEYDOWN {
            return false;
        }
        if self.rename_target.is_some() {
            return false;
        }
        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
        match msg.wParam as u16 {
            VK_ESCAPE => {
                self.hide();
                true
            }
            key if ctrl && (key == b'F' as u16 || key == b'L' as u16) => {
                SetFocus(self.query);
                SendMessageW(self.query, EM_SETSEL, 0, -1);
                true
            }
            key if ctrl && key == b'A' as u16 && GetFocus() == self.query => {
                SendMessageW(self.query, EM_SETSEL, 0, -1);
                true
            }
            key if ctrl && key == b'A' as u16 && GetFocus() == self.list => {
                if self.rows_current() {
                    let state = LVITEMW {
                        stateMask: LVIS_SELECTED,
                        state: LVIS_SELECTED,
                        ..zeroed()
                    };
                    SendMessageW(
                        self.list,
                        LVM_SETITEMSTATE,
                        usize::MAX,
                        &state as *const _ as isize,
                    );
                }
                true
            }
            key if ctrl && key == b'C' as u16 && GetFocus() == self.list => {
                if let Some(item) = self.selected() {
                    if GetKeyState(VK_SHIFT as i32) < 0 {
                        match self
                            .selected_paths()
                            .and_then(|paths| platform::copy_path(&paths.join("\r\n")))
                        {
                            Ok(()) => self.notice = "선택한 항목의 경로 복사됨".into(),
                            Err(error) => self.notice = error,
                        }
                        self.update_status();
                    } else if msg.lParam & (1isize << 30) == 0 {
                        self.copy_file(item, false);
                    }
                }
                true
            }
            key if ctrl && key == b'X' as u16 && GetFocus() == self.list => {
                if msg.lParam & (1isize << 30) == 0 {
                    if let Some(item) = self.selected() {
                        self.copy_file(item, true);
                    }
                }
                true
            }
            key if ctrl && key == b'V' as u16 && GetFocus() == self.list => {
                if msg.lParam & (1isize << 30) == 0 {
                    if let Some(item) = self.selected() {
                        self.paste_files(item);
                    } else {
                        self.notice = "붙여넣을 폴더나 그 폴더 안의 파일을 선택하세요.".into();
                        self.update_status();
                    }
                }
                true
            }
            VK_RETURN if GetFocus() == self.list => {
                if let Some(item) = self.selected() {
                    self.open(item, false);
                }
                true
            }
            VK_DOWN if GetFocus() == self.query && !self.rows.is_empty() && !self.list_frozen => {
                SetFocus(self.list);
                let state = LVITEMW {
                    stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                    state: LVIS_SELECTED | LVIS_FOCUSED,
                    ..zeroed()
                };
                SendMessageW(self.list, LVM_SETITEMSTATE, 0, &state as *const _ as isize);
                true
            }
            VK_F5 if !self.indexing => {
                self.start_scan(false, GetKeyState(VK_SHIFT as i32) < 0);
                true
            }
            VK_F2 if GetFocus() == self.list => {
                if let Some(item) = self.selected() {
                    self.begin_rename(item);
                }
                true
            }
            VK_DELETE if GetFocus() == self.list => {
                // Only the first plain Delete press opens confirmation. Editing
                // text and holding the key must not cause repeated deletions.
                if !ctrl && GetKeyState(VK_SHIFT as i32) >= 0 && msg.lParam & (1isize << 30) == 0 {
                    if let Some(item) = self.selected() {
                        self.delete_file(item);
                    }
                }
                true
            }
            VK_APPS if GetFocus() == self.list => {
                self.context_menu();
                true
            }
            VK_F10 if GetFocus() == self.list && GetKeyState(VK_SHIFT as i32) < 0 => {
                self.context_menu();
                true
            }
            _ => false,
        }
    }
    unsafe fn handle_shell(&mut self) {
        while let Some(event) = self.shell.as_ref().and_then(|shell| shell.try_recv()) {
            match event {
                ShellEvent::Toggle => {
                    if IsWindowVisible(self.hwnd) != 0 && GetForegroundWindow() == self.hwnd {
                        self.hide();
                    } else {
                        self.show();
                    }
                }
                ShellEvent::Show => self.show(),
                ShellEvent::Exit => {
                    self.request_exit();
                }
            }
        }
    }
    unsafe fn request_exit(&mut self) {
        if self.file_operation_in_progress || self.file_refresh_pending {
            self.exit_after_file_operation = true;
            // The captured deletion path is explicitly refreshed on completion.
            // Stop unrelated incoming changes so a busy drive cannot postpone
            // an already requested exit indefinitely.
            self.watcher = None;
            self.notice = "파일 작업을 마친 뒤 종료합니다.".into();
            self.update_status();
            self.finish_file_refresh();
            return;
        }
        self.remember_geometry();
        DestroyWindow(self.hwnd);
    }
    unsafe fn show(&self) {
        ShowWindow(
            self.hwnd,
            if IsIconic(self.hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(self.hwnd);
        SetFocus(self.query);
        if !self.pending.is_empty() {
            SetTimer(self.hwnd, CHANGE_TIMER, 1, None);
        }
    }
    unsafe fn hide(&mut self) {
        self.remember_geometry();
        let _ = self.settings.save();
        SetFocus(null_mut());
        ShowWindow(self.hwnd, SW_HIDE);
        self.schedule_changes();
    }
    unsafe fn schedule_changes(&self) {
        if self.pending.is_empty() {
            return;
        }
        let foreground = IsWindowVisible(self.hwnd) != 0
            && IsIconic(self.hwnd) == 0
            && GetForegroundWindow() == self.hwnd;
        let delay = self.pending.delay_ms(foreground);
        let delay = if self.pending.requires_full_scan(&self.settings.roots) {
            delay.max(self.pending.full_scan_delay_ms())
        } else {
            delay
        };
        SetTimer(self.hwnd, CHANGE_TIMER, delay, None);
    }
    unsafe fn defer_save(&self) {
        SetTimer(self.hwnd, SAVE_TIMER, 450, None);
    }
    unsafe fn remember_geometry(&mut self) {
        let mut placement: WINDOWPLACEMENT = zeroed();
        placement.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(self.hwnd, &mut placement) != 0 {
            self.settings.maximized = placement.showCmd == SW_SHOWMAXIMIZED as u32;
            let r = placement.rcNormalPosition;
            if r.right - r.left >= 600 && r.bottom - r.top >= 300 {
                self.settings.window_size = [(r.right - r.left) as f32, (r.bottom - r.top) as f32];
                self.settings.window_pos = Some([r.left as f32, r.top as f32]);
            }
        }
        for i in 0..5 {
            let width = SendMessageW(self.list, LVM_GETCOLUMNWIDTH, i, 0) as i32;
            if width > 0 {
                self.settings.column_widths[i] = width;
            }
        }
        self.settings.query = control_text(self.query);
    }
    unsafe fn update_status(&self) {
        if self.list_frozen && self.notice.is_empty() {
            return;
        }
        let total = self
            .engine
            .as_ref()
            .map_or(self.display_total, |engine| engine.index().len());
        let text = if !self.notice.is_empty() {
            self.notice.clone()
        } else {
            format!(
                "{} / {}개  ·  {:.0}ms  ·  {}{}  ·  {}",
                self.rows.len(),
                total,
                self.elapsed,
                if !self.updating && !self.indexing && !self.pending.is_empty() {
                    "변경 대기 · 기존 목록 검색 가능"
                } else {
                    &self.index_message
                },
                if self.searching { " · 검색 중" } else { "" },
                if self
                    .shell
                    .as_ref()
                    .is_some_and(|shell| shell.hotkey_error.is_some())
                {
                    "단축키를 설정에서 확인하세요."
                } else {
                    &self.settings.hotkey
                }
            )
        };
        let selected = SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0);
        let text = if selected > 1 {
            format!("{selected}개 선택 · {text}")
        } else {
            text
        };
        if *self.status_text.borrow() != text {
            SetWindowTextW(self.status, wide(&text).as_ptr());
            *self.status_text.borrow_mut() = text;
        }
    }
    unsafe fn draw_button(&self, l: LPARAM) {
        let draw = &*(l as *const DRAWITEMSTRUCT);
        if draw.CtlType == ODT_COMBOBOX {
            let selected =
                draw.itemState & ODS_SELECTED != 0 && draw.itemState & ODS_COMBOBOXEDIT == 0;
            let brush = CreateSolidBrush(if selected { 0x00694a36 } else { INPUT_BG });
            FillRect(draw.hDC, &draw.rcItem, brush);
            DeleteObject(brush);
            SetTextColor(draw.hDC, FG);
            SetBkMode(draw.hDC, TRANSPARENT as i32);
            SelectObject(draw.hDC, self.font);
            let label = match draw.itemID {
                0 => "파일 + 폴더",
                1 => "파일",
                2 => "폴더",
                _ => "",
            };
            let text = wide(label);
            let mut rect = draw.rcItem;
            rect.left += 5;
            rect.right -= 3;
            DrawTextW(
                draw.hDC,
                text.as_ptr(),
                -1,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            if draw.itemState & ODS_FOCUS != 0 {
                let mut rect = draw.rcItem;
                InflateRect(&mut rect, -2, -2);
                DrawFocusRect(draw.hDC, &rect);
            }
            return;
        }
        let active = draw.itemState & ODS_SELECTED != 0;
        let brush = CreateSolidBrush(if active { 0x00694a36 } else { INPUT_BG });
        FillRect(draw.hDC, &draw.rcItem, brush);
        DeleteObject(brush);
        SetTextColor(draw.hDC, FG);
        SetBkMode(draw.hDC, TRANSPARENT as i32);
        SelectObject(draw.hDC, self.font);
        let mut text = wide(&control_text(draw.hwndItem));
        let mut rect = draw.rcItem;
        DrawTextW(
            draw.hDC,
            text.as_mut_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        if draw.itemState & ODS_FOCUS != 0 {
            let mut focus = draw.rcItem;
            InflateRect(&mut focus, -3, -3);
            DrawFocusRect(draw.hDC, &focus);
        }
    }
    unsafe fn release_error(&mut self, error: &str) {
        self.notice = error.into();
        if !self.settings_window.is_null() {
            SetWindowTextW(self.settings_error, wide(error).as_ptr());
        }
        self.update_status();
    }
    unsafe fn refresh_release_controls(&self) {
        if self.settings_window.is_null() {
            return;
        }
        SetWindowTextW(
            GetDlgItem(self.settings_window, RELEASE_STATUS as i32),
            wide(&self.release_text).as_ptr(),
        );
        let button = GetDlgItem(self.settings_window, CHECK_RELEASE as i32);
        SetWindowTextW(
            button,
            wide(if self.release_busy {
                "처리 중…"
            } else if self.staged_release.is_some() {
                "설치 후 재실행"
            } else if self.available_release.is_some() {
                "업데이트 설치…"
            } else {
                "업데이트 확인"
            })
            .as_ptr(),
        );
        EnableWindow(button, (!self.release_busy) as i32);
    }
    unsafe fn check_release(&mut self, manual: bool) {
        if self.release_busy {
            return;
        }
        if self.staged_release.is_some() {
            self.install_release();
            return;
        }
        if self.available_release.is_some() {
            self.download_release();
            return;
        }
        self.release_busy = true;
        self.release_text = format!("현재 v{} · 확인 중…", updater::VERSION);
        self.refresh_release_controls();
        let events = self.events.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("release-check".into())
            .spawn(move || {
                events.send(AppEvent::ReleaseChecked {
                    manual,
                    result: updater::check(),
                });
            })
        {
            self.release_busy = false;
            self.release_error(&error.to_string());
            self.refresh_release_controls();
        }
    }
    unsafe fn download_release(&mut self) {
        let Some(release) = self.available_release.clone() else {
            return;
        };
        if self.release_busy {
            return;
        }
        let owner = if self.settings_window.is_null() {
            self.hwnd
        } else {
            self.settings_window
        };
        let answer = MessageBoxW(owner, wide(&format!("현재 v{} → 새 버전 v{}\n\n업데이트를 다운로드하고 앱을 다시 실행할까요?\n저장된 설정과 인덱스는 유지됩니다. 저장하지 않은 설정은 적용되지 않습니다.", updater::VERSION, release.version)).as_ptr(), wide("Hyperfinder 업데이트").as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2);
        if answer != IDYES {
            return;
        }
        self.release_busy = true;
        self.release_text = format!("v{} 다운로드 중…", release.version);
        self.refresh_release_controls();
        let events = self.events.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("release-download".into())
            .spawn(move || {
                events.send(AppEvent::ReleaseDownloaded(updater::download(&release)));
            })
        {
            self.release_busy = false;
            self.release_error(&error.to_string());
            self.refresh_release_controls();
        }
    }
    unsafe fn install_release(&mut self) {
        if self.file_operation_in_progress
            || self.file_refresh_pending
            || self.drag_active
            || self.context_menu_active
            || self.rename_target.is_some()
            || self.indexing
        {
            self.release_error("다운로드가 완료되었습니다. 진행 중인 파일 작업·인덱싱을 마친 뒤 ‘설치 후 재실행’을 누르세요.");
            return;
        }
        self.remember_geometry();
        if let Err(error) = self.settings.save() {
            self.release_error(&error);
            return;
        }
        if let Some(staged) = self.staged_release.take() {
            match staged.launch() {
                Ok(()) => self.request_exit(),
                Err(error) => {
                    self.release_text = "설치 실패 · 다시 시도하세요".into();
                    self.release_error(&error);
                }
            }
        }
    }
    unsafe fn show_settings(&mut self) {
        if !self.settings_window.is_null() {
            SetForegroundWindow(self.settings_window);
            return;
        }
        let mut rect: RECT = zeroed();
        GetWindowRect(self.hwnd, &mut rect);
        let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
        let mut width = (580.0 * scale) as i32;
        let mut height = (720.0 * scale) as i32;
        let mut x = rect.left + 70;
        let mut y = rect.top + 70;
        let mut monitor: MONITORINFO = zeroed();
        monitor.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(
            MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        ) != 0
        {
            let work = monitor.rcWork;
            width = width.min(work.right - work.left);
            height = height.min(work.bottom - work.top);
            x = x.clamp(work.left, work.right - width);
            y = y.clamp(work.top, work.bottom - height);
        }
        self.settings_window = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            wide(SETTINGS_CLASS).as_ptr(),
            wide("Hyperfinder 설정").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            width,
            height,
            self.hwnd,
            null_mut(),
            GetModuleHandleW(null()),
            self as *mut App as _,
        );
        self.refresh_release_controls();
        SetFocus(GetDlgItem(self.settings_window, SAVE_SETTINGS as i32));
    }
    unsafe fn settings_controls(&mut self, hwnd: HWND) {
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &mut client);
        let available = ((client.bottom - client.top) as f32 / scale) as i32;
        let list_height = (110 - ((680 - available).max(0) + 1) / 2).max(54);
        let shift = 110 - list_height;
        let s = |v: i32| (v as f32 * scale).round() as i32;
        let place = |child, x, y, width, height| {
            MoveWindow(child, s(x), s(y), s(width), s(height), 1);
        };
        let dark: i32 = 1;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const _ as _,
            4,
        );
        let label = self.control("Static", "전역 단축키", 0, 210, hwnd);
        place(label, 16, 14, 530, 24);
        self.hotkey_edit = self.control(
            "Edit",
            &self.settings.hotkey,
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32 | ES_READONLY as u32,
            HOTKEY_EDIT,
            hwnd,
        );
        place(self.hotkey_edit, 16, 42, 530, 28);
        SetWindowSubclass(
            self.hotkey_edit,
            Some(hotkey_subclass),
            HOTKEY_EDIT,
            self as *mut Self as usize,
        );
        let label = self.control(
            "Static",
            "칸을 클릭한 뒤 Ctrl / Alt / Shift / Win과 키를 함께 누르세요.",
            0,
            214,
            hwnd,
        );
        place(label, 16, 75, 530, 24);
        let label = self.control("Static", "검색 폴더", 0, 211, hwnd);
        place(label, 16, 108, 530, 24);
        self.roots_edit = self.control(
            "ListBox",
            "",
            WS_TABSTOP
                | WS_BORDER
                | WS_VSCROLL
                | WS_HSCROLL
                | LBS_EXTENDEDSEL as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            ROOTS_EDIT,
            hwnd,
        );
        place(self.roots_edit, 16, 136, 430, list_height);
        for root in &self.settings.roots {
            SendMessageW(
                self.roots_edit,
                LB_ADDSTRING,
                0,
                wide(&root.to_string_lossy()).as_ptr() as isize,
            );
        }
        let add = self.control(
            "Button",
            "추가…",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            ADD_ROOT,
            hwnd,
        );
        let remove = self.control(
            "Button",
            "삭제",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            REMOVE_ROOT,
            hwnd,
        );
        place(add, 458, 136, 88, 30);
        place(remove, 458, 174, 88, 30);
        self.folder_extent(self.roots_edit);
        let label = self.control("Static", "제외 폴더 (하위 항목 포함)", 0, 223, hwnd);
        place(label, 16, 260 - shift, 530, 24);
        self.excluded_edit = self.control(
            "ListBox",
            "",
            WS_TABSTOP
                | WS_BORDER
                | WS_VSCROLL
                | WS_HSCROLL
                | LBS_EXTENDEDSEL as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            EXCLUDED_EDIT,
            hwnd,
        );
        place(self.excluded_edit, 16, 288 - shift, 430, list_height);
        for folder in &self.settings.excluded_folders {
            SendMessageW(
                self.excluded_edit,
                LB_ADDSTRING,
                0,
                wide(&folder.to_string_lossy()).as_ptr() as isize,
            );
        }
        let add = self.control(
            "Button",
            "추가…",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            ADD_EXCLUDED,
            hwnd,
        );
        let remove = self.control(
            "Button",
            "해제",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            REMOVE_EXCLUDED,
            hwnd,
        );
        place(add, 458, 288 - shift, 88, 30);
        place(remove, 458, 326 - shift, 88, 30);
        self.folder_extent(self.excluded_edit);
        let label = self.control("Static", "글자 크기 (8~20pt)", 0, 212, hwnd);
        place(label, 16, 420 - shift * 2, 240, 28);
        self.font_edit = self.control(
            "Edit",
            &format!("{}", self.settings.font_size),
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
            FONT_EDIT,
            hwnd,
        );
        place(self.font_edit, 270, 418 - shift * 2, 90, 28);
        self.auto_fast = self.control(
            "Button",
            "시작 시 고속 동기화 · 관리자 권한 확인",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            AUTO_FAST,
            hwnd,
        );
        SetWindowTheme(self.auto_fast, wide("").as_ptr(), wide("").as_ptr());
        SendMessageW(
            self.auto_fast,
            BM_SETCHECK,
            self.settings.fast_index as usize,
            0,
        );
        place(self.auto_fast, 16, 458 - shift * 2, 530, 28);
        self.settings_error = self.control(
            "Static",
            "제외 폴더와 하위 항목은 검색하지 않습니다. 목록을 변경해도 실제 폴더는 삭제하지 않습니다.",
            0,
            213,
            hwnd,
        );
        place(self.settings_error, 16, 496 - shift * 2, 530, 48);
        self.auto_release = self.control(
            "Button",
            "시작할 때 새 버전 확인",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            AUTO_RELEASE,
            hwnd,
        );
        SetWindowTheme(self.auto_release, wide("").as_ptr(), wide("").as_ptr());
        SendMessageW(
            self.auto_release,
            BM_SETCHECK,
            self.settings.check_updates_on_startup as usize,
            0,
        );
        place(self.auto_release, 16, 548 - shift * 2, 530, 28);
        let version = self.control("Static", &self.release_text, 0, RELEASE_STATUS, hwnd);
        place(version, 16, 586 - shift * 2, 305, 28);
        let check = self.control(
            "Button",
            "업데이트 확인",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            CHECK_RELEASE,
            hwnd,
        );
        place(check, 330, 582 - shift * 2, 216, 32);
        // WM_CREATE runs before settings_window receives CreateWindowEx's return.
        EnableWindow(check, (!self.release_busy) as i32);
        let save = self.control(
            "Button",
            "저장",
            WS_TABSTOP | BS_OWNERDRAW as u32,
            SAVE_SETTINGS,
            hwnd,
        );
        place(save, 446, 630 - shift * 2, 100, 30);
    }
    unsafe fn folder_paths(&self, list: HWND) -> Vec<PathBuf> {
        let count = SendMessageW(list, LB_GETCOUNT, 0, 0).max(0) as usize;
        (0..count)
            .filter_map(|i| {
                let length = SendMessageW(list, LB_GETTEXTLEN, i, 0);
                if !(0..=32768).contains(&length) {
                    return None;
                }
                let mut buffer = vec![0u16; length as usize + 1];
                SendMessageW(list, LB_GETTEXT, i, buffer.as_mut_ptr() as isize);
                Some(PathBuf::from(String::from_utf16_lossy(
                    &buffer[..length as usize],
                )))
            })
            .collect()
    }
    unsafe fn folder_extent(&self, list: HWND) {
        let dc = GetDC(list);
        let old = SelectObject(dc, self.font);
        let width = self
            .folder_paths(list)
            .iter()
            .map(|p| {
                let text = wide(&p.to_string_lossy());
                let mut size: SIZE = zeroed();
                GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32 - 1, &mut size);
                size.cx + 24
            })
            .max()
            .unwrap_or(0);
        SendMessageW(list, LB_SETHORIZONTALEXTENT, width as usize, 0);
        SelectObject(dc, old);
        ReleaseDC(list, dc);
    }
    unsafe fn add_folders(&mut self, list: HWND) {
        match platform::choose_folders(self.settings_window as isize) {
            Ok(paths) => {
                let mut existing = self.folder_paths(list);
                for path in paths {
                    if !existing
                        .iter()
                        .any(|old| normalized(old) == normalized(&path))
                    {
                        SendMessageW(
                            list,
                            LB_ADDSTRING,
                            0,
                            wide(&path.to_string_lossy()).as_ptr() as isize,
                        );
                        existing.push(path);
                    }
                }
                self.folder_extent(list);
            }
            Err(error) => {
                SetWindowTextW(self.settings_error, wide(&error).as_ptr());
            }
        }
    }
    unsafe fn remove_folders(&mut self, list: HWND) {
        let count = SendMessageW(list, LB_GETSELCOUNT, 0, 0).max(0) as usize;
        let mut items = vec![0i32; count];
        SendMessageW(list, LB_GETSELITEMS, count, items.as_mut_ptr() as isize);
        for &item in items.iter().rev() {
            SendMessageW(list, LB_DELETESTRING, item as usize, 0);
        }
        self.folder_extent(list);
    }
    unsafe fn resume_hotkey(&mut self) {
        if self.hotkey_paused {
            self.hotkey_paused = false;
            if let Some(shell) = &mut self.shell {
                if let Err(error) = shell.set_hotkey(&self.settings.hotkey) {
                    self.notice = error.clone();
                    if !self.settings_window.is_null() {
                        SetWindowTextW(self.settings_error, wide(&error).as_ptr());
                    }
                }
            }
        }
    }
    unsafe fn apply_settings(&mut self) {
        let hotkey = control_text(self.hotkey_edit).trim().to_owned();
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in self.folder_paths(self.roots_edit) {
            if !path.is_absolute() || !path.is_dir() {
                SetWindowTextW(
                    self.settings_error,
                    wide(&format!("폴더를 확인하세요: {}", path.display())).as_ptr(),
                );
                return;
            }
            if !roots.iter().any(|old| normalized(old) == normalized(&path)) {
                roots.push(path);
            }
        }
        if roots.is_empty() {
            SetWindowTextW(
                self.settings_error,
                wide("검색할 폴더를 하나 이상 추가하세요.").as_ptr(),
            );
            return;
        }
        let excluded_folders = self.folder_paths(self.excluded_edit);
        if excluded_folders.len() > 256 || excluded_folders.iter().any(|path| !path.is_absolute()) {
            SetWindowTextW(
                self.settings_error,
                wide("제외 폴더는 절대 경로로 최대 256개까지 설정할 수 있습니다.").as_ptr(),
            );
            return;
        }
        let scope_changed = roots != self.settings.roots
            || crate::exclusions::FolderExclusions::new(&excluded_folders).paths()
                != crate::exclusions::FolderExclusions::new(&self.settings.excluded_folders)
                    .paths();
        if (self.indexing || self.updating) && scope_changed {
            SetWindowTextW(
                self.settings_error,
                wide("목록 갱신이 끝난 후 검색 폴더나 제외 폴더를 변경하세요.").as_ptr(),
            );
            return;
        }
        let font = match control_text(self.font_edit).parse::<f32>() {
            Ok(v) if (8.0..=20.0).contains(&v) => v,
            _ => {
                SetWindowTextW(
                    self.settings_error,
                    wide("글자 크기는 8~20 사이 숫자입니다.").as_ptr(),
                );
                return;
            }
        };
        if let Some(shell) = &mut self.shell {
            if let Err(error) = shell.set_hotkey(&hotkey) {
                SetWindowTextW(self.settings_error, wide(&error).as_ptr());
                return;
            }
        } else {
            match Shell::new_native(self.hwnd as isize, SHELL_EVENT, &hotkey) {
                Ok(shell) => self.shell = Some(shell),
                Err(error) => {
                    SetWindowTextW(self.settings_error, wide(&error).as_ptr());
                    return;
                }
            }
        }
        self.settings.roots = roots;
        self.settings.excluded_folders = excluded_folders;
        self.settings.hotkey = hotkey;
        self.settings.font_size = font;
        self.settings.fast_index = SendMessageW(self.auto_fast, BM_GETCHECK, 0, 0) != 0;
        self.settings.check_updates_on_startup =
            SendMessageW(self.auto_release, BM_GETCHECK, 0, 0) != 0;
        self.remember_geometry();
        if let Err(error) = self.settings.save() {
            SetWindowTextW(self.settings_error, wide(&error).as_ptr());
            return;
        }
        DestroyWindow(self.settings_window);
        self.install_font();
        self.layout();
        if scope_changed {
            self.pending = WatchInbox::default();
            self.configure_watcher();
            self.start_scan(true, false);
        }
        self.update_status();
    }
}

// The header keeps its native resize/click behavior. Its text is drawn here
// because Windows' dark header theme still inherits a black caption color.
unsafe extern "system" fn hotkey_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let app = &mut *(data as *mut App);
    match message {
        WM_GETDLGCODE => return (DLGC_WANTALLKEYS | DLGC_WANTCHARS) as isize,
        WM_SETFOCUS => {
            if let Some(shell) = &app.shell {
                match shell.pause_hotkey() {
                    Ok(()) => app.hotkey_paused = true,
                    Err(error) => {
                        SetWindowTextW(app.settings_error, wide(&error).as_ptr());
                    }
                }
            }
            SendMessageW(hwnd, EM_SETSEL, 0, -1);
        }
        WM_KILLFOCUS => app.resume_hotkey(),
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if w == VK_TAB as usize {
                SetFocus(GetNextDlgTabItem(
                    app.settings_window,
                    hwnd,
                    (GetKeyState(VK_SHIFT as i32) < 0) as i32,
                ));
            } else if w == VK_ESCAPE as usize {
                SetWindowTextW(hwnd, wide(&app.settings.hotkey).as_ptr());
                SetFocus(GetDlgItem(app.settings_window, SAVE_SETTINGS as i32));
            } else if let Some(text) = platform::captured_hotkey(
                w as u32,
                GetKeyState(VK_CONTROL as i32) < 0,
                GetKeyState(VK_MENU as i32) < 0,
                GetKeyState(VK_SHIFT as i32) < 0,
                GetKeyState(VK_LWIN as i32) < 0 || GetKeyState(VK_RWIN as i32) < 0,
            ) {
                SetWindowTextW(hwnd, wide(&text).as_ptr());
                SendMessageW(hwnd, EM_SETSEL, 0, -1);
                SetWindowTextW(
                    app.settings_error,
                    wide("입력한 단축키는 저장하면 적용됩니다.").as_ptr(),
                );
            }
            return 0;
        }
        WM_CHAR | WM_SYSCHAR => return 0,
        WM_NCDESTROY => {
            app.resume_hotkey();
            RemoveWindowSubclass(hwnd, Some(hotkey_subclass), id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, message, w, l)
}

unsafe extern "system" fn query_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(query_subclass), id);
        return DefSubclassProc(hwnd, message, w, l);
    }
    let result = DefSubclassProc(hwnd, message, w, l);
    // Do this after the native edit control has placed its caret. Selecting on
    // focus alone is undone by the rest of its mouse-click handling.
    if matches!(message, WM_SETFOCUS | WM_LBUTTONUP) {
        SendMessageW(hwnd, EM_SETSEL, 0, -1);
    }
    result
}

unsafe extern "system" fn list_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        let app = &*(data as *const App);
        if let Some(accessibility) = &app.accessibility {
            accessibility.disconnect();
        }
        RemoveWindowSubclass(hwnd, Some(list_subclass), id);
        return DefSubclassProc(hwnd, message, w, l);
    }
    let app = &*(data as *const App);
    if message == WM_GETOBJECT {
        if let Some(result) = app.accessibility.as_ref().and_then(|a| a.get_object(w, l)) {
            return result;
        }
    }
    if message == crate::accessibility::SNAPSHOT && l != 0 {
        let output = &mut *(l as *mut Option<crate::accessibility::View>);
        if let Some(engine) = &app.engine {
            let top = SendMessageW(hwnd, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
            let mut rect: RECT = zeroed();
            GetWindowRect(hwnd, &mut rect);
            let mut row: RECT = zeroed();
            row.left = LVIR_BOUNDS as i32;
            let valid = SendMessageW(hwnd, LVM_GETITEMRECT, top, &mut row as *mut _ as isize) != 0;
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(hwnd, &mut origin);
            let focused = SendMessageW(hwnd, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
            let mut horizontal: SCROLLINFO = zeroed();
            horizontal.cbSize = size_of::<SCROLLINFO>() as u32;
            horizontal.fMask = SIF_POS | SIF_RANGE | SIF_PAGE;
            GetScrollInfo(hwnd, SB_HORZ, &mut horizontal);
            *output = Some(crate::accessibility::View {
                engine: engine.clone(),
                rows: app.rows.clone(),
                epoch: app.accessibility_epoch,
                top,
                visible: (SendMessageW(hwnd, LVM_GETCOUNTPERPAGE, 0, 0).max(1) as usize + 1)
                    .min(256),
                rect,
                row_height: if valid {
                    (row.bottom - row.top).max(1)
                } else {
                    22
                },
                first_row_y: origin.y + if valid { row.top } else { 24 },
                focused: usize::try_from(focused).ok(),
                has_focus: GetFocus() == hwnd,
                horizontal: (
                    horizontal.nPos,
                    horizontal.nMax - horizontal.nMin + 1,
                    horizontal.nPage,
                ),
            });
        }
        return 1;
    }
    if message == crate::accessibility::ACTION && l != 0 {
        if app.rows_current() && !app.list_frozen {
            crate::accessibility::perform(
                hwnd,
                &mut *(l as *mut crate::accessibility::Action),
                app.accessibility_epoch,
                app.rows.len(),
            );
        }
        return 1;
    }
    if message == crate::accessibility::SELECTION && l != 0 {
        let selection = &mut *(l as *mut crate::accessibility::Selection);
        // Large selections are announced as invalidated; do not instantiate
        // millions of automation objects merely to read the selection property.
        if selection.epoch == app.accessibility_epoch
            && SendMessageW(hwnd, LVM_GETSELECTEDCOUNT, 0, 0) <= 4096
        {
            selection.rows = app.selected_items();
            selection.valid = true;
        }
        return 1;
    }
    if app.list_frozen
        && (matches!(
            message,
            WM_VSCROLL | WM_HSCROLL | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_CHAR | WM_UNICHAR
        ) || (message == WM_KEYDOWN
            && matches!(
                w as u16,
                VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_PRIOR | VK_NEXT
            )))
    {
        return 0;
    }
    if message == WM_NOTIFY && l != 0 {
        let notice = &*(l as *const NMHDR);
        if notice.hwndFrom == SendMessageW(hwnd, LVM_GETHEADER, 0, 0) as HWND {
            let app = &*(data as *const App);
            if notice.code == HDN_ENDTRACKW {
                PostMessageW(app.hwnd, HEADER_CHANGED, 0, 0);
            }
            if notice.code == NM_CUSTOMDRAW {
                let draw = &*(l as *const NMCUSTOMDRAW);
                if draw.dwDrawStage == CDDS_PREPAINT {
                    return CDRF_NOTIFYITEMDRAW as isize;
                }
                if draw.dwDrawStage == CDDS_ITEMPREPAINT {
                    FillRect(draw.hdc, &draw.rc, app.input_background);
                    let edge = CreateSolidBrush(0x00463c34);
                    FrameRect(draw.hdc, &draw.rc, edge);
                    DeleteObject(edge);
                    SetTextColor(draw.hdc, FG);
                    SetBkMode(draw.hdc, TRANSPARENT as i32);
                    SelectObject(draw.hdc, app.font);
                    let text = wide(
                        ["이름", "폴더 경로", "크기", "생성일", "수정일"]
                            .get(draw.dwItemSpec)
                            .copied()
                            .unwrap_or(""),
                    );
                    let mut rect = draw.rc;
                    rect.left += 7;
                    rect.right -= 8;
                    let sorted = match app.settings.sort {
                        SortField::Name => 0,
                        SortField::Path => 1,
                        SortField::Size => 2,
                        SortField::Created => 3,
                        SortField::Modified => 4,
                    };
                    if sorted == draw.dwItemSpec {
                        let mut arrow = rect;
                        arrow.left = arrow.right - 16;
                        rect.right -= 20;
                        let text = wide(if app.settings.descending {
                            "▼"
                        } else {
                            "▲"
                        });
                        DrawTextW(
                            draw.hdc,
                            text.as_ptr(),
                            -1,
                            &mut arrow,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );
                    }
                    DrawTextW(
                        draw.hdc,
                        text.as_ptr(),
                        -1,
                        &mut rect,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                    return CDRF_SKIPDEFAULT as isize;
                }
            }
        }
    }
    let result = DefSubclassProc(hwnd, message, w, l);
    if matches!(
        message,
        WM_VSCROLL
            | WM_HSCROLL
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
            | WM_KEYUP
            | WM_SETFOCUS
            | LVM_SCROLL
            | LVM_ENSUREVISIBLE
    ) {
        PostMessageW(app.hwnd, ACCESSIBILITY_CHANGED, 0, 0);
    }
    result
}

unsafe extern "system" fn combo_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(combo_subclass), id);
        return DefSubclassProc(hwnd, message, w, l);
    }
    let result = DefSubclassProc(hwnd, message, w, l);
    if matches!(message, WM_PAINT | WM_PRINTCLIENT) {
        let app = &*(data as *const App);
        let hdc = if message == WM_PRINTCLIENT {
            w as HDC
        } else {
            GetDC(hwnd)
        };
        if !hdc.is_null() {
            let mut info: COMBOBOXINFO = zeroed();
            info.cbSize = size_of::<COMBOBOXINFO>() as u32;
            if GetComboBoxInfo(hwnd, &mut info) != 0 {
                FillRect(hdc, &info.rcButton, app.input_background);
                SetTextColor(hdc, FG);
                SetBkMode(hdc, TRANSPARENT as i32);
                SelectObject(hdc, app.font);
                let text = wide("▾");
                let mut rect = info.rcButton;
                DrawTextW(
                    hdc,
                    text.as_ptr(),
                    -1,
                    &mut rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                let mut bounds: RECT = zeroed();
                GetClientRect(hwnd, &mut bounds);
                let edge = CreateSolidBrush(0x00463c34);
                FrameRect(hdc, &bounds, edge);
                DeleteObject(edge);
            }
            if message != WM_PRINTCLIENT {
                ReleaseDC(hwnd, hdc);
            }
        }
    }
    result
}

unsafe extern "system" fn settings_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(l as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, message, w, l);
    }
    let app = &mut *ptr;
    match message {
        WM_CREATE => {
            app.settings_window = hwnd;
            app.settings_controls(hwnd);
            0
        }
        WM_COMMAND if w & 0xffff == SAVE_SETTINGS => {
            app.apply_settings();
            0
        }
        WM_COMMAND if w & 0xffff == CHECK_RELEASE => {
            app.check_release(true);
            0
        }
        WM_COMMAND if w & 0xffff == ADD_ROOT => {
            app.add_folders(app.roots_edit);
            0
        }
        WM_COMMAND if w & 0xffff == REMOVE_ROOT => {
            app.remove_folders(app.roots_edit);
            0
        }
        WM_COMMAND if w & 0xffff == ADD_EXCLUDED => {
            app.add_folders(app.excluded_edit);
            0
        }
        WM_COMMAND if w & 0xffff == REMOVE_EXCLUDED => {
            app.remove_folders(app.excluded_edit);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            app.resume_hotkey();
            app.settings_window = null_mut();
            SetFocus(app.query);
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if l as HWND != app.hotkey_edit => {
            SetTextColor(w as HDC, FG);
            SetBkColor(w as HDC, BG);
            app.background as isize
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC => {
            SetTextColor(w as HDC, FG);
            SetBkColor(w as HDC, INPUT_BG);
            app.input_background as isize
        }
        WM_DRAWITEM => {
            app.draw_button(l);
            1
        }
        _ => DefWindowProcW(hwnd, message, w, l),
    }
}
unsafe fn control_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut text = vec![0u16; len + 1];
    let read = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&text[..read])
}
unsafe fn write_text(buffer: *mut u16, capacity: i32, text: &str) {
    if buffer.is_null() || capacity <= 0 {
        return;
    }
    let mut written = 0;
    for ch in text
        .encode_utf16()
        .take(capacity.saturating_sub(1) as usize)
    {
        *buffer.add(written) = ch;
        written += 1;
    }
    *buffer.add(written) = 0;
}
pub(crate) fn format_size(value: Option<u64>) -> String {
    let Some(size) = value else {
        return "—".into();
    };
    if size < 1024 {
        return format!("{size} B");
    }
    let mut scaled = size as f64;
    let mut unit = "KB";
    for candidate in ["KB", "MB", "GB", "TB"] {
        scaled /= 1024.0;
        unit = candidate;
        if scaled < 1024.0 {
            break;
        }
    }
    format!("{scaled:.1} {unit}")
}
pub(crate) fn format_date(value: Option<i64>) -> String {
    value
        .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
        .map(|date| {
            date.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod watcher_tests {
    use super::*;

    #[test]
    fn startup_replay_ready_keeps_old_rows_live_until_replacement_search_finishes() {
        fn window() -> HWND {
            let mut previous = null_mut();
            loop {
                let hwnd =
                    unsafe { FindWindowExW(null_mut(), previous, wide(CLASS).as_ptr(), null()) };
                if hwnd.is_null() {
                    return hwnd;
                }
                let mut pid = 0;
                unsafe {
                    GetWindowThreadProcessId(hwnd, &mut pid);
                }
                if pid == std::process::id() {
                    return hwnd;
                }
                previous = hwnd;
            }
        }
        fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> usize {
            let mut result = 0;
            assert_ne!(
                unsafe {
                    SendMessageTimeoutW(
                        hwnd,
                        msg,
                        w,
                        l,
                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                        1000,
                        &mut result,
                    )
                },
                0
            );
            result
        }
        fn until(mut condition: impl FnMut() -> bool) {
            let start = Instant::now();
            while !condition() {
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn name(hwnd: HWND, list: HWND, row: usize) -> String {
            let mut buffer = [0u16; 128];
            let mut display: NMLVDISPINFOW = unsafe { zeroed() };
            display.hdr.hwndFrom = list;
            display.hdr.code = LVN_GETDISPINFOW;
            display.item.mask = LVIF_TEXT;
            display.item.iItem = row as i32;
            display.item.pszText = buffer.as_mut_ptr();
            display.item.cchTextMax = buffer.len() as i32;
            send(hwnd, WM_NOTIFY, RESULTS, &mut display as *mut _ as isize);
            String::from_utf16_lossy(
                &buffer[..buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len())],
            )
        }
        type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;
        struct Cleanup {
            path: PathBuf,
            environment: Option<std::ffi::OsString>,
            ui: Option<std::thread::JoinHandle<Result<(), String>>>,
            gate: Gate,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                *self.gate.0.lock().unwrap() = true;
                self.gate.1.notify_all();
                let hwnd = window();
                if !hwnd.is_null() {
                    unsafe {
                        PostMessageW(hwnd, WM_APP + 4, 0, 0);
                    }
                }
                if let Some(ui) = self.ui.take() {
                    let _ = ui.join();
                }
                unsafe {
                    if let Some(old) = &self.environment {
                        std::env::set_var("QUICKFIND_DATA_DIR", old);
                    } else {
                        std::env::remove_var("QUICKFIND_DATA_DIR");
                    }
                }
                if let Ok(path) = self.path.canonicalize() {
                    assert!(path.starts_with(std::env::temp_dir().canonicalize().unwrap()));
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
        let path =
            std::env::temp_dir().join(format!("hyperfinder-startup-ready-{}", std::process::id()));
        let root = path.join("files");
        let data = path.join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let mut index = Index::from_parts(
            (0..512)
                .map(|n| crate::model::Entry {
                    name: if n == 400 {
                        "old-marker.txt".into()
                    } else {
                        format!("row-{n}.txt")
                    },
                    parent: 0,
                    size: Some(1),
                    created: Some(1),
                    modified: Some(n),
                    is_dir: false,
                })
                .collect(),
            vec![root.to_string_lossy().into_owned()],
        );
        index.roots = vec![root.to_string_lossy().into_owned()];
        cache::save(&data.join("index.bin"), &index).unwrap();
        let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let mut cleanup = Cleanup {
            path,
            environment: std::env::var_os("QUICKFIND_DATA_DIR"),
            ui: None,
            gate: gate.clone(),
        };
        unsafe {
            std::env::set_var("QUICKFIND_DATA_DIR", &data);
        }
        let ui_root = root.clone();
        cleanup.ui = Some(std::thread::spawn(move || {
            run(Settings {
                roots: vec![ui_root],
                fast_index: false,
                hotkey: "Ctrl+Alt+Shift+F20".into(),
                ..Default::default()
            })
        }));
        until(|| {
            let w = window();
            let list = unsafe { GetDlgItem(w, RESULTS as i32) };
            !list.is_null() && send(list, LVM_GETITEMCOUNT, 0, 0) == 512
        });
        let hwnd = window();
        let list = unsafe { GetDlgItem(hwnd, RESULTS as i32) };
        let query = unsafe { GetDlgItem(hwnd, QUERY as i32) };
        let workers = background_work(rayon::current_num_threads);
        let (blocked, ready) = mpsc::channel();
        let blocker = std::thread::spawn(move || {
            background_work(|| {
                rayon::scope(|scope| {
                    for _ in 0..workers {
                        let gate = gate.clone();
                        let blocked = blocked.clone();
                        scope.spawn(move |_| {
                            blocked.send(()).unwrap();
                            let _guard = gate
                                .1
                                .wait_timeout_while(
                                    gate.0.lock().unwrap(),
                                    Duration::from_secs(10),
                                    |open| !*open,
                                )
                                .unwrap();
                        });
                    }
                })
            })
        });
        for _ in 0..workers {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        index.entries.retain(|e| e.name != "old-marker.txt");
        cache::save(&data.join("index.bin"), &index).unwrap(); // Simulate the helper's committed result.
        let committed_at = std::fs::metadata(data.join("index.bin"))
            .unwrap()
            .modified()
            .unwrap();
        let replacement = Arc::new(SearchEngine::new(Arc::new(index)));
        send(
            hwnd,
            WM_APP + 60,
            0,
            Box::into_raw(Box::new(replacement)) as isize,
        );
        assert_eq!(send(list, LVM_GETITEMCOUNT, 0, 0), 512);
        assert!(
            !name(hwnd, list, 400).is_empty(),
            "startup replay froze rows outside the cached viewport"
        );
        send(query, WM_SETTEXT, 0, wide("old-marker").as_ptr() as isize);
        until(|| {
            send(list, LVM_GETITEMCOUNT, 0, 0) == 1 && name(hwnd, list, 0) == "old-marker.txt"
        });
        let selected = LVITEMW {
            stateMask: LVIS_SELECTED,
            state: LVIS_SELECTED,
            ..unsafe { zeroed() }
        };
        send(list, LVM_SETITEMSTATE, 0, &selected as *const _ as isize);
        for indexing in [false, true] {
            let mut captured: Result<Vec<crate::file_clipboard::DragFile>, String> =
                Err("not delivered".to_owned());
            send(
                hwnd,
                WM_APP + 64,
                usize::from(indexing),
                &mut captured as *mut Result<Vec<crate::file_clipboard::DragFile>, String> as isize,
            );
            let captured = captured.unwrap_or_else(|e| panic!("background work blocked drag: {e}"));
            assert_eq!(captured.len(), 1);
            assert_eq!(Path::new(&captured[0].path), root.join("old-marker.txt"));
            assert!(!captured[0].is_dir);
        }
        // Model the Shell's modal drag loop without moving any user files.
        // A prepared replacement must not change the source row mid-drag.
        send(hwnd, WM_APP + 65, 1, 0);
        let mut blocked: Result<Vec<crate::file_clipboard::DragFile>, String> = Ok(Vec::new());
        send(hwnd, WM_APP + 64, 0, &mut blocked as *mut _ as isize);
        assert!(
            blocked.is_err(),
            "a second file operation must still be blocked"
        );
        *cleanup.gate.0.lock().unwrap() = true;
        cleanup.gate.1.notify_all();
        blocker.join().unwrap();
        until(|| send(hwnd, WM_APP + 65, 2, 0) != 0);
        assert_eq!(send(list, LVM_GETITEMCOUNT, 0, 0), 1);
        assert_eq!(name(hwnd, list, 0), "old-marker.txt");
        send(hwnd, WM_APP + 65, 0, 0);
        until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
        unsafe {
            PostMessageW(hwnd, WM_APP + 4, 0, 0);
        }
        cleanup.ui.take().unwrap().join().unwrap().unwrap();
        let saved = cache::load(&data.join("index.bin"), &[root]).unwrap();
        assert_eq!(saved.len(), 511);
        assert!(!saved.entries.iter().any(|e| e.name == "old-marker.txt"));
        assert_eq!(
            std::fs::metadata(data.join("index.bin"))
                .unwrap()
                .modified()
                .unwrap(),
            committed_at,
            "the GUI rewrote the cache already committed by the startup helper"
        );
    }

    #[test]
    fn hidden_window_dispatches_restore_and_worker_messages_without_dialog_navigation() {
        use std::sync::atomic::AtomicUsize;
        unsafe extern "system" fn procedure(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            unsafe {
                if m == WM_NCCREATE {
                    let create = &*(l as *const CREATESTRUCTW);
                    SetWindowLongPtrW(h, GWLP_USERDATA, create.lpCreateParams as isize);
                }
                if matches!(m, WORK | SHELL_EVENT | WATCH_WORK) || m == WM_APP + 3 {
                    let counter = GetWindowLongPtrW(h, GWLP_USERDATA) as *const AtomicUsize;
                    (*counter).fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
                DefWindowProcW(h, m, w, l)
            }
        }
        unsafe {
            let class = wide("QuickFind.HiddenMessageRegression");
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let handled = AtomicUsize::new(0);
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                &handled as *const AtomicUsize as _,
            );
            assert!(!window.is_null());
            assert_eq!(IsWindowVisible(window), 0);
            // No window is displayed and no keyboard or mouse input is sent.
            for message in [WM_APP + 3, WORK, SHELL_EVENT, WATCH_WORK] {
                assert_ne!(PostMessageW(window, message, 0, 0), 0);
                let mut msg: MSG = zeroed();
                assert_ne!(
                    PeekMessageW(&mut msg, window, message, message, PM_REMOVE),
                    0
                );
                assert!(!dialog_key_message(window, &msg));
                DispatchMessageW(&msg);
            }
            assert_eq!(handled.load(Ordering::Relaxed), 4);
            let hidden_key = MSG {
                hwnd: window,
                message: WM_KEYDOWN,
                wParam: VK_TAB as usize,
                ..zeroed()
            };
            assert!(!dialog_key_message(window, &hidden_key));
            DestroyWindow(window);
            UnregisterClassW(class.as_ptr(), instance);
        }
    }
    fn modified(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(path)
    }
    #[test]
    fn notification_storm_has_one_path_and_one_wakeup() {
        let mut batch = WatchBatch::default();
        let mut wakes = 0;
        for _ in 0..100_000 {
            wakes += batch.add(modified(PathBuf::from(r"C:\data\busy.log"))) as usize;
        }
        assert_eq!(wakes, 1);
        assert_eq!(batch.paths.len(), 1);
        assert!(!batch.overflow);
        assert!(batch.bytes < MAX_WATCH_BYTES);
    }
    #[test]
    fn recovery_pool_limits_parallelism_and_runs_below_foreground_priority() {
        let (threads, priority) = background_work(|| {
            (rayon::current_num_threads(), unsafe {
                windows_sys::Win32::System::Threading::GetThreadPriority(GetCurrentThread())
            })
        });
        assert!((1..=2).contains(&threads));
        assert!(priority < 0, "recovery worker priority was {priority}");
    }
    #[test]
    fn watcher_io_paths_preserve_original_unicode_and_spelling() {
        let inbox = WatchInbox::default();
        let original = PathBuf::from("C:\\자료\\İ름\\Report.txt");
        inbox.0.lock().unwrap().add(modified(original.clone()));
        assert_eq!(inbox.take().unwrap()[0].paths, [original.clone()]);
        {
            let mut batch = inbox.0.lock().unwrap();
            batch.roots = vec![PathBuf::from("C:\\자료\\İ름")];
            for n in 0..6000 {
                batch.add(modified(original.with_file_name(format!("보고서-{n}.txt"))));
            }
            assert!(batch.compacted && !batch.overflow);
        }
        assert_eq!(
            inbox.take().unwrap()[0].paths,
            [PathBuf::from("C:\\자료\\İ름")]
        );
    }
    #[test]
    fn distinct_notification_storm_stops_at_a_fixed_memory_bound() {
        let mut batch = WatchBatch::default();
        let mut wakes = 0;
        for n in 0..100_000 {
            wakes += batch.add(modified(PathBuf::from(format!(r"C:\data\{n}.tmp")))) as usize;
        }
        assert_eq!(wakes, 1);
        assert!(!batch.overflow);
        assert!(batch.compacted);
        assert_eq!(batch.paths.len(), 1);
        assert_eq!(batch.paths.get(Path::new(r"c:\data")), Some(&true));
        assert!(batch.bytes < MAX_WATCH_BYTES);
    }
    #[test]
    fn scoped_storm_preserves_siblings_and_never_widens_above_search_roots() {
        let mut batch = WatchBatch {
            roots: vec![r"c:\work".into(), r"d:\docs".into()],
            ..Default::default()
        };
        for n in 0..12_000 {
            batch.add(modified(PathBuf::from(format!(r"C:\work\build\{n}.tmp"))));
        }
        batch.add(modified(r"D:\docs\report.txt".into()));
        assert_eq!(batch.paths.len(), 2);
        assert_eq!(batch.paths.get(Path::new(r"c:\work\build")), Some(&true));
        assert_eq!(batch.paths.get(Path::new(r"d:\docs")), Some(&true));
        batch.add(modified(r"C:\work".into()));
        assert_eq!(batch.paths.get(Path::new(r"c:\work")), Some(&true));
        assert!(!batch.paths.contains_key(Path::new(r"c:\")));
        assert!(!batch.overflow);
    }
    #[test]
    fn a_new_build_directory_does_not_promote_its_storm_to_the_search_root() {
        let mut batch = WatchBatch {
            roots: vec![r"c:\work".into()],
            ..Default::default()
        };
        batch.add(
            Event::new(EventKind::Create(notify::event::CreateKind::Any))
                .add_path(r"C:\work\build".into()),
        );
        for n in 0..8000 {
            batch.add(
                Event::new(EventKind::Create(notify::event::CreateKind::Any))
                    .add_path(format!(r"C:\work\build\{n}.tmp").into()),
            );
        }
        assert!(batch.compacted && !batch.overflow);
        assert_eq!(batch.paths.len(), 1);
        assert_eq!(batch.paths.get(Path::new(r"c:\work\build")), Some(&true));
        batch.add(modified(r"C:\work\build".into()));
        assert_eq!(batch.paths.len(), 1);
        assert_eq!(batch.paths.get(Path::new(r"c:\work\build")), Some(&true));
        assert!(!batch.paths.contains_key(Path::new(r"c:\work")));
    }
    #[test]
    fn widely_distributed_storm_is_bounded_without_losing_paths() {
        let mut batch = WatchBatch {
            roots: vec![r"c:\work".into()],
            ..Default::default()
        };
        for n in 0..12_000 {
            batch.add(modified(PathBuf::from(format!(
                r"C:\work\project-{n}\out\a.tmp"
            ))));
        }
        assert!(batch.compacted && !batch.overflow);
        assert!(batch.paths.len() <= MAX_DIRTY_FOLDERS);
        for n in 0..12_000 {
            let path = PathBuf::from(format!(r"c:\work\project-{n}\out\a.tmp"));
            assert!(path.ancestors().any(|p| batch.paths.get(p) == Some(&true)));
        }
    }
    #[test]
    fn full_recovery_cooldown_does_not_delay_partial_followups() {
        let inbox = WatchInbox::default();
        inbox.recovery_finished();
        inbox
            .0
            .lock()
            .unwrap()
            .add(modified(r"c:\data\later.txt".into()));
        assert!(inbox.delay_ms(true) <= 900);
        assert!(inbox.take().is_ok());
        inbox.0.lock().unwrap().add(Event::new(EventKind::Any));
        assert!(inbox.delay_ms(true) > 29_000);
        let (_, _, reconnect, full) = inbox.take_work();
        assert!(reconnect && full);
        inbox.0.lock().unwrap().add(Event::new(EventKind::Any));
        assert!(inbox.delay_ms(false) > 29_000);
    }
    #[test]
    fn overflow_is_consumed_and_new_changes_are_collected_during_recovery() {
        let inbox = WatchInbox::default();
        inbox.0.lock().unwrap().add(Event::new(EventKind::Any));
        assert!(inbox.take().is_err());
        assert!(inbox.is_empty());
        assert!(
            inbox
                .0
                .lock()
                .unwrap()
                .add(modified(PathBuf::from(r"C:\data\new.txt")))
        );
        let events = inbox.take().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].paths, [PathBuf::from(r"C:\data\new.txt")]);
        assert!(inbox.is_empty());
        inbox.retry_rescan();
        assert!(inbox.delay_ms(false) > 29_000);
        assert!(
            !inbox
                .0
                .lock()
                .unwrap()
                .add(modified(PathBuf::from(r"C:\data\later.txt")))
        );
        assert!(inbox.delay_ms(true) > 29_000);
        assert!(inbox.take().is_err());
        assert!(inbox.is_empty());
    }

    #[test]
    fn previous_snapshot_remains_searchable_during_recovery_and_after_failure() {
        let root =
            std::env::temp_dir().join(format!("hyperfinder-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("keep.txt"), b"keep").unwrap();
        let previous = Arc::new(SearchEngine::new(Arc::new(
            indexer::scan(&[root.clone()], &AtomicBool::new(false), |_| {}).unwrap(),
        )));
        std::fs::write(root.join("new.txt"), b"new").unwrap();
        let (started, wait_started) = mpsc::channel();
        let (resume, wait_resume) = mpsc::channel();
        let snapshot = previous.clone();
        let worker = std::thread::spawn(move || {
            let once = AtomicBool::new(false);
            refresh_snapshot(
                snapshot,
                Err("overflow".into()),
                &AtomicBool::new(false),
                |_| {
                    if !once.swap(true, Ordering::Relaxed) {
                        started.send(()).unwrap();
                        wait_resume.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                },
            )
        });
        wait_started.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            previous
                .search("keep", &SearchOptions::default())
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            previous
                .search("new", &SearchOptions::default())
                .unwrap()
                .rows
                .len(),
            0
        );
        resume.send(()).unwrap();
        let (current, changed) = worker.join().unwrap().unwrap();
        assert!(changed);
        assert_eq!(
            current
                .search("new", &SearchOptions::default())
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(previous.index().len(), 1);
        assert!(
            refresh_snapshot(
                previous.clone(),
                Err("overflow".into()),
                &AtomicBool::new(true),
                |_| {}
            )
            .is_err()
        );
        assert_eq!(
            previous
                .search("keep", &SearchOptions::default())
                .unwrap()
                .rows
                .len(),
            1
        );
        let path = root.canonicalize().unwrap();
        assert!(path.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn coalescing_preserves_recursive_rename_refresh() {
        let path = PathBuf::from(r"c:\data\folder");
        let mut batch = WatchBatch::default();
        batch.add(modified(path.clone()));
        batch.add(
            Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )))
            .add_path(path.clone()),
        );
        batch.add(modified(path.clone()));
        assert_eq!(batch.paths.get(&path), Some(&true));
    }
    #[test]
    fn background_coalescing_has_a_fixed_deadline() {
        let inbox = WatchInbox::default();
        {
            let mut state = inbox.0.lock().unwrap();
            state.started = Some(Instant::now());
        }
        assert!(inbox.delay_ms(false) > inbox.delay_ms(true));
        {
            let mut state = inbox.0.lock().unwrap();
            state.started = Some(Instant::now() - Duration::from_secs(20));
        }
        assert_eq!(inbox.delay_ms(false), 1);
        assert_eq!(inbox.delay_ms(true), 1);
    }
    fn restored_view_fixture() -> (SearchEngine, SavedView) {
        let entries = [("report.txt", 1), ("new.txt", 0), ("report.txt", 0)]
            .into_iter()
            .map(|(name, parent)| crate::model::Entry {
                name: name.into(),
                parent,
                size: Some(1),
                created: Some(1),
                modified: Some(2),
                is_dir: false,
            })
            .collect();
        let engine = SearchEngine::new(Arc::new(Index {
            entries,
            folders: vec![r"C:\A".into(), r"C:\B".into()].into(),
            ..Default::default()
        }));
        let mut settings = Settings::default();
        settings.query = "report".into();
        let view = SavedView {
            top: 4,
            row_height: 22,
            selected: Some(RowTarget {
                name: "report.txt".into(),
                parent: r"C:\A".into(),
            }),
            selection: vec![selection_identity(r"C:\A", "report.txt")].into(),
            selection_parents: vec![r"C:\A".to_owned()].into(),
            selection_mark: Some(RowTarget {
                parent: r"C:\A".into(),
                name: "report.txt".into(),
            }),
            anchor: Some(RowTarget {
                name: "report.txt".into(),
                parent: r"C:\B".into(),
            }),
            settings,
        };
        (engine, view)
    }
    #[test]
    fn view_identity_survives_row_id_remap_and_duplicate_filenames() {
        let (engine, view) = restored_view_fixture();
        let new_rows = [1, 2, 0];
        let restored = view.locate(&engine, &new_rows);
        assert_eq!(restored.selected, Some(1));
        assert_eq!(restored.selection, vec![1]);
        assert_eq!(restored.selection_mark, Some(1));
        assert_eq!(restored.top, 2);
        assert_eq!(
            engine
                .index()
                .full_path(new_rows[restored.selected.unwrap()] as usize),
            r"C:\A\report.txt"
        );
        assert_eq!(
            engine.index().full_path(new_rows[restored.top] as usize),
            r"C:\B\report.txt"
        );
    }
    #[test]
    fn multiple_selection_keeps_matching_identities_and_drops_deleted_files() {
        let (engine, mut view) = restored_view_fixture();
        let mut selection = vec![
            selection_identity(r"C:\A", "report.txt"),
            selection_identity(r"C:\B", "report.txt"),
            selection_identity(r"C:\A", "deleted.txt"),
        ];
        selection.sort_unstable();
        view.selection = selection.into();
        view.selection_parents = vec![r"C:\A".into(), r"C:\B".into()].into();
        assert_eq!(view.locate(&engine, &[1, 2, 0]).selection, vec![1, 2]);
    }
    #[test]
    fn query_or_sort_changes_reset_saved_view() {
        let (_, view) = restored_view_fixture();
        assert!(view.matches(&view.settings));
        let mut edited = view.settings.clone();
        edited.query = "new query".into();
        assert!(!view.matches(&edited));
        let mut edited = view.settings.clone();
        edited.sort = SortField::Name;
        assert!(!view.matches(&edited));
        let mut edited = view.settings.clone();
        edited.descending = !edited.descending;
        assert!(!view.matches(&edited));
    }
    #[test]
    fn first_page_remains_at_top_when_newer_files_arrive() {
        let (engine, mut view) = restored_view_fixture();
        view.top = 0;
        view.anchor = None;
        let restored = view.locate(&engine, &[1, 2, 0]);
        assert_eq!(restored.top, 0);
    }
    #[test]
    fn frozen_copy_paths_are_bounded_and_keep_original_identity() {
        let row = |path: String| CachedRow {
            path,
            ..Default::default()
        };
        let mut copies = FrozenCopies::default();
        assert!(copies.insert(0, row(r"C:\old\report.txt".into())));
        assert!(copies.insert(0, row(r"C:\different\report.txt".into())));
        assert_eq!(copies.paths[&0].path, r"C:\old\report.txt");
        for item in 1..10_000 {
            copies.insert(item, row(format!(r"C:\files\{item}.txt")));
        }
        assert_eq!(copies.paths.len(), MAX_FROZEN_COPY_PATHS);
        assert!(copies.bytes <= MAX_FROZEN_COPY_BYTES);
        let mut copies = FrozenCopies::default();
        assert!(!copies.insert(0, row("x".repeat(MAX_FROZEN_COPY_BYTES + 1))));
        for item in 0..1000 {
            copies.insert(item, row("x".repeat(16 * 1024)));
        }
        assert!(!copies.paths.is_empty());
        assert!(copies.paths.len() < MAX_FROZEN_COPY_PATHS);
        assert!(copies.bytes <= MAX_FROZEN_COPY_BYTES);
    }
    #[test]
    fn cache_flush_separates_save_batches_and_acknowledges_after_write() {
        let (sender, receiver) = mpsc::channel();
        let (reply, flushed) = mpsc::channel();
        for version in [1, 2] {
            sender
                .send(CacheRequest::Save(Arc::new(Index {
                    built_at: version,
                    ..Index::default()
                })))
                .unwrap();
        }
        sender.send(CacheRequest::Flush(reply)).unwrap();
        for version in [3, 4] {
            sender
                .send(CacheRequest::Save(Arc::new(Index {
                    built_at: version,
                    ..Index::default()
                })))
                .unwrap();
        }
        sender.send(CacheRequest::Stop).unwrap();
        let mut written = Vec::new();
        run_cache_requests(receiver, |index| {
            if index.built_at == 2 {
                assert!(flushed.try_recv().is_err());
            }
            if index.built_at == 4 {
                assert!(flushed.try_recv().is_ok());
            }
            written.push(index.built_at);
        });
        assert_eq!(written, [2, 4]);
    }
    #[test]
    fn cache_flush_without_a_save_and_stop_after_flush_complete() {
        let (sender, receiver) = mpsc::channel();
        let (reply, flushed) = mpsc::channel();
        sender.send(CacheRequest::Flush(reply)).unwrap();
        sender.send(CacheRequest::Stop).unwrap();
        run_cache_requests(receiver, |_| panic!("no save was queued"));
        assert!(flushed.try_recv().is_ok());
    }
    #[test]
    fn cache_stop_does_not_acknowledge_a_later_flush() {
        let (sender, receiver) = mpsc::channel();
        let (reply, flushed) = mpsc::channel();
        sender.send(CacheRequest::Stop).unwrap();
        sender.send(CacheRequest::Flush(reply)).unwrap();
        run_cache_requests(receiver, |_| panic!("no save was queued"));
        assert_eq!(flushed.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }
}
