//! Small Windows shell integration; no polling or keyboard hook.

use std::{cell::Cell, path::PathBuf, sync::mpsc, thread, time::Duration};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*, Threading::*},
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const CALLBACK: u32 = WM_APP + 1;
const COMMAND: u32 = WM_APP + 2;
const SHOW_EXISTING: u32 = WM_APP + 3;
const TRAY_KEYSELECT: u32 = 0x401;
const CLASS: &str = "QuickFind.MessageWindow";
pub const MAIN_CLASS: &str = "QuickFind.MainWindow";

pub fn app_icon(small: bool) -> HICON {
    unsafe {
        let (width, height) = if small {
            (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
        } else {
            (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
        };
        let icon = LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1usize as *const u16,
            IMAGE_ICON,
            width,
            height,
            LR_SHARED,
        );
        if icon.is_null() {
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn local_data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("QUICKFIND_DATA_DIR") {
        let path = PathBuf::from(p);
        return std::path::absolute(&path).unwrap_or(path);
    }
    // The Hyperfinder rename retains this directory and QUICKFIND_DATA_DIR
    // so existing settings, caches and private diagnostic runs keep working.
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("QuickFind")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}

pub fn parse_hotkey(text: &str) -> Result<Hotkey, String> {
    let mut modifiers = MOD_NOREPEAT;
    let mut key = None;
    for raw in text.split('+') {
        let part = raw.trim().to_ascii_uppercase();
        let modifier = match part.as_str() {
            "CTRL" | "CONTROL" => Some(MOD_CONTROL),
            "ALT" => Some(MOD_ALT),
            "SHIFT" => Some(MOD_SHIFT),
            "WIN" | "WINDOWS" => Some(MOD_WIN),
            _ => None,
        };
        if let Some(m) = modifier {
            modifiers |= m;
            continue;
        }
        let k = if part.len() == 1 && part.as_bytes()[0].is_ascii_alphanumeric() {
            u32::from(part.as_bytes()[0])
        } else if let Some(n) = part
            .strip_prefix("NUM")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n <= 9)
        {
            u32::from(VK_NUMPAD0) + n
        } else if let Some(n) = part
            .strip_prefix('F')
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| (1..=24).contains(n))
        {
            u32::from(VK_F1) + n - 1
        } else {
            return Err("단축키는 Ctrl+Alt+1, Ctrl+Shift+F 같은 형식으로 입력하세요.".into());
        };
        if key.replace(k).is_some() {
            return Err("단축키에는 일반 키를 하나만 지정하세요.".into());
        }
    }
    if modifiers & (MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN) == 0 {
        return Err("Ctrl, Alt, Shift 중 보조 키를 지정하세요.".into());
    }
    Ok(Hotkey {
        modifiers,
        key: key.ok_or("단축키의 일반 키가 비어 있습니다.")?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum ShellEvent {
    Toggle,
    Show,
    Exit,
}
enum ShellCommand {
    Hotkey(Hotkey, mpsc::Sender<Result<(), String>>),
    Pause(mpsc::Sender<()>),
    Stop,
}

pub struct Shell {
    events: mpsc::Receiver<ShellEvent>,
    commands: mpsc::Sender<ShellCommand>,
    hwnd: isize,
    join: Option<thread::JoinHandle<()>>,
    pub hotkey_error: Option<String>,
}

struct ShellWindow {
    events: mpsc::Sender<ShellEvent>,
    commands: mpsc::Receiver<ShellCommand>,
    notify_hwnd: isize,
    notify_message: u32,
    current: Cell<Option<Hotkey>>,
    hotkey_id: Cell<i32>,
    tray: NOTIFYICONDATAW,
    taskbar_created: u32,
    menu_active: Cell<bool>,
    closing: Cell<bool>,
}

impl ShellWindow {
    unsafe fn emit(&self, event: ShellEvent) {
        if !self.closing.get() && self.events.send(event).is_ok() && self.notify_hwnd != 0 {
            unsafe { PostMessageW(self.notify_hwnd as HWND, self.notify_message, 0, 0) };
        }
    }

    unsafe fn close(&self) {
        if !self.closing.replace(true) {
            unsafe {
                if self.menu_active.get() {
                    EndMenu();
                }
                PostQuitMessage(0);
            }
        }
    }

    unsafe fn popup(&self, hwnd: HWND) {
        if self.closing.get() || self.menu_active.replace(true) {
            return;
        }
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                self.menu_active.set(false);
                return;
            }
            AppendMenuW(menu, MF_STRING, 1, wide("열기").as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, 2, wide("종료").as_ptr());
            let mut pos: POINT = std::mem::zeroed();
            GetCursorPos(&mut pos);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetForegroundWindow(hwnd);
            // A nested menu loop dispatches to this window procedure too.
            // Keep only shared state across it so close/hotkey messages can
            // safely re-enter the handler.
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                pos.x,
                pos.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            DestroyMenu(menu);
            self.menu_active.set(false);
            PostMessageW(hwnd, WM_NULL, 0, 0);
            ShowWindow(hwnd, SW_HIDE);
            match selected {
                1 => self.emit(ShellEvent::Show),
                2 => self.emit(ShellEvent::Exit),
                _ => {}
            }
        }
    }

    unsafe fn run_commands(&self, hwnd: HWND) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                ShellCommand::Stop => {
                    unsafe { self.close() };
                    break;
                }
                ShellCommand::Pause(reply) => {
                    if self.current.take().is_some() {
                        unsafe {
                            UnregisterHotKey(hwnd, self.hotkey_id.get());
                        }
                    }
                    let _ = reply.send(());
                }
                ShellCommand::Hotkey(hk, reply) => {
                    let result = if self.current.get() == Some(hk) {
                        Ok(())
                    } else {
                        let previous = self.hotkey_id.get();
                        let next = if previous == 1 { 2 } else { 1 };
                        unsafe {
                            if RegisterHotKey(hwnd, next, hk.modifiers, hk.key) != 0 {
                                if self.current.get().is_some() {
                                    UnregisterHotKey(hwnd, previous);
                                }
                                self.hotkey_id.set(next);
                                self.current.set(Some(hk));
                                Ok(())
                            } else {
                                Err("다른 프로그램에서 사용하는 단축키입니다. 기존 단축키를 유지합니다.".into())
                            }
                        }
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ShellWindow;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let state = &*state;
        match msg {
            WM_CLOSE => state.close(),
            WM_HOTKEY => {
                if wp == state.hotkey_id.get() as usize && state.current.get().is_some() {
                    state.emit(ShellEvent::Toggle);
                }
            }
            SHOW_EXISTING => state.emit(ShellEvent::Show),
            CALLBACK => match lp as u32 & 0xffff {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | TRAY_KEYSELECT => {
                    state.emit(ShellEvent::Show);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => state.popup(hwnd),
                _ => {}
            },
            COMMAND => state.run_commands(hwnd),
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            _ if msg == state.taskbar_created && msg != 0 && !state.closing.get() => {
                Shell_NotifyIconW(NIM_ADD, &state.tray);
                Shell_NotifyIconW(NIM_SETVERSION, &state.tray);
            }
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        0
    }
}

impl Shell {
    pub fn new_native(
        notify_hwnd: isize,
        notify_message: u32,
        shortcut: &str,
    ) -> Result<Self, String> {
        let parsed = parse_hotkey(shortcut);
        let (event_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::spawn(move || unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = wide(CLASS);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let mut state = Box::new(ShellWindow {
                events: event_tx,
                commands: command_rx,
                notify_hwnd,
                notify_message,
                current: Cell::new(None),
                hotkey_id: Cell::new(1),
                tray: std::mem::zeroed(),
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                menu_active: Cell::new(false),
                closing: Cell::new(false),
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                state.as_mut() as *mut ShellWindow as _,
            );
            if hwnd.is_null() {
                let _ = ready_tx.send(Err(format!("트레이 창 생성 실패: {}", GetLastError())));
                return;
            }
            let error = match parsed {
                Ok(hk)
                    if RegisterHotKey(hwnd, state.hotkey_id.get(), hk.modifiers, hk.key) != 0 =>
                {
                    state.current.set(Some(hk));
                    None
                }
                Ok(_) => {
                    Some("단축키가 다른 프로그램에서 사용 중입니다. 설정에서 변경하세요.".into())
                }
                Err(e) => Some(e),
            };
            let icon = app_icon(true);
            let mut tray: NOTIFYICONDATAW = std::mem::zeroed();
            tray.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            tray.hWnd = hwnd;
            tray.uID = 1;
            tray.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            tray.uCallbackMessage = CALLBACK;
            tray.hIcon = icon;
            let tip = wide("Hyperfinder · 파일 검색");
            tray.szTip[..tip.len()].copy_from_slice(&tip);
            if Shell_NotifyIconW(NIM_ADD, &tray) == 0 {
                if state.current.get().is_some() {
                    UnregisterHotKey(hwnd, state.hotkey_id.get());
                }
                DestroyWindow(hwnd);
                let _ = ready_tx.send(Err("알림 영역 아이콘을 만들 수 없습니다.".into()));
                return;
            }
            tray.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &tray);
            state.tray = tray;
            let _ = ready_tx.send(Ok((hwnd as isize, error)));
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if state.current.get().is_some() {
                UnregisterHotKey(hwnd, state.hotkey_id.get());
            }
            Shell_NotifyIconW(NIM_DELETE, &tray);
            DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), instance);
        });
        let (hwnd, hotkey_error) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "트레이 초기화 응답이 없습니다.".to_string())??;
        Ok(Self {
            events,
            commands,
            hwnd,
            join: Some(join),
            hotkey_error,
        })
    }
    pub fn try_recv(&self) -> Option<ShellEvent> {
        self.events.try_recv().ok()
    }
    pub fn set_hotkey(&mut self, text: &str) -> Result<(), String> {
        let hk = parse_hotkey(text)?;
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(ShellCommand::Hotkey(hk, tx))
            .map_err(|e| e.to_string())?;
        unsafe {
            PostMessageW(self.hwnd as HWND, COMMAND, 0, 0);
        }
        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "단축키 설정 시간이 초과됐습니다.".to_string())?;
        if result.is_ok() {
            self.hotkey_error = None;
        }
        result
    }
    pub fn pause_hotkey(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(ShellCommand::Pause(tx))
            .map_err(|e| e.to_string())?;
        unsafe {
            PostMessageW(self.hwnd as HWND, COMMAND, 0, 0);
        }
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| "단축키 입력 준비 시간이 초과됐습니다.".into())
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.commands.send(ShellCommand::Stop);
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(t) = self.join.take() {
            let _ = t.join();
        }
    }
}

