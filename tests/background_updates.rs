#![cfg(windows)]

use quickfind::{cache, engine::SortField, indexer, model::Entry, native_app, settings::Settings};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::*, WindowsAndMessaging::*},
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
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
        "UI stopped responding to message {message}"
    );
    result
}

fn text(window: HWND) -> String {
    let mut buffer = vec![0u16; 2048];
    let count = send(
        window,
        WM_GETTEXT,
        buffer.len(),
        buffer.as_mut_ptr() as isize,
    );
    String::from_utf16_lossy(&buffer[..count])
}

#[track_caller]
fn until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "background update timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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
fn hidden_app_reconciles_a_file_storm_and_keeps_watching() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-background-{}-{unique}",
        std::process::id()
    )));
    let root = fixture.0.join("files");
    let data = fixture.0.join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(root.join("keep.txt"), b"keep").unwrap();
    let settings = Settings {
        roots: vec![root.clone()],
        fast_index: false,
        hotkey: "Ctrl+Alt+Shift+F23".into(),
        ..Settings::default()
    };
    cache::save(
        &data.join("index.bin"),
        &indexer::scan(&settings.roots, &AtomicBool::new(false), |_| {}).unwrap(),
    )
    .unwrap();
    // This test process uses only its own data and HWND; the user's running
    // instance, hotkey and index remain untouched.
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
    }
    let ui = std::thread::spawn(move || native_app::run(settings));
    until(|| {
        let window = own_window();
        let list = unsafe { GetDlgItem(window, 108) };
        !list.is_null() && send(list, LVM_GETITEMCOUNT, 0, 0) == 1
    });
    let window = own_window();
    let list = unsafe { GetDlgItem(window, 108) };
    let query = unsafe { GetDlgItem(window, 100) };
    let status = unsafe { GetDlgItem(window, 110) };
    send(window, WM_CLOSE, 0, 0);
    assert_eq!(unsafe { IsWindowVisible(window) }, 0);
    let burst = root.join("burst");
    std::fs::create_dir(&burst).unwrap();
    let burst_started = Instant::now();
    let mut saw_recovery = false;
    let mut max_response = Duration::ZERO;
    for n in 0..8000 {
        std::fs::write(burst.join(format!("burst-{n:05}.txt")), b"burst").unwrap();
        if n & 31 == 0 {
            let started = Instant::now();
            send(window, WM_NULL, 0, 0);
            max_response = max_response.max(started.elapsed());
            saw_recovery |= text(status).contains("대량 변경");
        }
    }
    assert!(
        burst_started.elapsed() < Duration::from_secs(14),
        "fixture was too slow to accumulate overflow"
    );
    until(|| {
        let started = Instant::now();
        send(window, WM_NULL, 0, 0);
        max_response = max_response.max(started.elapsed());
        saw_recovery |= text(status).contains("대량 변경");
        send(list, LVM_GETITEMCOUNT, 0, 0) == 8002
    });
    assert_eq!(unsafe { IsWindowVisible(window) }, 0);
    // A later create/rename/delete must still propagate while hidden: the old
    // implementation permanently detached its watcher after this overflow.
    std::fs::rename(root.join("keep.txt"), root.join("renamed.txt")).unwrap();
    std::fs::remove_file(burst.join("burst-00000.txt")).unwrap();
    std::fs::write(root.join("after-overflow.txt"), b"after").unwrap();
    let mut last_followup_diagnostic = Instant::now();
    until(|| {
        send(window, WM_NULL, 0, 0);
        let saved = cache::load(&data.join("index.bin"), &[root.clone()]);
        let found = saved
            .as_ref()
            .is_ok_and(|index| index.entries.iter().any(|e| e.name == "after-overflow.txt"));
        if last_followup_diagnostic.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "followup persistence: rows={}, saved_rows={:?}, found={found}, status={}",
                send(list, LVM_GETITEMCOUNT, 0, 0),
                saved.as_ref().map(|index| index.len()),
                text(status)
            );
            last_followup_diagnostic = Instant::now();
        }
        found
    });
    // Querying the updated snapshot must work without opening the window.
    send(
        query,
        WM_SETTEXT,
        0,
        wide("after-overflow").as_ptr() as isize,
    );
    until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 1);
    println!(
        "hidden file storm: 8000 files; max WM_NULL response: {:.2} ms; recovery status observed: {saw_recovery}; continued updates and search passed",
        max_response.as_secs_f64() * 1000.0
    );
    unsafe {
        PostMessageW(window, WM_APP + 4, 0, 0);
    }
    until(|| ui.is_finished());
    ui.join().unwrap().unwrap();
    let saved = cache::load(&data.join("index.bin"), &[root]).unwrap();
    assert_eq!(saved.len(), 8002);
    assert!(saved.entries.iter().any(|e| e.name == "renamed.txt"));
    assert!(
        saved
            .entries
            .iter()
            .all(|e| e.name != "keep.txt" && e.name != "burst-00000.txt")
    );
    assert!(saved.entries.iter().any(|e| e.name == "after-overflow.txt"));
}

