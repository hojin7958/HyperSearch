#![cfg(windows)]
use quickfind::{
    cache,
    engine::SortField,
    model::{EntryView, Index},
    native_app,
    settings::Settings,
};
use std::{
    path::PathBuf,
    ptr::null_mut,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HWND as WinHwnd,
        System::{Com::*, Variant::VARIANT},
        UI::Accessibility::*,
    },
    core::BSTR,
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::*, WindowsAndMessaging::*},
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn own_window() -> HWND {
    let mut prev = null_mut();
    loop {
        let h = unsafe {
            FindWindowExW(
                null_mut(),
                prev,
                wide("QuickFind.MainWindow").as_ptr(),
                std::ptr::null(),
            )
        };
        if h.is_null() {
            return h;
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(h, &mut pid);
        }
        if pid == std::process::id() {
            return h;
        }
        prev = h;
    }
}
fn send(h: HWND, m: u32, w: usize, l: isize) -> usize {
    let mut result = 0;
    assert_ne!(
        unsafe {
            SendMessageTimeoutW(h, m, w, l, SMTO_ABORTIFHUNG | SMTO_BLOCK, 3000, &mut result)
        },
        0,
        "UI message timed out"
    );
    result
}
fn until(mut f: impl FnMut() -> bool) {
    let s = Instant::now();
    while !f() {
        assert!(s.elapsed() < Duration::from_secs(45));
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Fixture {
    dir: PathBuf,
    ui: Option<std::thread::JoinHandle<Result<(), String>>>,
    old: Option<std::ffi::OsString>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let h = own_window();
        if !h.is_null() {
            unsafe {
                PostMessageW(h, WM_APP + 4, 0, 0);
            }
        }
        if let Some(ui) = self.ui.take() {
            ui.join().unwrap().unwrap();
        }
        unsafe {
            match &self.old {
                Some(v) => std::env::set_var("QUICKFIND_DATA_DIR", v),
                None => std::env::remove_var("QUICKFIND_DATA_DIR"),
            }
        }
        let dir = self.dir.canonicalize().unwrap();
        assert!(dir.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[test]
fn large_results_are_virtualized_and_offscreen_items_remain_accessible() {
    let count = std::env::var("HYPERFINDER_UIA_ROWS")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(100_000)
        .max(100_000);
    let dir =
        std::env::temp_dir().join(format!("hyperfinder-accessibility-{}", std::process::id()));
    assert!(!dir.exists());
    std::fs::create_dir(&dir).unwrap();
    let mut fixture = Fixture {
        dir: dir.clone(),
        ui: None,
        old: std::env::var_os("QUICKFIND_DATA_DIR"),
    };
    let root = dir.join("files");
    let data = dir.join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&data).unwrap();
    let mut index = Index::default();
    index.roots = vec![root.to_string_lossy().into_owned()];
    index.folders.push(index.roots[0].clone());
    for i in 0..count {
        index.entries.push_view(EntryView {
            name: &format!("item-{i:08}.txt"),
            parent: 0,
            size: Some(i as u64),
            created: None,
            modified: None,
            is_dir: false,
        });
    }
    index.compact();
    cache::save(&data.join("index.bin"), &index).unwrap();
    drop(index);
    let settings = Settings {
        roots: vec![root],
        fast_index: false,
        hotkey: "Ctrl+Alt+Shift+F20".into(),
        sort: SortField::Name,
        descending: false,
        ..Settings::default()
    };
    unsafe {
        std::env::set_var("QUICKFIND_DATA_DIR", &data);
    }
    fixture.ui = Some(std::thread::spawn(move || native_app::run(settings)));
    until(|| {
        let h = own_window();
        let list = unsafe { GetDlgItem(h, 108) };
        !list.is_null() && send(list, LVM_GETITEMCOUNT, 0, 0) == count
    });
    let h = own_window();
    let list = unsafe { GetDlgItem(h, 108) };
    send(h, WM_NULL, 0, 0);
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).unwrap();
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
        let element = automation.ElementFromHandle(WinHwnd(list)).unwrap();
        assert_eq!(
            element.CurrentAutomationId().unwrap().to_string(),
            "search-results"
        );
        let start = Instant::now();
        let condition = automation.CreateTrueCondition().unwrap();
        let children = element.FindAll(TreeScope_Descendants, &condition).unwrap();
        let visible = children.Length().unwrap();
        assert!(
            (1..=256).contains(&visible),
            "exposed {visible} of {count} rows"
        );
        let subtree_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(start.elapsed() < Duration::from_secs(3));
        let container: IUIAutomationItemContainerPattern = element
            .GetCurrentPatternAs(UIA_ItemContainerPatternId)
            .unwrap();
        let last = container
            .FindItemByProperty(
                None,
                UIA_NamePropertyId,
                &VARIANT::from(format!("item-{:08}.txt", count - 1).as_str()),
            )
            .unwrap();
        assert!(last.CurrentIsOffscreen().unwrap().as_bool());
        let realize: IUIAutomationVirtualizedItemPattern = last
            .GetCurrentPatternAs(UIA_VirtualizedItemPatternId)
            .unwrap();
        realize.Realize().unwrap();
        assert!(send(list, LVM_GETTOPINDEX, 0, 0) > count - 100);
        assert!(!last.CurrentIsOffscreen().unwrap().as_bool());
        let selection: IUIAutomationSelectionItemPattern = last
            .GetCurrentPatternAs(UIA_SelectionItemPatternId)
            .unwrap();
        selection.Select().unwrap();
        assert!(selection.CurrentIsSelected().unwrap().as_bool());
        let selected: IUIAutomationSelectionPattern =
            element.GetCurrentPatternAs(UIA_SelectionPatternId).unwrap();
        assert_eq!(selected.GetCurrentSelection().unwrap().Length().unwrap(), 1);
        last.SetFocus().unwrap();
        assert_eq!(
            send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize),
            count - 1
        );
        let scroll: IUIAutomationScrollPattern =
            element.GetCurrentPatternAs(UIA_ScrollPatternId).unwrap();
        scroll.SetScrollPercent(-1.0, 0.0).unwrap();
        assert_eq!(send(list, LVM_GETTOPINDEX, 0, 0), 0);
        scroll
            .Scroll(ScrollAmount_NoAmount, ScrollAmount_SmallIncrement)
            .unwrap();
        assert_eq!(send(list, LVM_GETTOPINDEX, 0, 0), 1);
        if scroll.CurrentHorizontallyScrollable().unwrap().as_bool() {
            scroll.SetScrollPercent(100.0, -1.0).unwrap();
            assert!(scroll.CurrentHorizontalScrollPercent().unwrap() > 90.0);
        }
        let children = element.FindAll(TreeScope_Descendants, &condition).unwrap();
        assert!(children.Length().unwrap() <= 256);
        assert!(
            BSTR::try_from(
                &last
                    .GetCurrentPropertyValue(UIA_HelpTextPropertyId)
                    .unwrap()
            )
            .unwrap()
            .to_string()
            .contains("크기")
        );
        let query = GetDlgItem(h, 100);
        send(query, WM_SETTEXT, 0, wide("no-such-item").as_ptr() as isize);
        until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
        assert!(
            last.CurrentName().is_err(),
            "stale row identities must not point at replacement files"
        );
        let item_condition = automation
            .CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_ListItemControlTypeId.0),
            )
            .unwrap();
        let children = element
            .FindAll(TreeScope_Descendants, &item_condition)
            .unwrap();
        assert_eq!(children.Length().unwrap(), 0);
        println!(
            "UIA: {count} cached rows, {visible} exposed elements including headers, initial subtree read {subtree_ms:.2}ms"
        );
        drop((
            children, selection, realize, last, container, element, condition, automation,
        ));
        CoUninitialize();
    }
}