pub fn copy_path(text: &str) -> Result<(), String> {
    let data = wide(text);
    unsafe {
        let mut owner = GetActiveWindow();
        if owner.is_null() {
            owner = FindWindowW(wide(CLASS).as_ptr(), std::ptr::null());
        }
        if owner.is_null() {
            return Err("클립보드 소유 창을 찾을 수 없습니다.".into());
        }
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(owner) != 0 {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !opened {
            return Err("클립보드가 사용 중입니다. 다시 클릭하세요.".into());
        }
        let result = (|| {
            let bytes = data.len() * std::mem::size_of::<u16>();
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if memory.is_null() {
                return Err("클립보드 메모리를 할당할 수 없습니다.".into());
            }
            let ptr = GlobalLock(memory);
            if ptr.is_null() {
                GlobalFree(memory);
                return Err("클립보드 메모리를 열 수 없습니다.".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr().cast::<u8>(), ptr.cast::<u8>(), bytes);
            GlobalUnlock(memory);
            if EmptyClipboard() == 0 || SetClipboardData(13, memory).is_null() {
                GlobalFree(memory);
                return Err("경로를 복사할 수 없습니다.".into());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

pub fn open_path(path: &str) -> Result<(), String> {
    unsafe {
        let code = ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(path).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        ) as isize;
        if code <= 32 {
            Err(format!("파일을 열 수 없습니다. Windows 오류: {code}"))
        } else {
            Ok(())
        }
    }
}
pub fn reveal_path(path: &str) -> Result<(), String> {
    use windows::{
        Win32::{
            System::Com::CoTaskMemFree,
            UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName},
        },
        core::PCWSTR,
    };
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
    };
    unsafe {
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        if initialized < 0 {
            return Err(format!(
                "파일 탐색기 준비 실패: 0x{:08X}",
                initialized as u32
            ));
        }
        let mut pidl = std::ptr::null_mut();
        let result = (|| {
            let location = std::path::Path::new(path);
            if location.is_dir() || location.parent().is_none() {
                return open_path(path);
            }
            let path_wide = wide(path);
            SHParseDisplayName(PCWSTR(path_wide.as_ptr()), None, &mut pidl, 0, None)
                .map_err(|error| format!("경로를 찾을 수 없습니다: {error}"))?;
            SHOpenFolderAndSelectItems(pidl, None, 0)
                .map_err(|error| format!("파일 탐색기에서 위치를 열 수 없습니다: {error}"))
        })();
        CoTaskMemFree(Some(pidl.cast()));
        CoUninitialize();
        result
    }
}

/// Delete one explicitly confirmed item through the Windows file-operation UI.
/// Call on a worker thread: this function initializes an STA on that thread.
/// Windows may still warn when the target cannot be moved to the Recycle Bin.
pub fn delete_path(owner: isize, path: &str) -> Result<bool, String> {
    delete_paths(owner, &[path.to_owned()])
}
pub fn delete_paths(owner: isize, paths: &[String]) -> Result<bool, String> {
    use windows::{
        Win32::{
            Foundation::HWND as ShellHwnd,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            UI::Shell::{
                FOF_ALLOWUNDO, FOF_NO_CONNECTED_ELEMENTS, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING,
                FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem,
                SHCreateItemFromParsingName,
            },
        },
        core::PCWSTR,
    };
    if paths.is_empty() || paths.len() > 4096 {
        return Err("한 번에 1~4,096개 항목을 삭제할 수 있습니다.".into());
    }
    for path in paths {
        validate_delete_path(path)?;
    }
    let paths = crate::file_clipboard::top_level_paths(paths);
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|error| format!("삭제 작업을 준비할 수 없습니다: {error}"))?;
        let _apartment = Apartment;
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| format!("Windows 삭제 작업을 준비할 수 없습니다: {error}"))?;
        operation
            .SetOwnerWindow(ShellHwnd(owner as _))
            .map_err(|error| format!("삭제 확인 창을 준비할 수 없습니다: {error}"))?;
        operation
            .SetOperationFlags(
                FOF_ALLOWUNDO
                    | FOFX_RECYCLEONDELETE
                    | FOF_WANTNUKEWARNING
                    | FOF_NOCONFIRMATION
                    | FOF_NO_CONNECTED_ELEMENTS,
            )
            .map_err(|error| format!("삭제 옵션을 설정할 수 없습니다: {error}"))?;
        for path in &paths {
            let path_wide = wide(path);
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(path_wide.as_ptr()), None)
                .map_err(|error| format!("삭제할 파일을 찾을 수 없습니다: {error}"))?;
            operation
                .DeleteItem(&item, None)
                .map_err(|error| format!("삭제 작업을 등록할 수 없습니다: {error}"))?;
        }
        let performed = operation.PerformOperations();
        // A successful HRESULT alone does not prove that a file operation ran:
        // cancellations can also report success. Query this after failures too.
        let aborted = operation
            .GetAnyOperationsAborted()
            .map(|value| value.as_bool());
        classify_delete_result(performed, aborted)
    }
}

