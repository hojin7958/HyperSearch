//! Explorer's registered shell menu, hosted on the application's STA thread.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{cell::RefCell, mem::size_of, path::Path, ptr::null_mut};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::Com::CoTaskMemFree,
        UI::{
            Shell::{
                CMF_CANRENAME, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMIC_MASK_CONTROL_DOWN,
                CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO,
                CMINVOKECOMMANDINFOEX, Common::ITEMIDLIST, GCS_VERBA, GCS_VERBW, IContextMenu,
                IContextMenu2, IContextMenu3, IShellFolder, SEE_MASK_NOASYNC, SEE_MASK_UNICODE,
                SHBindToParent, SHParseDisplayName,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MF_SEPARATOR, MF_STRING,
                PostMessageW, SW_SHOWNORMAL, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
                WM_NULL,
            },
        },
    },
    core::{Interface, PCSTR, PCWSTR, PSTR},
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};

const FIRST_SHELL: u32 = 1;
const LAST_SHELL: u32 = 0x7fff;
const COPY_FULL_PATH: usize = 0x8000;
const OPEN_LOCATION: usize = 0x8001;
const COPY_FILES: usize = 0x8002;
const CUT_FILES: usize = 0x8003;
const DELETE_FILES: usize = 0x8004;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Cancelled,
    Invoked,
    CopyPath,
    OpenLocation,
    /// Explorer normally delegates this verb to its view. The host edits its
    /// list label instead, using a non-overwriting filesystem rename.
    Rename,
    CopyFiles,
    CutFiles,
    DeleteFiles,
}

#[derive(Clone)]
struct Forwarder {
    v3: Option<IContextMenu3>,
    v2: Option<IContextMenu2>,
}
thread_local! { static ACTIVE: RefCell<Option<Forwarder>> = const { RefCell::new(None) }; }

struct ActiveMenu(Option<Forwarder>);
impl ActiveMenu {
    fn set(menu: &IContextMenu) -> Self {
        let current = Forwarder {
            v3: menu.cast().ok(),
            v2: menu.cast().ok(),
        };
        Self(ACTIVE.with(|active| active.replace(Some(current))))
    }
}
impl Drop for ActiveMenu {
    fn drop(&mut self) {
        let previous = self.0.take();
        ACTIVE.with(|active| {
            active.replace(previous);
        });
    }
}
struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}
struct Popup(HMENU);
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

/// Call before the host window handles these messages itself. Shell extensions
/// use them to populate lazy submenus, draw menu items and resolve menu keys.
pub fn handle_message(message: u32, w: usize, l: isize) -> Option<isize> {
    if !matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        return None;
    }
    // Nonzero IDs identify owner-drawn controls rather than menu items.
    if matches!(message, WM_DRAWITEM | WM_MEASUREITEM) && w != 0 {
        return None;
    }
    let forward = ACTIVE.with(|active| active.borrow().clone())?;
    unsafe {
        if let Some(menu) = forward.v3 {
            let mut result = LRESULT(0);
            if menu
                .HandleMenuMsg2(message, WPARAM(w), LPARAM(l), Some(&mut result))
                .is_ok()
            {
                return Some(result.0);
            }
        }
        if message != WM_MENUCHAR {
            if let Some(menu) = forward.v2 {
                if menu.HandleMenuMsg(message, WPARAM(w), LPARAM(l)).is_ok() {
                    return Some(if matches!(message, WM_DRAWITEM | WM_MEASUREITEM) {
                        1
                    } else {
                        0
                    });
                }
            }
        }
    }
    None
}

/// Displays the same registered classic shell menu used by Explorer, including
/// third-party extensions. No verb runs unless the user selects its menu item.
pub fn show(owner: isize, path: &str, x: i32, y: i32) -> Result<MenuAction, String> {
    show_many(owner, &[path.to_owned()], x, y)
}
pub fn show_many(owner: isize, paths: &[String], x: i32, y: i32) -> Result<MenuAction, String> {
    if paths.is_empty() {
        return Ok(MenuAction::Cancelled);
    }
    unsafe {
        show_inner(HWND(owner as _), paths, x, y).map_err(|error| format!("파일 메뉴: {error}"))
    }
}

