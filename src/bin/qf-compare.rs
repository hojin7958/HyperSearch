//! Private QA: compare index contents without printing user filenames.
use quickfind::{cache, indexer, model::EntryView};
use serde::Serialize;
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Serialize)]
struct Metadata {
    size: Option<u64>,
    created: Option<i64>,
    modified: Option<i64>,
    is_dir: bool,
}
impl From<EntryView<'_>> for Metadata {
    fn from(e: EntryView<'_>) -> Self {
        Self {
            size: e.size,
            created: e.created,
            modified: e.modified,
            is_dir: e.is_dir,
        }
    }
}
#[derive(Serialize)]
struct Sample {
    path: String,
    cached: Metadata,
    current: Result<Option<Metadata>, String>,
}
#[derive(Serialize)]
struct Fixture {
    path: String,
    present: bool,
    size_matches: bool,
    created_matches: bool,
    modified_matches: bool,
}
#[derive(Default, Serialize)]
struct Summary {
    old_entries: usize,
    new_entries: usize,
    old_skipped: u64,
    new_skipped: u64,
    common: usize,
    old_only: usize,
    new_only: usize,
    new_duplicate_hashes: usize,
    common_size_differences: usize,
    common_created_differences: usize,
    common_modified_differences: usize,
    missing_sampled: usize,
    missing_still_exist: usize,
    missing_gone: usize,
    missing_inaccessible: usize,
    fixture_items: usize,
    fixture_missing: usize,
    fixture_metadata_differences: usize,
    fixture_stale_entries: usize,
}
#[derive(Serialize)]
struct Report {
    summary: Summary,
    old_only_samples: Vec<Sample>,
    metadata_difference_samples: Vec<Sample>,
    fixture: Vec<Fixture>,
    fixture_stale_paths: Vec<String>,
}