/// Run on the UI STA. Cancelling the picker does not change the saved roots.
pub fn choose_folders(owner: isize) -> Result<Vec<std::path::PathBuf>, String> {
    use windows::{
        Win32::{
            Foundation::{ERROR_CANCELLED, HWND},
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            UI::Shell::{
                FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
                FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::PCWSTR,
    };
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| e.to_string())?;
        dialog
            .SetOptions(
                dialog.GetOptions().map_err(|e| e.to_string())?
                    | FOS_PICKFOLDERS
                    | FOS_FORCEFILESYSTEM
                    | FOS_PATHMUSTEXIST
                    | FOS_ALLOWMULTISELECT,
            )
            .map_err(|e| e.to_string())?;
        dialog
            .SetTitle(PCWSTR(wide("검색할 폴더 선택").as_ptr()))
            .map_err(|e| e.to_string())?;
        if let Err(e) = dialog.Show(Some(HWND(owner as _))) {
            return if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                Ok(Vec::new())
            } else {
                Err(format!("폴더 선택 창: {e}"))
            };
        }
        let items = dialog.GetResults().map_err(|e| e.to_string())?;
        let mut paths = Vec::new();
        for i in 0..items.GetCount().map_err(|e| e.to_string())? {
            let item = items.GetItemAt(i).map_err(|e| e.to_string())?;
            let raw = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| e.to_string())?;
            let text = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast()));
            paths.push(std::path::PathBuf::from(text.map_err(|e| e.to_string())?));
        }
        Ok(paths)
    }
}