unsafe fn show_inner(
    owner: HWND,
    paths: &[String],
    x: i32,
    y: i32,
) -> windows::core::Result<MenuAction> {
    let path = &paths[0];
    let parent_key = |path: &str| {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_lowercase())
    };
    let flat = paths.iter().all(|p| parent_key(p) == parent_key(path));
    let mut pidls = Vec::new();
    let mut children = Vec::new();
    let mut folder: Option<IShellFolder> = None;
    if flat {
        for path in paths {
            let path_wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let mut pidl = Pidl(null_mut());
            SHParseDisplayName(PCWSTR(path_wide.as_ptr()), None, &mut pidl.0, 0, None)?;
            let mut child = null_mut();
            let parent: IShellFolder = SHBindToParent(pidl.0, Some(&mut child))?;
            folder.get_or_insert(parent);
            children.push(child.cast_const());
            pidls.push(pidl);
        }
    }
    let context: Option<IContextMenu> = folder
        .map(|parent| parent.GetUIObjectOf(owner, &children, None))
        .transpose()?;
    let menu = Popup(CreatePopupMenu()?);
    let location_text: Vec<u16> = "경로 열기 (파일 탐색기)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    AppendMenuW(
        menu.0,
        MF_STRING,
        OPEN_LOCATION,
        PCWSTR(location_text.as_ptr()),
    )?;
    AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
    let shifted = GetKeyState(VK_SHIFT as i32) < 0;
    let flags = CMF_CANRENAME | CMF_EXPLORE | if shifted { CMF_EXTENDEDVERBS } else { 0 };
    if let Some(context) = &context {
        context
            .QueryContextMenu(menu.0, 2, FIRST_SHELL, LAST_SHELL, flags)
            .ok()?;
    } else {
        for (id, title) in [
            (COPY_FILES, "복사"),
            (CUT_FILES, "잘라내기"),
            (DELETE_FILES, "삭제…"),
        ] {
            let text: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            AppendMenuW(menu.0, MF_STRING, id, PCWSTR(text.as_ptr()))?;
        }
    }
    let copy_text: Vec<u16> = "전체 경로 복사".encode_utf16().chain(Some(0)).collect();
    AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
    AppendMenuW(
        menu.0,
        MF_STRING,
        COPY_FULL_PATH,
        PCWSTR(copy_text.as_ptr()),
    )?;
    let _active = context.as_ref().map(ActiveMenu::set);
    let _ = SetForegroundWindow(owner);
    let command = TrackPopupMenuEx(
        menu.0,
        (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
        x,
        y,
        owner,
        None,
    )
    .0 as u32;
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    if command == 0 {
        return Ok(MenuAction::Cancelled);
    }
    if command == COPY_FULL_PATH as u32 {
        return Ok(MenuAction::CopyPath);
    }
    if command == OPEN_LOCATION as u32 {
        return Ok(MenuAction::OpenLocation);
    }
    match command as usize {
        COPY_FILES => return Ok(MenuAction::CopyFiles),
        CUT_FILES => return Ok(MenuAction::CutFiles),
        DELETE_FILES => return Ok(MenuAction::DeleteFiles),
        _ => {}
    }
    if !(FIRST_SHELL..=LAST_SHELL).contains(&command) {
        return Ok(MenuAction::Cancelled);
    }
    let ordinal = (command - FIRST_SHELL) as usize;
    let Some(context) = context else {
        return Ok(MenuAction::Cancelled);
    };
    if canonical_verb(&context, ordinal).eq_ignore_ascii_case("rename") {
        return Ok(MenuAction::Rename);
    }
    let directory: Vec<u16> = Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut mask = SEE_MASK_UNICODE | SEE_MASK_NOASYNC | CMIC_MASK_PTINVOKE;
    if GetKeyState(VK_SHIFT as i32) < 0 {
        mask |= CMIC_MASK_SHIFT_DOWN;
    }
    if GetKeyState(VK_CONTROL as i32) < 0 {
        mask |= CMIC_MASK_CONTROL_DOWN;
    }
    let invoke = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: mask,
        hwnd: owner,
        lpVerb: PCSTR(ordinal as *const u8),
        lpVerbW: PCWSTR(ordinal as *const u16),
        lpDirectoryW: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x, y },
        ..Default::default()
    };
    context
        .InvokeCommand((&invoke as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())?;
    Ok(MenuAction::Invoked)
}

unsafe fn canonical_verb(menu: &IContextMenu, ordinal: usize) -> String {
    let mut unicode = [0u16; 256];
    if menu
        .GetCommandString(
            ordinal,
            GCS_VERBW,
            None,
            PSTR(unicode.as_mut_ptr().cast()),
            unicode.len() as u32,
        )
        .is_ok()
    {
        let length = unicode
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(unicode.len());
        if length > 0 {
            return String::from_utf16_lossy(&unicode[..length]);
        }
    }
    let mut ansi = [0u8; 256];
    if menu
        .GetCommandString(
            ordinal,
            GCS_VERBA,
            None,
            PSTR(ansi.as_mut_ptr()),
            ansi.len() as u32,
        )
        .is_ok()
    {
        let length = ansi.iter().position(|&ch| ch == 0).unwrap_or(ansi.len());
        return String::from_utf8_lossy(&ansi[..length]).into_owned();
    }
    String::new()
}