fn key(path: &str) -> u64 {
    let mut h = DefaultHasher::new();
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
        .hash(&mut h);
    h.finish()
}
fn main() {
    if let Err(e) = run() {
        eprintln!("Index comparison failed: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut old = None;
    let mut new = None;
    let mut out = None;
    let mut fixture = None;
    let mut roots = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str(){Some("--old")=>old=Some(PathBuf::from(args.next().ok_or("missing old")?)),Some("--new")=>new=Some(PathBuf::from(args.next().ok_or("missing new")?)),Some("--out")=>out=Some(PathBuf::from(args.next().ok_or("missing out")?)),Some("--fixture")=>fixture=Some(PathBuf::from(args.next().ok_or("missing fixture")?)),Some("--root")=>roots.push(PathBuf::from(args.next().ok_or("missing root")?)),_=>return Err("Usage: qf-compare --old PATH --new PATH --out PRIVATEJSON [--fixture DIR] [--root C:\\]".into())}
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("C:\\"));
    }
    let old = cache::load(&old.ok_or("missing --old")?, &roots)?;
    let new = cache::load(&new.ok_or("missing --new")?, &roots)?;
    let mut keys: Vec<(u64, u32)> = (0..new.entries.len())
        .map(|i| (key(&new.full_path(i)), i as u32))
        .collect();
    keys.sort_unstable();
    let mut s = Summary {
        old_entries: old.entries.len(),
        new_entries: new.entries.len(),
        old_skipped: old.skipped,
        new_skipped: new.skipped,
        ..Summary::default()
    };
    s.new_duplicate_hashes = keys.windows(2).filter(|p| p[0].0 == p[1].0).count();
    let mut seen = vec![false; new.entries.len()];
    let mut old_only_rows = Vec::new();
    let mut differences = Vec::new();
    let mut rng = 0x4939_10af_u64;
    for i in 0..old.entries.len() {
        let path = old.full_path(i);
        let hash = key(&path);
        if let Ok(pos) = keys.binary_search_by_key(&hash, |p| p.0) {
            s.common += 1;
            let row = keys[pos].1 as usize;
            seen[row] = true;
            let a = old.entry(i);
            let b = new.entry(row);
            s.common_size_differences += usize::from(a.size != b.size);
            s.common_created_differences += usize::from(a.created != b.created);
            s.common_modified_differences += usize::from(a.modified != b.modified);
            if differences.len() < 50
                && (a.size != b.size || a.created != b.created || a.modified != b.modified)
            {
                differences.push(Sample {
                    path: path.clone(),
                    cached: b.into(),
                    current: metadata(Path::new(&path)),
                });
            }
        } else {
            s.old_only += 1;
            if old_only_rows.len() < 200 {
                old_only_rows.push(i);
            } else {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let slot = rng as usize % s.old_only;
                if slot < 200 {
                    old_only_rows[slot] = i;
                }
            }
        }
    }
    s.new_only = seen.iter().filter(|v| !**v).count();
    let mut samples = Vec::new();
    for row in old_only_rows {
        let path = old.full_path(row);
        let current = metadata(Path::new(&path));
        match &current {
            Ok(Some(_)) => s.missing_still_exist += 1,
            Ok(None) => s.missing_gone += 1,
            Err(_) => s.missing_inaccessible += 1,
        };
        samples.push(Sample {
            path,
            cached: old.entry(row).into(),
            current,
        });
    }
    s.missing_sampled = samples.len();
    let mut fixtures = Vec::new();
    let mut fixture_stale_paths = Vec::new();
    if let Some(root) = fixture {
        let actual = indexer::scan(&[root], &AtomicBool::new(false), |_| {})?;
        let actual_keys: HashSet<u64> = (0..actual.entries.len())
            .map(|i| key(&actual.full_path(i)))
            .collect();
        for i in 0..actual.entries.len() {
            let path = actual.full_path(i);
            let found = keys
                .binary_search_by_key(&key(&path), |p| p.0)
                .ok()
                .map(|p| keys[p].1 as usize);
            let a = actual.entry(i);
            let (size, created, modified) = found
                .map(|row| {
                    let b = new.entry(row);
                    (
                        a.size == b.size,
                        a.created == b.created,
                        a.modified == b.modified,
                    )
                })
                .unwrap_or((false, false, false));
            s.fixture_items += 1;
            s.fixture_missing += usize::from(found.is_none());
            s.fixture_metadata_differences +=
                usize::from(found.is_some() && (!size || !created || !modified));
            fixtures.push(Fixture {
                path,
                present: found.is_some(),
                size_matches: size,
                created_matches: created,
                modified_matches: modified,
            });
        }
        let root = actual.roots[0]
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase();
        let child_prefix = format!("{root}\\");
        let fixture_folders: HashSet<u32> = new
            .folders
            .iter()
            .enumerate()
            .filter_map(|(id, path)| {
                let path = path
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .to_lowercase();
                (path == root || path.starts_with(&child_prefix)).then_some(id as u32)
            })
            .collect();
        for (i, entry) in new.entries.iter().enumerate() {
            if fixture_folders.contains(&entry.parent) {
                let path = new.full_path(i);
                if !actual_keys.contains(&key(&path)) {
                    fixture_stale_paths.push(path);
                }
            }
        }
        s.fixture_stale_entries = fixture_stale_paths.len();
    }
    println!("{}", serde_json::to_string(&s).map_err(|e| e.to_string())?);
    let report = Report {
        summary: s,
        old_only_samples: samples,
        metadata_difference_samples: differences,
        fixture: fixtures,
        fixture_stale_paths,
    };
    let out = out.ok_or("missing --out")?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        out,
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn metadata(path: &Path) -> Result<Option<Metadata>, String> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null};
    use windows_sys::Win32::{
        Foundation::{FILETIME, GetLastError, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FindClose, FindExInfoBasic, FindExSearchNameMatch,
            FindFirstFileExW, WIN32_FIND_DATAW,
        },
    };
    let p = path.to_string_lossy();
    let p = if p.starts_with(r"\\?\") {
        p.into_owned()
    } else {
        format!(r"\\?\{p}")
    };
    let w: Vec<u16> = OsStr::new(&p).encode_wide().chain(Some(0)).collect();
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            w.as_ptr(),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            null(),
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        return if code == 2 || code == 3 {
            Ok(None)
        } else {
            Err(format!("Win32 error {code}"))
        };
    }
    unsafe { FindClose(h) };
    fn timestamp(t: FILETIME) -> Option<i64> {
        let n = ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        (n != 0).then_some((n / 10_000_000) as i64 - 11_644_473_600)
    }
    let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    Ok(Some(Metadata {
        size: (!is_dir).then_some(((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64),
        created: timestamp(data.ftCreationTime),
        modified: timestamp(data.ftLastWriteTime),
        is_dir,
    }))
}
#[cfg(not(windows))]
fn metadata(_: &Path) -> Result<Option<Metadata>, String> {
    Err("Windows only".into())
}
