#![cfg(windows)]
//! Opt-in administrator QA. Reads NTFS metadata; mutates only its own temp tree.
use quickfind::{cache, ntfs::ResidentIndex};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(path) = self.0.canonicalize() {
            assert!(path.starts_with(std::env::temp_dir().canonicalize().unwrap()));
            assert!(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("hyperfinder-live-")
            );
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
fn catch_up(index: &mut ResidentIndex) {
    let started = Instant::now();
    loop {
        let mut failures = Vec::new();
        index
            .poll(&AtomicBool::new(false), |event, detail| {
                if event.ends_with("failed")
                    || event.ends_with("unavailable")
                    || event.ends_with("retry")
                {
                    failures.push(format!("{event}: {detail}"));
                }
            })
            .unwrap();
        assert!(failures.is_empty(), "{}", failures.join("; "));
        if index.caught_up() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "journal backlog did not catch up"
        );
    }
}
#[test]
#[ignore = "requires administrator; reads one NTFS volume and writes only a private temp fixture"]
fn live_ntfs_backlog_moves_deletes_and_restart_use_journal_without_rebuild() {
    let started = Instant::now();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-live-{}-{}",
        std::process::id(),
        quickfind::resident::now()
    )));
    let files = fixture.0.join("files");
    let output = fixture.0.join("state");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::create_dir(files.join("old")).unwrap();
    std::fs::write(files.join("old\\before.txt"), b"old").unwrap();
    std::fs::write(files.join("deleted.txt"), b"old").unwrap();
    let mut index = ResidentIndex::new(vec![files.clone()], vec![], None);
    index.set_private_roots(vec![output.clone()]);
    catch_up(&mut index);
    assert_eq!(index.full_scans, 1);
    let first = output.join("first.bin");
    index.checkpoint(&first).unwrap();
    let initial_ms = started.elapsed().as_millis();
    std::fs::rename(files.join("old"), files.join("moved")).unwrap();
    std::fs::remove_file(files.join("deleted.txt")).unwrap();
    for n in 0..5000 {
        std::fs::write(files.join(format!("burst-{n:05}.txt")), b"new").unwrap();
    }
    catch_up(&mut index);
    assert_eq!(index.full_scans, 1, "a burst must not rebuild the volume");
    let second = output.join("second.bin");
    index.checkpoint(&second).unwrap();
    let saved = cache::load(&second, &[files.clone()]).unwrap();
    let paths: HashSet<String> = (0..saved.len()).map(|row| saved.full_path(row)).collect();
    assert!(
        paths.contains(
            &files
                .join("moved\\before.txt")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert!(!paths.contains(&files.join("old\\before.txt").to_string_lossy().into_owned()));
    assert!(!paths.contains(&files.join("deleted.txt").to_string_lossy().into_owned()));
    for n in 0..5000 {
        assert!(
            paths.contains(
                &files
                    .join(format!("burst-{n:05}.txt"))
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }
    drop(index);
    drop(saved);
    drop(paths);
    std::fs::write(files.join("while-stopped.txt"), b"restart").unwrap();
    let mut resumed = ResidentIndex::new(
        vec![files.clone()],
        vec![],
        Some(&second.with_extension("ntfs.bin")),
    );
    resumed.set_private_roots(vec![output.clone()]);
    catch_up(&mut resumed);
    assert_eq!(
        resumed.full_scans, 0,
        "valid checkpoint must resume with USN"
    );
    let third = output.join("third.bin");
    resumed.checkpoint(&third).unwrap();
    let saved = cache::load(&third, &[files.clone()]).unwrap();
    assert!(saved.entries.iter().any(|e| e.name == "while-stopped.txt"));
    // Metadata-only checkpoint reuses identical public bytes and remains readable.
    let unchanged = output.join("unchanged.bin");
    resumed.checkpoint_unchanged(&unchanged, &third).unwrap();
    assert_eq!(
        std::fs::read(&unchanged).unwrap(),
        std::fs::read(&third).unwrap()
    );
    println!(
        "live NTFS: initial_ms={initial_ms}; 5000 creations, parent move, deletion, stopped change and resume passed; recovery_full_scans=0; total_ms={}",
        started.elapsed().as_millis()
    );
}
