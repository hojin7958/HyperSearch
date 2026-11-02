#![cfg(windows)]

use quickfind::{cache, indexer, native_app, settings::Settings};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::MapWindowPoints,
    UI::{Controls::*, WindowsAndMessaging::*},
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn find_own_window(class: &str) -> HWND {
    let class = wide(class);
    let mut previous = std::ptr::null_mut();
    loop {
        let hwnd = unsafe {
            FindWindowExW(
                std::ptr::null_mut(),
                previous,
                class.as_ptr(),
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

#[track_caller]
fn until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "UI condition timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn text(hwnd: HWND) -> String {
    let mut value = vec![0u16; 1024];
    let count = unsafe { GetWindowTextW(hwnd, value.as_mut_ptr(), value.len() as i32) };
    String::from_utf16_lossy(&value[..count.max(0) as usize])
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let main = find_own_window("QuickFind.MainWindow");
        if !main.is_null() {
            unsafe {
                PostMessageW(main, WM_APP + 4, 0, 0);
            }
        }
        if let Ok(path) = self.0.canonicalize() {
            assert!(path.starts_with(std::env::temp_dir().canonicalize().unwrap()));
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[test]
fn settings_save_exclusions_filter_cached_rows_and_preserve_cancelled_removal() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder-settings-{}-{unique}",
        std::process::id()
    )));
    let root = fixture.0.join("검색 폴더");
    let excluded = root.join("제외 폴더");
    let sibling = root.join("제외 폴더2");
    let data = fixture.0.join("data");
    std::fs::create_dir_all(excluded.join("하위")).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(excluded.join("하위\\hidden.xlsx"), b"hidden").unwrap();
    std::fs::write(sibling.join("visible.xlsx"), b"visible").unwrap();
    std::fs::write(root.join("keep.txt"), b"keep").unwrap();
    let settings = Settings {
        roots: vec![root.clone()],
        hotkey: "Ctrl+Alt+Shift+F24".into(),
        fast_index: false,
        ..Settings::default()
    };
    let index = indexer::scan(&settings.roots, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(index.len(), 6);
    cache::save(&data.join("index.bin"), &index).unwrap();
    // This integration test is a separate process; its app thread only uses
    // this private directory and does not acquire the user's GUI instance lock.
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
    }
    let ui = std::thread::spawn(move || native_app::run(settings));
    // A top-level HWND becomes discoverable before WM_CREATE has finished
    // creating its children. Wait for the populated list, not just the HWND.
    until(|| {
        let main = find_own_window("QuickFind.MainWindow");
        let list = unsafe { GetDlgItem(main, 108) };
        !list.is_null() && unsafe { SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) == 6 }
    });
    let main = find_own_window("QuickFind.MainWindow");
    let list = unsafe { GetDlgItem(main, 108) };
    assert!(text(main).contains("*파일명*.xlsx"));
    unsafe {
        SendMessageW(main, WM_COMMAND, 106, 0);
    }
    let dialog = find_own_window("QuickFind.SettingsWindow");
    assert!(!dialog.is_null());
    let excluded_list = unsafe { GetDlgItem(dialog, 220) };
    let roots_list = unsafe { GetDlgItem(dialog, 202) };
    assert_eq!(unsafe { SendMessageW(excluded_list, LB_GETCOUNT, 0, 0) }, 0);
    let mut roots_rect: RECT = unsafe { std::mem::zeroed() };
    let mut excluded_rect: RECT = unsafe { std::mem::zeroed() };
    let mut save_rect: RECT = unsafe { std::mem::zeroed() };
    let mut client: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetWindowRect(roots_list, &mut roots_rect);
        GetWindowRect(excluded_list, &mut excluded_rect);
        GetWindowRect(GetDlgItem(dialog, 200), &mut save_rect);
        GetClientRect(dialog, &mut client);
        MapWindowPoints(
            dialog,
            std::ptr::null_mut(),
            (&mut client as *mut RECT).cast(),
            2,
        );
    }
    assert!(roots_rect.bottom <= excluded_rect.top);
    assert!(save_rect.bottom <= client.bottom && save_rect.right <= client.right);
    let path = wide(&excluded.to_string_lossy());
    unsafe {
        SendMessageW(excluded_list, LB_ADDSTRING, 0, path.as_ptr() as isize);
        SendMessageW(dialog, WM_COMMAND, 200, 0);
    }
    until(|| find_own_window("QuickFind.SettingsWindow").is_null());
    until(|| unsafe { SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) == 3 });
    let saved: Settings =
        serde_json::from_slice(&std::fs::read(data.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved.excluded_folders, [excluded.clone()]);
    assert_eq!(saved.font_size, 9.0);
    let cached = cache::load_with_exclusions(
        &data.join("index.bin"),
        &[root.clone()],
        &[excluded.clone()],
    )
    .unwrap();
    assert_eq!(cached.len(), 3);
    assert!(
        cached
            .entries
            .iter()
            .any(|entry| entry.name == "visible.xlsx")
    );
    assert!(cache::load_with_exclusions(&data.join("index.bin"), &[root], &[]).is_err());
    // Removing a list item then closing without Save leaves the stored scope.
    unsafe {
        SendMessageW(main, WM_COMMAND, 106, 0);
    }
    let dialog = find_own_window("QuickFind.SettingsWindow");
    let excluded_list = unsafe { GetDlgItem(dialog, 220) };
    assert_eq!(unsafe { SendMessageW(excluded_list, LB_GETCOUNT, 0, 0) }, 1);
    unsafe {
        SendMessageW(excluded_list, LB_SETSEL, 1, 0);
        SendMessageW(dialog, WM_COMMAND, 222, 0);
    }
    assert_eq!(unsafe { SendMessageW(excluded_list, LB_GETCOUNT, 0, 0) }, 0);
    unsafe {
        SendMessageW(dialog, WM_CLOSE, 0, 0);
        PostMessageW(main, WM_APP + 4, 0, 0);
    }
    until(|| ui.is_finished());
    ui.join().unwrap().unwrap();
    let saved: Settings =
        serde_json::from_slice(&std::fs::read(data.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved.excluded_folders, [excluded]);
    assert!(std::fs::read(fixture.0.join("검색 폴더\\제외 폴더\\하위\\hidden.xlsx")).is_ok());
}
