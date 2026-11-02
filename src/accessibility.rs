//! The stock Win32 list-view UIA proxy enumerates every owner-data row.
//! Expose only the viewport in the tree; ItemContainer/VirtualizedItem retain
//! access to every result without materializing millions of UI elements.
#![allow(non_snake_case, non_upper_case_globals)]
use crate::engine::SearchEngine;
use std::{
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, E_OUTOFMEMORY, HWND, LPARAM, WPARAM},
        System::{
            Com::SAFEARRAY,
            Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement},
            Variant::{VARIANT, VT_I4, VT_UNKNOWN},
        },
        UI::Accessibility::*,
    },
    core::{BOOL, BSTR, Error, HRESULT, IUnknown, IUnknownImpl, Interface, Ref, Result, implement},
};
use windows_sys::Win32::{
    Foundation::RECT,
    UI::{Controls::*, Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::*},
};

pub(crate) const SNAPSHOT: u32 = WM_APP + 61;
pub(crate) const ACTION: u32 = WM_APP + 62;
pub(crate) const SELECTION: u32 = WM_APP + 63;
#[derive(Clone)]
pub(crate) struct View {
    pub engine: Arc<SearchEngine>,
    pub rows: Arc<Vec<u32>>,
    pub epoch: u64,
    pub top: usize,
    pub visible: usize,
    pub rect: RECT,
    pub row_height: i32,
    pub first_row_y: i32,
    pub focused: Option<usize>,
    pub has_focus: bool,
    pub horizontal: (i32, i32, u32), // position, range, viewport in pixels
}
impl View {
    fn end(&self) -> usize {
        (self.top + self.visible).min(self.rows.len())
    }
    fn visible(&self, row: usize) -> bool {
        (self.top..self.end()).contains(&row)
    }
}
pub(crate) struct Action {
    pub epoch: u64,
    pub row: usize,
    pub kind: ActionKind,
    pub accepted: bool,
}
pub(crate) enum ActionKind {
    Realize,
    ScrollTop,
    ScrollHorizontal,
    Focus,
    Select,
    Add,
    Remove,
}
pub(crate) struct Selection {
    pub epoch: u64,
    pub rows: Vec<usize>,
    pub valid: bool,
}
struct Endpoint {
    hwnd: usize,
    alive: AtomicBool,
}
impl Endpoint {
    fn view(&self) -> Result<View> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let mut view: Option<View> = None;
        // In-process message, handled in constant time on the window thread.
        unsafe {
            SendMessageW(self.hwnd as _, SNAPSHOT, 0, &mut view as *mut _ as isize);
        }
        view.ok_or_else(unavailable)
    }
    fn action(&self, view: &View, row: usize, kind: ActionKind) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let mut action = Action {
            epoch: view.epoch,
            row,
            kind,
            accepted: false,
        };
        unsafe {
            SendMessageW(self.hwnd as _, ACTION, 0, &mut action as *mut _ as isize);
        }
        if action.accepted {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
    fn item(self: &Arc<Self>, view: &View, row: usize) -> IRawElementProviderSimple {
        Node {
            endpoint: self.clone(),
            item: Some((view.epoch, row)),
        }
        .into()
    }
    fn root(self: &Arc<Self>) -> IRawElementProviderSimple {
        Node {
            endpoint: self.clone(),
            item: None,
        }
        .into()
    }
    fn selection(&self, view: &View) -> Result<Vec<usize>> {
        let mut selection = Selection {
            epoch: view.epoch,
            rows: Vec::new(),
            valid: false,
        };
        unsafe {
            SendMessageW(
                self.hwnd as _,
                SELECTION,
                0,
                &mut selection as *mut _ as isize,
            );
        }
        if selection.valid {
            Ok(selection.rows)
        } else {
            Err(Error::from_hresult(HRESULT(UIA_E_INVALIDOPERATION as i32)))
        }
    }
}
fn unavailable() -> Error {
    Error::from_hresult(HRESULT(UIA_E_ELEMENTNOTAVAILABLE as i32))
}
// windows-rs projects nullable COM interface outputs as Result<Interface>.
// S_OK with the caller's initialized null output is the COM "no element" value.
fn absent<T>() -> Result<T> {
    Err(Error::empty())
}

