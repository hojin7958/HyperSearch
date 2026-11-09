#![cfg(windows)]
use quickfind::{
    cache,
    model::{Entry, Index},
    native_app,
    resident::{Manifest, Status},
    settings::Settings,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::*, WindowsAndMessaging::*},
};
fn wide(v: &str) -> Vec<u16> {
    v.encode_utf16().chain(Some(0)).collect()
}
fn own_window() -> HWND {
    let mut previous = std::ptr::null_mut();
    loop {
        let window = unsafe {
            FindWindowExW(
                std::ptr::null_mut(),
                previous,
                wide("QuickFind.MainWindow").as_ptr(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return window;
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut pid);
        }
        if pid == std::process::id() {
            return window;
        }
        previous = window;
    }
}
fn send(window: HWND, message: u32, w: usize, l: isize) -> usize {
    let mut result = 0;
    assert_ne!(
        unsafe {
            SendMessageTimeoutW(
                window,
                message,
                w,
                l,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                1000,
                &mut result,
            )
        },
        0,
        "UI unavailable: hwnd={window:p}, message={message}, error={}",
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    );
    result
}
fn until(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "resident update timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn first_name(list: HWND) -> String {
    let mut buffer = [0u16; 512];
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.pszText = buffer.as_mut_ptr();
    item.cchTextMax = buffer.len() as i32;
    let count = send(
        list,
        LVM_GETITEMTEXTW,
        0,
        (&mut item as *mut LVITEMW) as isize,
    );
    String::from_utf16_lossy(&buffer[..count.min(buffer.len())])
}
fn json(path: &Path, value: &impl serde::Serialize) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn publish(dir: &Path, root: &Path, generation: u128, names: &[&str]) {
    let mut index = Index::default();
    index.roots = vec![root.to_string_lossy().into_owned()];
    index.folders.push(root.to_string_lossy().into_owned());
    for name in names {
        index.entries.push(Entry {
            name: (*name).into(),
            parent: 0,
            size: Some(5),
            created: Some(1),
            modified: Some(1),
            is_dir: false,
        });
    }
    let manifest = Manifest {
        generation,
        entries: names.len(),
        roots: vec![root.into()],
        updated: quickfind::resident::now(),
        ..Manifest::default()
    };
    cache::save(&manifest.index_path(dir), &index).unwrap();
    json(&dir.join("current.json"), &manifest);
}
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let window = own_window();
        if !window.is_null() {
            unsafe {
                PostMessageW(window, WM_APP + 4, 0, 0);
            }
        }
        if let Ok(path) = self.0.canonicalize() {
            assert!(path.starts_with(std::env::temp_dir().canonicalize().unwrap()));
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
#[test]
fn service_snapshots_start_without_a_scan_and_refresh_while_hidden() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-resident-{}-{}",
        std::process::id(),
        quickfind::resident::now()
    )));
    let root = fixture.0.join("files");
    let data = fixture.0.join("data");
    let service = fixture.0.join("service");
    for dir in [&root, &data, &service] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(root.join("must-not-be-scanned.txt"), b"fixture").unwrap();
    json(
        &service.join("status.json"),
        &Status {
            running: false,
            heartbeat: quickfind::resident::now(),
            phase: "watching".into(),
            error: None,
            version: env!("CARGO_PKG_VERSION").into(),
        },
    );
    publish(&service, &root, 1, &["from-service.txt"]);
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
        std::env::set_var("QUICKFIND_RESIDENT_TEST_DIR", &service);
    }
    let settings = Settings {
        roots: vec![root.clone()],
        fast_index: true,
        check_updates_on_startup: false,
        hotkey: "Ctrl+Alt+Shift+F22".into(),
        ..Settings::default()
    };
    settings.save().unwrap();
    let app = std::thread::spawn(move || native_app::run(settings).unwrap());
    until(|| !own_window().is_null());
    let window = own_window();
    until(|| unsafe { !GetDlgItem(window, 108).is_null() && !GetDlgItem(window, 100).is_null() });
    let list = unsafe { GetDlgItem(window, 108) };
    let query = unsafe { GetDlgItem(window, 100) };
    send(query, WM_SETTEXT, 0, wide("from-service").as_ptr() as isize);
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 1 && first_name(list) == "from-service.txt");
    send(
        query,
        WM_SETTEXT,
        0,
        wide("must-not-be-scanned").as_ptr() as isize,
    );
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
    send(window, WM_CLOSE, 0, 0);
    publish(&service, &root, 2, &["after-background.txt", "second.txt"]);
    json(
        &service.join("status.json"),
        &Status {
            running: true,
            heartbeat: quickfind::resident::now(),
            phase: "watching".into(),
            error: None,
            version: env!("CARGO_PKG_VERSION").into(),
        },
    );
    send(
        query,
        WM_SETTEXT,
        0,
        wide("after-background").as_ptr() as isize,
    );
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 1 && first_name(list) == "after-background.txt");
    let before = std::fs::metadata(service.join("snapshot-2.bin")).unwrap();
    let delta = quickfind::resident_delta::Delta {
        base: 2,
        revision: 3,
        updated: quickfind::resident::now(),
        changes: vec![
            quickfind::resident_delta::Change {
                path: root.join("second.txt").to_string_lossy().into_owned(),
                entry: None,
            },
            quickfind::resident_delta::Change {
                path: root.join("delta-only.txt").to_string_lossy().into_owned(),
                entry: Some(Entry {
                    name: "delta-only.txt".into(),
                    parent: 0,
                    size: Some(12),
                    created: Some(2),
                    modified: Some(3),
                    is_dir: false,
                }),
            },
        ],
        ..Default::default()
    };
    json(&service.join("changes.json"), &delta);
    send(query, WM_SETTEXT, 0, wide("delta-only").as_ptr() as isize);
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 1 && first_name(list) == "delta-only.txt");
    assert_eq!(
        std::fs::metadata(service.join("snapshot-2.bin"))
            .unwrap()
            .modified()
            .unwrap(),
        before.modified().unwrap()
    );
    for _ in 0..4 {
        send(window, WM_APP + 3, 0, 0);
        send(window, WM_CLOSE, 0, 0);
    }
    send(query, WM_SETTEXT, 0, wide("from-service").as_ptr() as isize);
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
    assert!(
        !data.join("index.status.json").exists(),
        "legacy elevated indexer was launched"
    );
    assert!(
        !data.join("indexer-events.jsonl").exists()
            || !std::fs::read_to_string(data.join("indexer-events.jsonl"))
                .unwrap()
                .contains("watcher_update")
    );
    send(window, WM_APP + 4, 0, 0);
    app.join().unwrap();
    unsafe {
        std::env::remove_var("QUICKFIND_RESIDENT_TEST_DIR");
        std::env::remove_var("QUICKFIND_DATA_DIR");
    }
}
