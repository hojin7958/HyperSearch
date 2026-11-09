#![cfg(windows)]
//! Opt-in integration with the installed service. No user settings are changed.
use quickfind::{model::Index, resident};
use std::{
    path::PathBuf,
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
                    .starts_with("hyperfinder-service-")
            );
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn wait_for(index: &mut Option<(u128, Index)>, expected: impl Fn(&Index) -> bool) {
    let request = resident::request(false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some((dir, manifest)) = resident::manifest() {
            let base = index
                .as_ref()
                .filter(|(generation, _)| *generation == manifest.content_checkpoint())
                .map(|(_, index)| index);
            if let Ok(view) = resident::load_view(&dir, &manifest, base) {
                let complete = view.request_id >= request && expected(&view.index);
                *index = Some((manifest.content_checkpoint(), view.index));
                if complete {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "installed service did not publish the fixture change"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[test]
#[ignore = "requires the per-user service already installed and a search root containing the temp directory"]
fn installed_service_publishes_creations_renames_and_deletes_without_rescan() {
    assert!(resident::available(), "install the service first");
    let (dir, before) = resident::manifest().expect("initial service snapshot");
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-service-{}-{}",
        std::process::id(),
        resident::now()
    )));
    assert!(before.roots.iter().any(|root| fixture.0.starts_with(root)));
    std::fs::create_dir(&fixture.0).unwrap();
    let file_name = format!("resident-verification-{}.txt", std::process::id());
    let old_path = fixture.0.join(&file_name);
    let new_name = format!("resident-renamed-{}.txt", std::process::id());
    std::fs::write(&old_path, b"owned service QA fixture").unwrap();
    let started = Instant::now();
    let mut view = None;
    wait_for(&mut view, |index| {
        index.entries.iter().any(|entry| entry.name == file_name)
    });
    let creation_ms = started.elapsed().as_millis();
    std::fs::rename(&old_path, fixture.0.join(&new_name)).unwrap();
    wait_for(&mut view, |index| {
        !index.entries.iter().any(|entry| entry.name == file_name)
            && index.entries.iter().any(|entry| entry.name == new_name)
    });
    std::fs::remove_file(fixture.0.join(&new_name)).unwrap();
    wait_for(&mut view, |index| {
        !index.entries.iter().any(|entry| entry.name == new_name)
    });
    let (_, after) = resident::manifest().unwrap();
    assert_eq!(
        before.full_scans, after.full_scans,
        "ordinary changes must not rebuild the volume"
    );
    assert!(resident::status().is_some_and(|s| s.running && s.error.is_none()));
    println!(
        "installed service: creation_ms={creation_ms}, create/rename/delete passed, full_scans={} unchanged, elapsed_ms={}, directory={}",
        after.full_scans,
        started.elapsed().as_millis(),
        dir.display()
    );
}