pub fn captured_hotkey(key: u32, ctrl: bool, alt: bool, shift: bool, win: bool) -> Option<String> {
    let name = match key {
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(key)?.to_string(),
        0x60..=0x69 => format!("Num{}", key - 0x60),
        0x70..=0x87 => format!("F{}", key - 0x70 + 1),
        _ => return None,
    };
    let mut parts = Vec::new();
    if ctrl {
        parts.push("Ctrl".to_owned());
    }
    if alt {
        parts.push("Alt".to_owned());
    }
    if shift {
        parts.push("Shift".to_owned());
    }
    if win {
        parts.push("Win".to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(name);
    Some(parts.join("+"))
}

fn validate_delete_path(path: &str) -> Result<(), String> {
    use std::path::{Component, Path, Prefix};
    if path.is_empty() || path.contains('\0') {
        return Err("삭제할 파일 경로가 올바르지 않습니다.".into());
    }
    let location = Path::new(path);
    if !location.is_absolute() || location.parent().is_none() || location.file_name().is_none() {
        return Err("루트가 아닌 파일·폴더의 전체 경로만 삭제할 수 있습니다.".into());
    }
    let mut parts = location.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return Err("전체 Windows 경로가 필요합니다.".into());
    };
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _)
    ) {
        return Err("장치 또는 Windows 네임스페이스 경로는 삭제할 수 없습니다.".into());
    }
    if !matches!(parts.next(), Some(Component::RootDir)) {
        return Err("드라이브 또는 공유 폴더의 전체 경로가 필요합니다.".into());
    }
    let mut children = 0;
    for part in parts {
        match part {
            Component::Normal(name) if !name.to_string_lossy().contains(':') => children += 1,
            _ => return Err("상위 폴더 이동이나 특수 경로는 삭제할 수 없습니다.".into()),
        }
    }
    if children == 0 {
        return Err("드라이브와 공유 폴더의 루트는 삭제할 수 없습니다.".into());
    }
    Ok(())
}