pub(crate) struct Accessibility {
    endpoint: Arc<Endpoint>,
    provider: IRawElementProviderSimple,
}
impl Accessibility {
    pub fn new(hwnd: usize) -> Self {
        let endpoint = Arc::new(Endpoint {
            hwnd,
            alive: AtomicBool::new(true),
        });
        let provider = endpoint.root();
        Self { endpoint, provider }
    }
    pub unsafe fn get_object(&self, w: usize, l: isize) -> Option<isize> {
        if l as i32 != UiaRootObjectId {
            return None;
        }
        Some(
            unsafe {
                UiaReturnRawElementProvider(
                    HWND(self.endpoint.hwnd as _),
                    WPARAM(w),
                    LPARAM(l),
                    &self.provider,
                )
            }
            .0,
        )
    }
    pub fn disconnect(&self) {
        self.endpoint.alive.store(false, Ordering::Release);
        unsafe {
            let _ = UiaDisconnectProvider(&self.provider);
        }
    }
    pub fn notify_changed(&self) {
        if !self.endpoint.alive.load(Ordering::Acquire)
            || !unsafe { UiaClientsAreListening() }.as_bool()
        {
            return;
        }
        unsafe {
            let _ = UiaRaiseStructureChangedEvent(
                &self.provider,
                StructureChangeType_ChildrenInvalidated,
                null_mut(),
                0,
            );
            let _ = UiaRaiseAutomationEvent(&self.provider, UIA_Selection_InvalidatedEventId);
            if let Ok(view) = self.endpoint.view() {
                if view.has_focus {
                    if let Some(row) = view.focused.filter(|&r| r < view.rows.len()) {
                        let _ = UiaRaiseAutomationEvent(
                            &self.endpoint.item(&view, row),
                            UIA_AutomationFocusChangedEventId,
                        );
                    }
                }
            }
        }
    }
}
impl Drop for Accessibility {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot,
    IItemContainerProvider,
    IVirtualizedItemProvider,
    IScrollItemProvider,
    IScrollProvider,
    ISelectionItemProvider,
    ISelectionProvider
)]
struct Node {
    endpoint: Arc<Endpoint>,
    item: Option<(u64, usize)>,
}
impl Node_Impl {
    fn view(&self) -> Result<View> {
        let view = self.endpoint.view()?;
        if self
            .item
            .is_some_and(|(epoch, row)| epoch != view.epoch || row >= view.rows.len())
        {
            return Err(unavailable());
        }
        Ok(view)
    }
    fn row(&self) -> Result<usize> {
        self.item
            .map(|(_, row)| row)
            .ok_or_else(|| E_INVALIDARG.into())
    }
    fn action(&self, kind: ActionKind) -> Result<()> {
        let view = self.view()?;
        self.endpoint.action(&view, self.row()?, kind)
    }
}
impl IRawElementProviderSimple_Impl for Node_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_ProviderOwnsSetFocus)
    }
    fn GetPatternProvider(&self, id: UIA_PATTERN_ID) -> Result<IUnknown> {
        if self.item.is_none() {
            if id == UIA_ItemContainerPatternId {
                return self.to_interface::<IItemContainerProvider>().cast();
            }
            if id == UIA_ScrollPatternId {
                return self.to_interface::<IScrollProvider>().cast();
            }
            if id == UIA_SelectionPatternId {
                return self.to_interface::<ISelectionProvider>().cast();
            }
        } else {
            if id == UIA_VirtualizedItemPatternId {
                return self.to_interface::<IVirtualizedItemProvider>().cast();
            }
            if id == UIA_ScrollItemPatternId {
                return self.to_interface::<IScrollItemProvider>().cast();
            }
            if id == UIA_SelectionItemPatternId {
                return self.to_interface::<ISelectionItemProvider>().cast();
            }
        }
        absent()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let view = self.view()?;
        let row = self.item.map(|(_, r)| r);
        Ok(match id {
            UIA_ControlTypePropertyId => if row.is_some() {
                UIA_ListItemControlTypeId.0
            } else {
                UIA_ListControlTypeId.0
            }
            .into(),
            UIA_NamePropertyId => match row {
                Some(r) => view.engine.index().entry(view.rows[r] as usize).name.into(),
                None => format!("검색 결과 {}개", view.rows.len()).as_str().into(),
            },
            UIA_HelpTextPropertyId => match row {
                Some(r) => {
                    let index = view.engine.index();
                    let entry = index.entry(view.rows[r] as usize);
                    format!(
                        "{} · 크기 {} · 생성일 {} · 수정일 {}",
                        index.parent_path(view.rows[r] as usize),
                        crate::native_app::format_size(entry.size),
                        crate::native_app::format_date(entry.created),
                        crate::native_app::format_date(entry.modified)
                    )
                    .as_str()
                    .into()
                }
                None => "방향키로 이동하고 검색어로 결과를 좁힐 수 있습니다.".into(),
            },
            UIA_AutomationIdPropertyId => row
                .map_or_else(
                    || "search-results".into(),
                    |r| format!("result-{}-{r}", view.epoch),
                )
                .as_str()
                .into(),
            UIA_IsControlElementPropertyId
            | UIA_IsContentElementPropertyId
            | UIA_IsEnabledPropertyId
            | UIA_IsKeyboardFocusablePropertyId => true.into(),
            UIA_IsOffscreenPropertyId => row.is_some_and(|r| !view.visible(r)).into(),
            UIA_HasKeyboardFocusPropertyId => (view.has_focus && row == view.focused).into(),
            UIA_SizeOfSetPropertyId => (view.rows.len().min(i32::MAX as usize) as i32).into(),
            UIA_PositionInSetPropertyId => row.map_or(0, |r| r as i32 + 1).into(),
            UIA_FrameworkIdPropertyId => "Hyperfinder".into(),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        if self.item.is_some() {
            return absent();
        }
        unsafe { UiaHostProviderFromHwnd(HWND(self.endpoint.hwnd as _)) }
    }
}
impl IRawElementProviderFragment_Impl for Node_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let view = self.view()?;
        let target = match (self.item, direction) {
            (None, NavigateDirection_FirstChild) if view.top < view.end() => Some(view.top),
            (None, NavigateDirection_LastChild) if view.top < view.end() => Some(view.end() - 1),
            (Some(_), NavigateDirection_Parent) => return self.endpoint.root().cast(),
            (Some((_, r)), NavigateDirection_NextSibling)
                if view.visible(r) && r + 1 < view.end() =>
            {
                Some(r + 1)
            }
            (Some((_, r)), NavigateDirection_PreviousSibling)
                if view.visible(r) && r > view.top =>
            {
                Some(r - 1)
            }
            _ => None,
        };
        match target {
            Some(r) => self.endpoint.item(&view, r).cast(),
            None => absent(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        let Some((epoch, row)) = self.item else {
            return Ok(null_mut());
        };
        let ids = [
            UiaAppendRuntimeId as i32,
            epoch as i32,
            (epoch >> 32) as i32,
            row as i32,
        ];
        unsafe {
            let array = SafeArrayCreateVector(VT_I4, 0, ids.len() as u32);
            if array.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            for (i, id) in ids.iter().enumerate() {
                if let Err(e) = SafeArrayPutElement(array, &(i as i32), id as *const _ as _) {
                    let _ = SafeArrayDestroy(array);
                    return Err(e);
                }
            }
            Ok(array)
        }
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let view = self.view()?;
        let (top, height) = match self.item {
            Some((_, r)) if view.visible(r) => (
                view.first_row_y + (r - view.top) as i32 * view.row_height,
                view.row_height,
            ),
            Some(_) => return Ok(UiaRect::default()),
            None => (view.rect.top, view.rect.bottom - view.rect.top),
        };
        Ok(UiaRect {
            left: view.rect.left as f64,
            top: top as f64,
            width: (view.rect.right - view.rect.left).max(0) as f64,
            height: height.max(0) as f64,
        })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        if self.item.is_some() {
            self.action(ActionKind::Focus)
        } else {
            let view = self.view()?;
            self.endpoint
                .action(&view, view.focused.unwrap_or(view.top), ActionKind::Focus)
        }
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        self.endpoint.root().cast()
    }
}
impl IRawElementProviderFragmentRoot_Impl for Node_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let view = self.view()?;
        if x >= view.rect.left as f64
            && x < view.rect.right as f64
            && y >= view.first_row_y as f64
            && y < view.rect.bottom as f64
        {
            let row =
                view.top + ((y - view.first_row_y as f64) / view.row_height.max(1) as f64) as usize;
            if view.visible(row) {
                return self.endpoint.item(&view, row).cast();
            }
        }
        self.endpoint.root().cast()
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let view = self.view()?;
        match view.focused.filter(|&r| r < view.rows.len()) {
            Some(r) => self.endpoint.item(&view, r).cast(),
            None => absent(),
        }
    }
}
impl IItemContainerProvider_Impl for Node_Impl {
    fn FindItemByProperty(
        &self,
        after: Ref<'_, IRawElementProviderSimple>,
        id: UIA_PROPERTY_ID,
        value: &VARIANT,
    ) -> Result<IRawElementProviderSimple> {
        let view = self.view()?;
        let prefix = format!("result-{}-", view.epoch);
        let start = if let Some(after) = after.as_ref() {
            let key = unsafe { after.GetPropertyValue(UIA_AutomationIdPropertyId) }?;
            let key = BSTR::try_from(&key)?.to_string();
            key.strip_prefix(&prefix)
                .and_then(|r| r.parse::<usize>().ok())
                .ok_or_else(unavailable)?
                .saturating_add(1)
        } else {
            0
        };
        let row = if id.0 == 0 {
            (start < view.rows.len()).then_some(start)
        } else if id == UIA_AutomationIdPropertyId {
            BSTR::try_from(value)?
                .to_string()
                .strip_prefix(&prefix)
                .and_then(|r| r.parse::<usize>().ok())
                .filter(|&r| r >= start && r < view.rows.len())
        } else if id == UIA_NamePropertyId {
            let name = BSTR::try_from(value)?.to_string();
            (start..view.rows.len())
                .find(|&r| view.engine.index().entry(view.rows[r] as usize).name == name)
        } else if id == UIA_SelectionItemIsSelectedPropertyId {
            let selected = self.endpoint.selection(&view)?;
            if bool::try_from(value)? {
                selected.into_iter().find(|&r| r >= start)
            } else {
                let selected: std::collections::HashSet<_> = selected.into_iter().collect();
                (start..view.rows.len()).find(|r| !selected.contains(r))
            }
        } else {
            return Err(E_INVALIDARG.into());
        };
        match row {
            Some(r) => Ok(self.endpoint.item(&view, r)),
            None => absent(),
        }
    }
}
impl IVirtualizedItemProvider_Impl for Node_Impl {
    fn Realize(&self) -> Result<()> {
        self.action(ActionKind::Realize)
    }
}
impl IScrollItemProvider_Impl for Node_Impl {
    fn ScrollIntoView(&self) -> Result<()> {
        self.action(ActionKind::Realize)
    }
}
impl ISelectionItemProvider_Impl for Node_Impl {
    fn Select(&self) -> Result<()> {
        self.action(ActionKind::Select)
    }
    fn AddToSelection(&self) -> Result<()> {
        self.action(ActionKind::Add)
    }
    fn RemoveFromSelection(&self) -> Result<()> {
        self.action(ActionKind::Remove)
    }
    fn IsSelected(&self) -> Result<BOOL> {
        self.view()?;
        Ok(BOOL(unsafe {
            SendMessageW(
                self.endpoint.hwnd as _,
                LVM_GETITEMSTATE,
                self.row()?,
                LVIS_SELECTED as isize,
            )
        } as i32))
    }
    fn SelectionContainer(&self) -> Result<IRawElementProviderSimple> {
        Ok(self.endpoint.root())
    }
}
impl ISelectionProvider_Impl for Node_Impl {
    fn GetSelection(&self) -> Result<*mut SAFEARRAY> {
        let view = self.view()?;
        let selected = self.endpoint.selection(&view)?;
        unsafe {
            let array = SafeArrayCreateVector(VT_UNKNOWN, 0, selected.len() as u32);
            if array.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            for (i, row) in selected.into_iter().enumerate() {
                let item = self.endpoint.item(&view, row);
                if let Err(e) = SafeArrayPutElement(array, &(i as i32), item.as_raw()) {
                    let _ = SafeArrayDestroy(array);
                    return Err(e);
                }
            }
            Ok(array)
        }
    }
    fn CanSelectMultiple(&self) -> Result<BOOL> {
        Ok(BOOL(1))
    }
    fn IsSelectionRequired(&self) -> Result<BOOL> {
        Ok(BOOL(0))
    }
}
impl IScrollProvider_Impl for Node_Impl {
    fn Scroll(&self, horizontal: ScrollAmount, vertical: ScrollAmount) -> Result<()> {
        let v = self.view()?;
        if horizontal != ScrollAmount_NoAmount {
            let delta = match horizontal {
                ScrollAmount_SmallIncrement => 24,
                ScrollAmount_SmallDecrement => -24,
                ScrollAmount_LargeIncrement => v.horizontal.2 as i32,
                ScrollAmount_LargeDecrement => -(v.horizontal.2 as i32),
                _ => return Err(E_INVALIDARG.into()),
            };
            self.endpoint.action(
                &v,
                (v.horizontal.0 + delta).clamp(0, (v.horizontal.1 - v.horizontal.2 as i32).max(0))
                    as usize,
                ActionKind::ScrollHorizontal,
            )?;
        }
        if vertical == ScrollAmount_NoAmount || v.rows.is_empty() {
            return Ok(());
        }
        let delta = match vertical {
            ScrollAmount_NoAmount => 0,
            ScrollAmount_SmallIncrement => 1,
            ScrollAmount_SmallDecrement => -1,
            ScrollAmount_LargeIncrement => v.visible as i64,
            ScrollAmount_LargeDecrement => -(v.visible as i64),
            _ => return Err(E_INVALIDARG.into()),
        };
        let row = (v.top as i64 + delta).clamp(0, v.rows.len().saturating_sub(1) as i64) as usize;
        self.endpoint.action(&v, row, ActionKind::ScrollTop)
    }
    fn SetScrollPercent(&self, horizontal: f64, vertical: f64) -> Result<()> {
        if [horizontal, vertical]
            .into_iter()
            .any(|p| p != -1.0 && !(0.0..=100.0).contains(&p))
        {
            return Err(E_INVALIDARG.into());
        }
        let view = self.view()?;
        if horizontal != -1.0 {
            self.endpoint.action(
                &view,
                (horizontal / 100.0 * (view.horizontal.1 - view.horizontal.2 as i32).max(0) as f64)
                    .round() as usize,
                ActionKind::ScrollHorizontal,
            )?;
        }
        if vertical == -1.0 || view.rows.is_empty() {
            return Ok(());
        }
        self.endpoint.action(
            &view,
            (vertical / 100.0 * view.rows.len().saturating_sub(view.visible) as f64).round()
                as usize,
            ActionKind::ScrollTop,
        )
    }
    fn HorizontalScrollPercent(&self) -> Result<f64> {
        let v = self.view()?;
        let max = v.horizontal.1 - v.horizontal.2 as i32;
        Ok(if max <= 0 {
            -1.0
        } else {
            100.0 * v.horizontal.0 as f64 / max as f64
        })
    }
    fn VerticalScrollPercent(&self) -> Result<f64> {
        let v = self.view()?;
        Ok(if v.rows.len() <= v.visible {
            -1.0
        } else {
            (100.0 * v.top as f64 / (v.rows.len() - v.visible) as f64).min(100.0)
        })
    }
    fn HorizontalViewSize(&self) -> Result<f64> {
        let v = self.view()?;
        Ok(if v.horizontal.1 <= 0 {
            100.0
        } else {
            (100.0 * v.horizontal.2 as f64 / v.horizontal.1 as f64).min(100.0)
        })
    }
    fn VerticalViewSize(&self) -> Result<f64> {
        let v = self.view()?;
        Ok(if v.rows.is_empty() {
            100.0
        } else {
            (100.0 * v.visible as f64 / v.rows.len() as f64).min(100.0)
        })
    }
    fn HorizontallyScrollable(&self) -> Result<BOOL> {
        let v = self.view()?;
        Ok(BOOL((v.horizontal.1 > v.horizontal.2 as i32) as i32))
    }
    fn VerticallyScrollable(&self) -> Result<BOOL> {
        let v = self.view()?;
        Ok(BOOL((v.rows.len() > v.visible) as i32))
    }
}

