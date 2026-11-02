#![cfg(windows)]

use quickfind::{
    cache,
    engine::{SearchEngine, SearchOptions},
    native_app,
    settings::Settings,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::*,
    UI::{Controls::LVM_GETITEMCOUNT, WindowsAndMessaging::*},
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn own_window() -> HWND {
    let mut previous = std::ptr::null_mut();
    loop {
        let hwnd = unsafe {
            FindWindowExW(
                std::ptr::null_mut(),
                previous,
                wide("QuickFind.MainWindow").as_ptr(),
                std::ptr::null(),
            )
        };
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
                10000,
                &mut result,
            )
        },
        0
    );
    result
}
fn text(hwnd: HWND) -> String {
    let mut buffer = [0u16; 1024];
    let count = send(hwnd, WM_GETTEXT, buffer.len(), buffer.as_mut_ptr() as isize);
    String::from_utf16_lossy(&buffer[..count])
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
#[ignore = "opt-in visible startup profile using a read-only copy of the user's cache"]
fn actual_cache_visible_startup() {
    let source = PathBuf::from(
        std::env::var_os("HYPERFINDER_STARTUP_SNAPSHOT_DIR").expect("snapshot directory required"),
    );
    let mut settings: Settings =
        serde_json::from_slice(&std::fs::read(source.join("settings.snapshot.json")).unwrap())
            .unwrap();
    // Isolate cached-list startup from elevation/USN replay. Personal settings
    // and cache are never changed; replay timings come from the actual helper report.
    settings.fast_index = false;
    settings.hotkey = "Ctrl+Alt+Shift+F21".into();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-startup-{}-{unique}",
        std::process::id()
    )));
    std::fs::create_dir_all(&fixture.0).unwrap();
    let data = fixture.0.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::copy(source.join("index.snapshot.bin"), data.join("index.bin")).unwrap();
    let index = cache::load(&data.join("index.bin"), &settings.roots).unwrap();
    let entries = index.len();
    let folders = index.folders.len();
    let expected = SearchEngine::new(Arc::new(index))
        .search(
            &settings.query,
            &SearchOptions {
                mode: settings.mode,
                match_path: settings.match_path,
                case_sensitive: settings.case_sensitive,
                kind: settings.kind,
                sort: settings.sort,
                descending: settings.descending,
            },
        )
        .unwrap()
        .rows
        .len();
    let query_length = settings.query.chars().count();
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
    }
    let start = Instant::now();
    let ui = std::thread::spawn(move || native_app::run(settings));
    let mut first_window_ms = None;
    let mut max_message = Duration::ZERO;
    let mut probes = 0;
    let mut last_state = String::new();
    let window = loop {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "startup timed out: {last_state}"
        );
        let window = own_window();
        let status = unsafe { GetDlgItem(window, 110) };
        let list = unsafe { GetDlgItem(window, 108) };
        if !status.is_null() && !list.is_null() {
            first_window_ms.get_or_insert(start.elapsed().as_secs_f64() * 1000.0);
            let probe = Instant::now();
            send(window, WM_NULL, 0, 0);
            max_message = max_message.max(probe.elapsed());
            probes += 1;
            let status_text = text(status);
            let rows = send(list, LVM_GETITEMCOUNT, 0, 0);
            last_state = format!("rows={rows}, expected={expected}, status={status_text}");
            if rows == expected
                && status_text.contains(&format!("/ {entries}개"))
                && !status_text.contains("검색 중")
            {
                break window;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let first_results_ms = start.elapsed().as_secs_f64() * 1000.0;
    let list = unsafe { GetDlgItem(window, 108) };
    let mut paints = Vec::new();
    for _ in 0..10 {
        let painted = Instant::now();
        unsafe {
            RedrawWindow(
                list,
                std::ptr::null(),
                std::ptr::null_mut(),
                RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );
        }
        send(window, WM_NULL, 0, 0);
        paints.push(painted.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = serde_json::json!({"entries": entries, "folders": folders, "query_length": query_length,
        "matches": expected, "first_window_ms": first_window_ms, "first_results_ms": first_results_ms,
        "max_wm_null_ms": max_message.as_secs_f64()*1000.0, "paint_roundtrip_ms": paints, "message_probes": probes,
        "warm_file_cache": true, "automatic_usn_sync_in_this_profile": false, "visible_window": true});
    println!("startup profile: {report}");
    if let Some(output) = std::env::var_os("HYPERFINDER_STARTUP_PROFILE_OUT") {
        std::fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    unsafe {
        PostMessageW(window, WM_APP + 4, 0, 0);
    }
    let closing = Instant::now();
    while !ui.is_finished() {
        assert!(closing.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(10));
    }
    ui.join().unwrap().unwrap();
}