fn classify_delete_result(
    performed: windows::core::Result<()>,
    aborted: windows::core::Result<bool>,
) -> Result<bool, String> {
    match performed {
        Err(error) if deletion_was_cancelled(error.code()) => Ok(false),
        Err(error) => Err(format!("파일을 삭제할 수 없습니다: {error}")),
        Ok(()) => match aborted {
            Ok(true) => Ok(false),
            Ok(false) => Ok(true),
            Err(error) => Err(format!("삭제 완료 여부를 확인할 수 없습니다: {error}")),
        },
    }
}
fn deletion_was_cancelled(code: windows::core::HRESULT) -> bool {
    use windows::Win32::{
        Foundation::E_ABORT,
        UI::Shell::{COPYENGINE_E_CANCELLED, COPYENGINE_E_USER_CANCELLED},
    };
    matches!(
        code,
        E_ABORT | COPYENGINE_E_CANCELLED | COPYENGINE_E_USER_CANCELLED
    ) || code == windows::core::HRESULT::from_win32(ERROR_CANCELLED)
        || code == windows::core::HRESULT::from_win32(ERROR_OPERATION_ABORTED)
}

pub struct InstanceGuard(HANDLE);
pub fn single_instance() -> Result<InstanceGuard, String> {
    unsafe {
        let handle = CreateMutexW(
            std::ptr::null(),
            0,
            wide("Local\\QuickFind.SingleInstance").as_ptr(),
        );
        if handle.is_null() {
            return Err(format!("실행 잠금 실패: {}", GetLastError()));
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            let mut main = std::ptr::null_mut();
            for _ in 0..40 {
                main = FindWindowW(wide(MAIN_CLASS).as_ptr(), std::ptr::null());
                if !main.is_null() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !main.is_null() {
                let mut pid = 0;
                GetWindowThreadProcessId(main, &mut pid);
                AllowSetForegroundWindow(pid);
                if IsIconic(main) != 0 {
                    ShowWindowAsync(main, SW_RESTORE);
                } else {
                    ShowWindowAsync(main, SW_SHOW);
                }
                // The existing UI performs its own focus activation when it
                // receives this message. Never synchronously enter a stalled
                // process merely because its executable was launched again.
                PostMessageW(main, SHOW_EXISTING, 0, 0);
            } else {
                let hwnd = FindWindowW(wide(CLASS).as_ptr(), std::ptr::null());
                if !hwnd.is_null() {
                    PostMessageW(hwnd, SHOW_EXISTING, 0, 0);
                }
            }
            return Err("already-running".into());
        }
        Ok(InstanceGuard(handle))
    }
}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Elevate only a worker instance of this executable. Call on a background thread.
pub fn run_fast_index(
    roots: &[PathBuf],
    excluded_folders: &[PathBuf],
    output: &std::path::Path,
    rebuild: bool,
    background: bool,
) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
    };
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = std::path::absolute(output).map_err(|e| e.to_string())?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(output.with_extension("error.txt"));
    let mut parameters = format!(
        "--indexer --output {}",
        quote_windows_arg(output.as_os_str())
    );
    if rebuild {
        parameters.push_str(" --rebuild");
    }
    if background {
        parameters.push_str(" --background");
    }
    for root in roots {
        let root = std::path::absolute(root).map_err(|e| e.to_string())?;
        parameters.push_str(" --root ");
        parameters.push_str(&quote_windows_arg(root.as_os_str()));
    }
    for excluded in excluded_folders {
        let excluded = std::path::absolute(excluded).map_err(|e| e.to_string())?;
        parameters.push_str(" --exclude ");
        parameters.push_str(&quote_windows_arg(excluded.as_os_str()));
    }
    let executable_wide = wide(&executable.to_string_lossy());
    let parameters_wide = wide(&parameters);
    let verb = wide("runas");
    unsafe {
        let com = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut launch: SHELLEXECUTEINFOW = std::mem::zeroed();
        launch.cbSize = std::mem::size_of_val(&launch) as u32;
        launch.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        launch.hwnd = FindWindowW(wide(MAIN_CLASS).as_ptr(), std::ptr::null());
        launch.lpVerb = verb.as_ptr();
        launch.lpFile = executable_wide.as_ptr();
        launch.lpParameters = parameters_wide.as_ptr();
        launch.nShow = SW_HIDE;
        let started = ShellExecuteExW(&mut launch);
        let error = GetLastError();
        if com >= 0 {
            CoUninitialize();
        }
        if started == 0 {
            if error == ERROR_CANCELLED {
                return Err("고속 인덱싱을 취소했습니다.".into());
            }
            return Err(format!(
                "고속 인덱서를 시작할 수 없습니다: {}",
                std::io::Error::from_raw_os_error(error as i32)
            ));
        }
        if launch.hProcess.is_null() {
            return Err("인덱서 프로세스 핸들이 없습니다.".into());
        }
        let wait = WaitForSingleObject(launch.hProcess, INFINITE);
        let mut exit_code = 1;
        let exit_ok = GetExitCodeProcess(launch.hProcess, &mut exit_code);
        CloseHandle(launch.hProcess);
        if wait != WAIT_OBJECT_0 || exit_ok == 0 {
            return Err("고속 인덱서 실행 결과를 확인할 수 없습니다.".into());
        }
        if exit_code != 0 {
            return Err(std::fs::read_to_string(output.with_extension("error.txt"))
                .unwrap_or_else(|_| format!("고속 인덱서가 오류 {exit_code}로 종료됐습니다.")));
        }
        Ok(())
    }
}