fn row_name(window: HWND, list: HWND, row: usize) -> String {
    let mut buffer = [0u16; 256];
    let mut display: NMLVDISPINFOW = unsafe { std::mem::zeroed() };
    display.hdr.hwndFrom = list;
    display.hdr.idFrom = 108;
    display.hdr.code = LVN_GETDISPINFOW;
    display.item.mask = LVIF_TEXT;
    display.item.iItem = row as i32;
    display.item.pszText = buffer.as_mut_ptr();
    display.item.cchTextMax = buffer.len() as i32;
    send(window, WM_NOTIFY, 108, &mut display as *mut _ as isize);
    let length = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[test]
fn million_row_snapshot_keeps_search_and_scrolling_live_during_a_real_file_storm() {
    const SYNTHETIC: usize = 1_000_000;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(
        std::env::temp_dir().join(format!("hyperfinder-large-{}-{unique}", std::process::id())),
    );
    let root = fixture.0.join("files");
    let cold = root.join("unchanged");
    let data = fixture.0.join("data");
    std::fs::create_dir_all(&cold).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(root.join("keep.txt"), b"keep").unwrap();
    let settings = Settings {
        roots: vec![root.clone()],
        fast_index: false,
        sort: SortField::Name,
        hotkey: "Ctrl+Alt+Shift+F22".into(),
        ..Default::default()
    };
    let mut index = indexer::scan(&settings.roots, &AtomicBool::new(false), |_| {}).unwrap();
    let parent = index.folders.find_path(&cold.to_string_lossy()).unwrap();
    // Only these cached records are synthetic; the storm and Win32 controls
    // are real. This measures large-snapshot UI behavior, not a full-drive scan.
    for n in 0..SYNTHETIC {
        index.entries.push(Entry {
            name: format!("stable-{n:07}.txt"),
            parent,
            size: Some(1),
            created: Some(1),
            modified: Some(n as i64),
            is_dir: false,
        });
    }
    index.compact();
    let initial = index.len();
    cache::save(&data.join("index.bin"), &index).unwrap();
    drop(index);
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
    }
    let ui = std::thread::spawn(move || native_app::run(settings));
    until(|| {
        let w = own_window();
        let l = unsafe { GetDlgItem(w, 108) };
        !l.is_null() && send(l, LVM_GETITEMCOUNT, 0, 0) == initial
    });
    let window = own_window();
    let list = unsafe { GetDlgItem(window, 108) };
    let query = unsafe { GetDlgItem(window, 100) };
    let status = unsafe { GetDlgItem(window, 110) };
    send(window, WM_CLOSE, 0, 0);
    let burst = root.join("burst");
    std::fs::create_dir(&burst).unwrap();
    for n in 0..8000 {
        std::fs::write(burst.join(format!("burst-{n:05}.txt")), b"burst").unwrap();
    }
    let expected = initial + 8002;
    let mut max_response = Duration::ZERO;
    let mut latencies = Vec::new();
    let mut saw_update = false;
    let mut probes = 0;
    let mut top = 50_000;
    let mut last_diagnostic = Instant::now();
    until(|| {
        let started = Instant::now();
        send(window, WM_NULL, 0, 0);
        max_response = max_response.max(started.elapsed());
        let message = text(status);
        let busy = message.contains("대량 변경") || message.contains("새 검색 결과 준비");
        if busy && !saw_update {
            saw_update = true;
            // These successive queries must reach the displayed rows while
            // recovery/preparation continues, and the newest query must win.
            for name in ["stable-0000001.txt", "stable-0999999.txt", "keep.txt"] {
                let typed = Instant::now();
                send(query, WM_SETTEXT, 0, wide(name).as_ptr() as isize);
                until(|| {
                    send(list, LVM_GETITEMCOUNT, 0, 0) == 1 && row_name(window, list, 0) == name
                });
                latencies.push(typed.elapsed());
            }
            std::fs::write(burst.join("during-recovery.txt"), b"followup").unwrap();
            send(query, WM_SETTEXT, 0, wide("").as_ptr() as isize);
        }
        let count = send(list, LVM_GETITEMCOUNT, 0, 0);
        if last_diagnostic.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "large fixture progress: rows={count}, expected={expected}, probes={probes}, status={message}"
            );
            last_diagnostic = Instant::now();
        }
        if count >= initial {
            top = (top + 17_000) % 900_000;
            send(list, LVM_ENSUREVISIBLE, top, 0);
            assert!(
                !row_name(window, list, top).is_empty(),
                "row became unavailable during recovery/preparation"
            );
            probes += 1;
        }
        count == expected
    });
    assert!(
        saw_update,
        "large fixture never observed recovery/preparation"
    );
    assert!(probes > 0);
    assert!(
        latencies
            .iter()
            .all(|latency| *latency < Duration::from_secs(1)),
        "input-to-result latency exceeded 1s: {latencies:?}"
    );
    // The million unchanged cached records prove this storm did not cause a
    // root-wide rebuild, which would have dropped those synthetic records.
    assert_eq!(send(list, LVM_GETITEMCOUNT, 0, 0), expected);
    println!(
        "million-row fixture + 8000 real files: max WM_NULL {:.2}ms; query-to-result ms {:?}; scroll probes {probes}",
        max_response.as_secs_f64() * 1000.0,
        latencies
            .iter()
            .map(|t| t.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>()
    );
    unsafe {
        PostMessageW(window, WM_APP + 4, 0, 0);
    }
    until(|| ui.is_finished());
    ui.join().unwrap().unwrap();
    let saved = cache::load(&data.join("index.bin"), &[root]).unwrap();
    assert_eq!(saved.len(), expected);
    assert!(
        saved
            .entries
            .iter()
            .any(|e| e.name == "during-recovery.txt")
    );
}
