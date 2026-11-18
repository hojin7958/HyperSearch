//! Keep direct files in the user's Downloads folder searchable during a service checkpoint.
use crate::{
    exclusions::FolderExclusions,
    model::{Entry, Index},
    settings::Settings,
};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use windows_sys::Win32::UI::Shell::FOLDERID_Downloads;

pub(crate) struct DownloadFolder {
    latest: Arc<Mutex<Option<Arc<Listing>>>>,
    stopped: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
}

pub(crate) struct Listing {
    path: PathBuf,
    files: Vec<Entry>,
}
impl Listing {
    fn read(path: &Path, excluded: &FolderExclusions) -> std::io::Result<Self> {
        let timestamp =
            |ticks: u64| (ticks != 0).then(|| (ticks / 10_000_000) as i64 - 11_644_473_600);
        let mut files = Vec::new();
        for item in std::fs::read_dir(path)? {
            let item = item?;
            if excluded.matches(&item.path()) {
                continue;
            }
            let info = item.metadata()?;
            if info.is_dir() {
                continue;
            }
            files.push(Entry {
                name: item.file_name().to_string_lossy().into_owned(),
                parent: 0,
                size: Some(info.len()),
                created: timestamp(info.creation_time()),
                modified: timestamp(info.last_write_time()),
                is_dir: false,
            });
        }
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            path: path.into(),
            files,
        })
    }

    pub(crate) fn apply(&self, index: &mut Index) {
        let Some(parent) = index.folders.find_path(&self.path.to_string_lossy()) else {
            return;
        };
        let mut remaining: HashMap<_, _> =
            self.files.iter().map(|e| (e.name.as_str(), e)).collect();
        index.entries.retain_in_folder(parent, |entry| {
            if entry.is_dir {
                return true;
            }
            if remaining.get(entry.name).is_some_and(|file| {
                file.size == entry.size
                    && file.created == entry.created
                    && file.modified == entry.modified
            }) {
                remaining.remove(entry.name);
                return true;
            }
            false
        });
        for file in remaining.into_values() {
            let mut entry = file.clone();
            entry.parent = parent;
            index.entries.push(entry);
        }
    }
}

impl DownloadFolder {
    pub(crate) fn for_settings(
        settings: &Settings,
        changed: impl Fn() + Send + 'static,
    ) -> Option<Self> {
        let path = crate::resident::known_folder(&FOLDERID_Downloads).ok()?;
        Self::watch(path, settings, changed)
    }
    fn watch(
        path: PathBuf,
        settings: &Settings,
        changed: impl Fn() + Send + 'static,
    ) -> Option<Self> {
        let included = FolderExclusions::new(&settings.roots);
        let excluded = FolderExclusions::new(&settings.excluded_folders);
        if !included.matches(&path) || excluded.matches(&path) {
            return None;
        }
        let latest = Arc::new(Mutex::new(None));
        let output = latest.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let cancel = stopped.clone();
        let (wake, changes) = mpsc::sync_channel(1);
        let signal = wake.clone();
        std::thread::Builder::new()
            .name("download-folder".into())
            .spawn(move || {
                let Ok(mut watcher) =
                    notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                        if !event.is_ok_and(|e| matches!(e.kind, EventKind::Access(_))) {
                            let _ = signal.try_send(());
                        }
                    })
                else {
                    return;
                };
                if watcher.watch(&path, RecursiveMode::NonRecursive).is_err() {
                    return;
                }
                while !cancel.load(Ordering::Acquire) {
                    let listing = Listing::read(&path, &excluded).ok().map(Arc::new);
                    let mut current = output.lock().unwrap_or_else(|p| p.into_inner());
                    let different = current.as_ref().map(|s: &Arc<Listing>| &s.files)
                        != listing.as_ref().map(|s| &s.files);
                    if different {
                        *current = listing;
                    }
                    drop(current);
                    if different && !cancel.load(Ordering::Acquire) {
                        changed();
                    }
                    // Reconcile occasionally if the folder itself was replaced
                    // and Windows stopped delivering changes on the old handle.
                    let _ = changes.recv_timeout(Duration::from_secs(60));
                    std::thread::sleep(Duration::from_millis(40));
                    let _ = changes.try_recv();
                }
            })
            .ok()?;
        Some(Self {
            latest,
            stopped,
            wake,
        })
    }
    pub(crate) fn latest(&self) -> Option<Arc<Listing>> {
        self.latest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for DownloadFolder {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::AtomicBool,
        time::{Instant, SystemTime},
    };

    #[test]
    fn completed_download_replaces_the_temporary_name_even_with_a_stale_service_index() {
        let root = std::env::temp_dir().join(format!(
            "hyperfinder-download-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("downloads");
        let other = root.join("other");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&other).unwrap();
        let temp = dir.join("Unconfirmed 42.crdownload");
        let final_file = dir.join("보고서.pdf");
        fs::write(&temp, b"fixture").unwrap();
        fs::write(other.join("Unconfirmed 42.crdownload"), b"other").unwrap();
        let settings = Settings {
            roots: vec![root.clone()],
            excluded_folders: vec![dir.join("excluded")],
            ..Default::default()
        };
        let mut base = crate::indexer::scan_with_exclusions(
            &settings.roots,
            &settings.excluded_folders,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        base.share();
        let watch = DownloadFolder::watch(dir.clone(), &settings, || {}).unwrap();
        let wait_for = |name: &str, exists: bool| {
            let start = Instant::now();
            loop {
                if let Some(list) = watch.latest() {
                    if list.files.iter().any(|f| f.name == name) == exists {
                        break list;
                    }
                }
                assert!(
                    start.elapsed() < Duration::from_secs(2),
                    "directory change not delivered"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for("Unconfirmed 42.crdownload", true);
        fs::rename(&temp, &final_file).unwrap();
        fs::write(dir.join("excluded"), b"skip").unwrap();
        let completed = wait_for("보고서.pdf", true);
        let mut view = base.clone();
        completed.apply(&mut view);
        completed.apply(&mut view);
        assert_eq!(
            view.entries
                .iter()
                .filter(|e| e.name == "보고서.pdf" && e.size == Some(7))
                .count(),
            1
        );
        assert_eq!(
            view.entries
                .iter()
                .filter(|e| e.name == "Unconfirmed 42.crdownload")
                .count(),
            1
        );
        assert!(!view.entries.iter().any(|e| e.name == "excluded"));
        fs::remove_file(final_file).unwrap();
        wait_for("보고서.pdf", false).apply(&mut view);
        assert!(!view.entries.iter().any(|e| e.name == "보고서.pdf"));
        assert!(
            DownloadFolder::watch(
                dir.clone(),
                &Settings {
                    excluded_folders: vec![dir.clone()],
                    ..settings
                },
                || {}
            )
            .is_none()
        );
        drop(watch);
        let owned = root.canonicalize().unwrap();
        assert_eq!(
            owned.parent(),
            Some(std::env::temp_dir().canonicalize().unwrap().as_path())
        );
        fs::remove_dir_all(owned).unwrap();
    }
}