fn quote_windows_arg(argument: &std::ffi::OsStr) -> String {
    let mut output = String::from("\"");
    let mut slashes = 0;
    for ch in argument.to_string_lossy().chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            output.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        output.push(ch);
    }
    output.extend(std::iter::repeat_n('\\', slashes * 2));
    output.push('"');
    output
}

pub fn request_exit_existing() {
    unsafe {
        let hwnd = FindWindowW(wide(MAIN_CLASS).as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            PostMessageW(hwnd, WM_APP + 4, 0, 0);
        }
    }
}

pub fn memory_usage() -> (usize, usize) {
    unsafe {
        let mut counters: windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS =
            std::mem::zeroed();
        counters.cb = std::mem::size_of_val(&counters) as u32;
        if windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of_val(&counters) as u32,
        ) == 0
        {
            return (0, 0);
        }
        (counters.WorkingSetSize, counters.PeakWorkingSetSize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delete_validation_accepts_qualified_child_paths_only() {
        for path in [
            r"C:\folder\file.txt",
            r"D:\한글 폴더\보고서.docx",
            r"C:\folder",
            r"\\server\share\folder\file.txt",
            r"\\?\C:\folder\file.txt",
            r"\\?\UNC\server\share\folder\file.txt",
        ] {
            assert!(validate_delete_path(path).is_ok(), "{path}");
        }
        for path in [
            "",
            r"file.txt",
            r"C:file.txt",
            r"\folder\file.txt",
            r"C:\",
            r"C:",
            r"\\server\share",
            r"\\server\share\",
            r"\\?\C:\",
            r"\\?\UNC\server\share\",
            r"C:\folder\..",
            r"C:\folder\..\other.txt",
            r"\\.\C:\folder\file.txt",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\file.txt",
            r"C:\file.txt:stream",
            "C:\\folder\\file\0.txt",
        ] {
            assert!(validate_delete_path(path).is_err(), "{path:?}");
        }
    }
    #[test]
    fn deletion_success_requires_successful_perform_and_no_aborted_operations() {
        use windows::{
            Win32::Foundation::E_ACCESSDENIED,
            core::{Error, HRESULT},
        };
        assert_eq!(classify_delete_result(Ok(()), Ok(false)).unwrap(), true);
        assert_eq!(classify_delete_result(Ok(()), Ok(true)).unwrap(), false);
        assert!(classify_delete_result(Ok(()), Err(Error::from_hresult(E_ACCESSDENIED))).is_err());
        assert!(
            classify_delete_result(Err(Error::from_hresult(E_ACCESSDENIED)), Ok(false)).is_err()
        );
        assert!(
            classify_delete_result(Err(Error::from_hresult(E_ACCESSDENIED)), Ok(true)).is_err()
        );
        for code in [
            0x80004004u32,
            0x800704C7,
            0x800703E3,
            0x80270000,
            0x80270001,
        ] {
            assert_eq!(
                classify_delete_result(Err(Error::from_hresult(HRESULT(code as i32))), Ok(false))
                    .unwrap(),
                false
            );
        }
    }
    #[test]
    fn cancellation_is_never_reported_as_completed_when_abort_query_fails() {
        use windows::{
            Win32::Foundation::{E_ABORT, E_ACCESSDENIED},
            core::Error,
        };
        assert_eq!(
            classify_delete_result(
                Err(Error::from_hresult(E_ABORT)),
                Err(Error::from_hresult(E_ACCESSDENIED))
            )
            .unwrap(),
            false
        );
    }
    #[test]
    fn hotkey_parsing_distinguishes_number_row_and_numpad() {
        assert_eq!(parse_hotkey("Control+Alt+1").unwrap().key, 0x31);
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Num1").unwrap().key,
            VK_NUMPAD1 as u32
        );
        assert_eq!(parse_hotkey("Ctrl+Shift+F24").unwrap().key, VK_F24 as u32);
        assert!(parse_hotkey("Ctrl+1+2").is_err());
        assert!(parse_hotkey("1").is_err());
        assert!(parse_hotkey("Ctrl+").is_err());
    }

    #[test]
    fn windows_arguments_escape_trailing_backslashes_and_spaces() {
        assert_eq!(
            quote_windows_arg(std::ffi::OsStr::new("C:\\")),
            "\"C:\\\\\""
        );
        assert_eq!(
            quote_windows_arg(std::ffi::OsStr::new("C:\\한글 폴더")),
            "\"C:\\한글 폴더\""
        );
    }

    #[test]
    fn shell_delivers_sent_and_posted_callbacks_without_keyboard_input() {
        // An invalid shortcut deliberately avoids claiming any global key;
        // tray activation must work even when hotkey registration failed.
        let shell = Shell::new_native(0, 0, "").unwrap();
        let hwnd = shell.hwnd as HWND;
        let send = |message, parameter| unsafe {
            let mut result = 0;
            assert_ne!(
                SendMessageTimeoutW(
                    hwnd,
                    message,
                    0,
                    parameter,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    2_000,
                    &mut result,
                ),
                0,
                "shell window did not dispatch a sent message"
            );
        };
        // Version 4 puts the icon ID in the high word of lParam.
        send(CALLBACK, (NIN_SELECT | (1 << 16)) as isize);
        assert_eq!(
            shell.events.recv_timeout(Duration::from_secs(2)).unwrap(),
            ShellEvent::Show
        );
        unsafe {
            assert_ne!(
                PostMessageW(hwnd, CALLBACK, 0, (TRAY_KEYSELECT | (1 << 16)) as isize),
                0
            );
        }
        assert_eq!(
            shell.events.recv_timeout(Duration::from_secs(2)).unwrap(),
            ShellEvent::Show
        );
        send(SHOW_EXISTING, 0);
        assert_eq!(
            shell.events.recv_timeout(Duration::from_secs(2)).unwrap(),
            ShellEvent::Show
        );
        // A sent close must also wake and finish the normal message loop.
        send(WM_CLOSE, 0);
        assert!(matches!(
            shell.events.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        drop(shell);
    }

    #[test]
    fn shell_registers_changes_and_releases_hotkeys_without_sending_input() {
        let hk = parse_hotkey("Ctrl+Alt+F23").unwrap();
        let mut shell = Shell::new_native(0, 0, "Ctrl+Alt+F23").unwrap();
        assert!(shell.hotkey_error.is_none(), "unexpected hotkey collision");
        shell.pause_hotkey().unwrap();
        unsafe {
            assert_ne!(
                RegisterHotKey(std::ptr::null_mut(), 4399, hk.modifiers, hk.key),
                0
            );
            UnregisterHotKey(std::ptr::null_mut(), 4399);
        }
        shell.set_hotkey("Ctrl+Alt+F23").unwrap();
        unsafe {
            assert_eq!(
                RegisterHotKey(std::ptr::null_mut(), 4400, hk.modifiers, hk.key),
                0
            );
        }
        shell.set_hotkey("Ctrl+Alt+F22").unwrap();
        unsafe {
            assert_ne!(
                RegisterHotKey(std::ptr::null_mut(), 4400, hk.modifiers, hk.key),
                0
            );
            // Trying to claim an occupied shortcut must keep the previous one.
            assert!(shell.set_hotkey("Ctrl+Alt+F23").is_err());
            UnregisterHotKey(std::ptr::null_mut(), 4400);
        }
        drop(shell);
        let replacement = parse_hotkey("Ctrl+Alt+F22").unwrap();
        unsafe {
            assert_ne!(
                RegisterHotKey(
                    std::ptr::null_mut(),
                    4401,
                    replacement.modifiers,
                    replacement.key
                ),
                0
            );
            UnregisterHotKey(std::ptr::null_mut(), 4401);
        }
    }
    #[test]
    fn captured_keys_roundtrip_and_distinguish_top_row_from_numpad() {
        for key in [0x31, 0x61, 0x41, 0x70, 0x87] {
            let text = captured_hotkey(key, true, true, false, false).unwrap();
            assert_eq!(parse_hotkey(&text).unwrap().key, key);
        }
        assert_eq!(
            captured_hotkey(0x31, true, true, false, false).unwrap(),
            "Ctrl+Alt+1"
        );
        assert_eq!(
            captured_hotkey(0x61, true, true, false, false).unwrap(),
            "Ctrl+Alt+Num1"
        );
        assert!(captured_hotkey(0x31, false, false, false, false).is_none());
        assert!(captured_hotkey(VK_CONTROL as u32, true, false, false, false).is_none());
    }
}