pub(crate) unsafe fn perform(
    hwnd: windows_sys::Win32::Foundation::HWND,
    action: &mut Action,
    epoch: u64,
    count: usize,
) {
    if action.epoch != epoch {
        return;
    }
    if matches!(action.kind, ActionKind::ScrollHorizontal) {
        unsafe {
            let mut info: SCROLLINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
            info.fMask = SIF_POS;
            GetScrollInfo(hwnd, SB_HORZ, &mut info);
            SendMessageW(
                hwnd,
                LVM_SCROLL,
                (action.row as i64 - info.nPos as i64).clamp(i32::MIN as i64, i32::MAX as i64)
                    as isize as usize,
                0,
            );
        }
        action.accepted = true;
        return;
    }
    if count == 0 && matches!(action.kind, ActionKind::Focus) {
        unsafe {
            SetFocus(hwnd);
        }
        action.accepted = true;
        return;
    }
    if action.row >= count {
        return;
    }
    unsafe {
        match action.kind {
            ActionKind::ScrollHorizontal => unreachable!(),
            ActionKind::Realize => {
                SendMessageW(hwnd, LVM_ENSUREVISIBLE, action.row, 0);
            }
            ActionKind::ScrollTop => {
                SendMessageW(hwnd, LVM_ENSUREVISIBLE, action.row, 0);
                let top = SendMessageW(hwnd, LVM_GETTOPINDEX, 0, 0).max(0);
                let mut rect: RECT = std::mem::zeroed();
                rect.left = LVIR_BOUNDS as i32;
                if SendMessageW(
                    hwnd,
                    LVM_GETITEMRECT,
                    top as usize,
                    &mut rect as *mut _ as isize,
                ) != 0
                {
                    let delta = ((action.row as i64 - top as i64)
                        * (rect.bottom - rect.top).max(1) as i64)
                        .clamp(i32::MIN as i64, i32::MAX as i64);
                    SendMessageW(hwnd, LVM_SCROLL, 0, delta as isize);
                }
            }
            ActionKind::Focus => {
                SetFocus(hwnd);
                SendMessageW(hwnd, LVM_ENSUREVISIBLE, action.row, 0);
                let state = LVITEMW {
                    stateMask: LVIS_FOCUSED,
                    state: LVIS_FOCUSED,
                    ..std::mem::zeroed()
                };
                SendMessageW(
                    hwnd,
                    LVM_SETITEMSTATE,
                    action.row,
                    &state as *const _ as isize,
                );
            }
            ActionKind::Select | ActionKind::Add | ActionKind::Remove => {
                let state = LVITEMW {
                    stateMask: LVIS_SELECTED,
                    state: if matches!(action.kind, ActionKind::Remove) {
                        0
                    } else {
                        LVIS_SELECTED
                    },
                    ..std::mem::zeroed()
                };
                if matches!(action.kind, ActionKind::Select) {
                    let clear = LVITEMW {
                        stateMask: LVIS_SELECTED,
                        ..std::mem::zeroed()
                    };
                    SendMessageW(
                        hwnd,
                        LVM_SETITEMSTATE,
                        usize::MAX,
                        &clear as *const _ as isize,
                    );
                }
                SendMessageW(
                    hwnd,
                    LVM_SETITEMSTATE,
                    action.row,
                    &state as *const _ as isize,
                );
            }
        }
        action.accepted = true;
    }
}
