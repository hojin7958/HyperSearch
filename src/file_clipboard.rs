//! Explorer-compatible file clipboard and drag source. File contents are never buffered here.
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{E_POINTER, FILETIME, HGLOBAL, HWND},
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, WIN32_FIND_DATAW},
        System::{
            Com::{
                BIND_OPTS, CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, CreateBindCtx,
                DVASPECT_CONTENT, FORMATETC, IDataObject, STGM_CREATE, STGMEDIUM, STGMEDIUM_0,
                TYMED_HGLOBAL,
            },
            Ole::{
                DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
                OleFlushClipboard, OleGetClipboard, OleInitialize, OleSetClipboard,
                OleUninitialize, ReleaseStgMedium,
            },
        },
        UI::Shell::{
            CFSTR_PASTESUCCEEDED, CFSTR_PERFORMEDDROPEFFECT, CFSTR_PREFERREDDROPEFFECT,
            Common::ITEMIDLIST, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOF_RENAMEONCOLLISION,
            FileOperation, IFileOperation, IFileSystemBindData, IFileSystemBindData_Impl,
            ILGetSize, IShellItem, IShellItemArray, SHCreateDataObject,
            SHCreateItemFromParsingName, SHCreateShellItemArrayFromDataObject, SHDoDragDrop,
            SHParseDisplayName, SIGDN_FILESYSPATH,
        },
    },
    core::{PCWSTR, implement, w},
};
use windows_sys::Win32::{
    Foundation::GlobalFree,
    System::{
        DataExchange::{
            GetClipboardSequenceNumber, IsClipboardFormatAvailable, RegisterClipboardFormatW,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
    },
};

const COPY: u32 = 1;
const MOVE: u32 = 2;
const MAX_PATHS: u32 = 4096;
const MAX_PATH_BYTES: usize = 2 * 1024 * 1024;

/// File information already displayed in the index; no disk access is needed
/// to turn a selected row into a Shell drag source.
pub struct DragFile {
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

#[implement(IFileSystemBindData)]
struct IndexedFileData(RefCell<WIN32_FIND_DATAW>);
impl IFileSystemBindData_Impl for IndexedFileData_Impl {
    fn SetFindData(&self, data: *const WIN32_FIND_DATAW) -> windows::core::Result<()> {
        if data.is_null() {
            return Err(E_POINTER.into());
        }
        *self.0.borrow_mut() = unsafe { *data };
        Ok(())
    }
    fn GetFindData(&self, data: *mut WIN32_FIND_DATAW) -> windows::core::Result<()> {
        if data.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *data = *self.0.borrow() };
        Ok(())
    }
}
fn file_time(seconds: Option<i64>) -> FILETIME {
    let ticks = seconds
        .and_then(|s| s.checked_add(11_644_473_600))
        .and_then(|s| u64::try_from(s).ok())
        .and_then(|s| s.checked_mul(10_000_000))
        .unwrap_or(0);
    FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}
fn indexed_bind_context(
    file: &DragFile,
) -> windows::core::Result<windows::Win32::System::Com::IBindCtx> {
    let size = file.size.unwrap_or(0);
    let mut info = WIN32_FIND_DATAW {
        dwFileAttributes: if file.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        },
        nFileSizeLow: size as u32,
        nFileSizeHigh: (size >> 32) as u32,
        ftCreationTime: file_time(file.created),
        ftLastWriteTime: file_time(file.modified),
        ..Default::default()
    };
    if let Some(name) = Path::new(&file.path).file_name() {
        for (target, ch) in info
            .cFileName
            .iter_mut()
            .take(259)
            .zip(name.to_string_lossy().encode_utf16())
        {
            *target = ch;
        }
    }
    let data: IFileSystemBindData = IndexedFileData(RefCell::new(info)).into();
    unsafe {
        let context = CreateBindCtx(0)?;
        // This is a parsing hint, not a file creation operation. The Shell
        // builds PIDLs from our indexed metadata instead of reopening paths.
        context.SetBindOptions(&BIND_OPTS {
            cbStruct: std::mem::size_of::<BIND_OPTS>() as u32,
            grfMode: STGM_CREATE.0,
            ..Default::default()
        })?;
        context.RegisterObjectParam(w!("File System Bind Data"), &data)?;
        Ok(context)
    }
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, String> {
        unsafe { OleInitialize(None) }.map_err(|e| format!("파일 클립보드 준비 실패: {e}"))?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() }
    }
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) }
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn format(id: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}
fn registered(name: PCWSTR) -> Result<u16, String> {
    let id = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if id == 0 {
        Err("파일 클립보드 형식을 등록할 수 없습니다.".into())
    } else {
        Ok(id as u16)
    }
}
fn set_bytes(data: &IDataObject, id: u16, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if memory.is_null() {
            return Err("클립보드 메모리 부족".into());
        }
        let target = GlobalLock(memory);
        if target.is_null() {
            GlobalFree(memory);
            return Err("클립보드 메모리를 열 수 없습니다.".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast(), bytes.len());
        GlobalUnlock(memory);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: HGLOBAL(memory),
            },
            ..Default::default()
        };
        // Ownership passes to the data object only when SetData succeeds.
        if let Err(error) = data.SetData(&format(id), &medium, true) {
            GlobalFree(memory);
            return Err(format!("파일 클립보드 구성 실패: {error}"));
        }
    }
    Ok(())
}
fn preferred_effect(data: &IDataObject) -> Result<u32, String> {
    let id = registered(CFSTR_PREFERREDDROPEFFECT)?;
    let Ok(raw) = (unsafe { data.GetData(&format(id)) }) else {
        return Ok(COPY);
    };
    let medium = Medium(raw);
    unsafe {
        if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err("파일 복사 형식이 올바르지 않습니다.".into());
        }
        let memory = medium.0.u.hGlobal.0;
        if GlobalSize(memory) < 4 {
            return Err("파일 복사 정보가 손상되었습니다.".into());
        }
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            return Err("파일 복사 정보를 읽을 수 없습니다.".into());
        }
        let value = std::ptr::read_unaligned(pointer.cast::<u32>());
        GlobalUnlock(memory);
        match value {
            COPY => Ok(COPY),
            MOVE => Ok(MOVE),
            _ => Err("파일 복사와 잘라내기만 붙여넣을 수 있습니다.".into()),
        }
    }
}
fn item(path: &Path) -> Result<IShellItem, String> {
    let text = path.to_str().ok_or("파일 경로를 읽을 수 없습니다.")?;
    if text.contains('\0') || !path.is_absolute() {
        return Err("파일의 전체 경로가 필요합니다.".into());
    }
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide(text).as_ptr()), None) }
        .map_err(|e| format!("파일을 찾을 수 없습니다: {e}"))
}
#[cfg(test)]
fn prepare(path: &Path, cut: bool) -> Result<IDataObject, String> {
    prepare_many(&[path.to_string_lossy().into_owned()], cut)
}
fn prepare_many(paths: &[String], cut: bool) -> Result<IDataObject, String> {
    shell_data(paths, Some(if cut { MOVE } else { COPY }))
}
fn shell_data(paths: &[String], effect: Option<u32>) -> Result<IDataObject, String> {
    shell_data_with_index(paths, effect, &[])
}
fn drag_data(files: &[DragFile]) -> Result<IDataObject, String> {
    let paths: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
    shell_data_with_index(&paths, None, files)
}
fn shell_data_with_index(
    paths: &[String],
    effect: Option<u32>,
    files: &[DragFile],
) -> Result<IDataObject, String> {
    if paths.is_empty()
        || paths.len() > MAX_PATHS as usize
        || paths.iter().map(String::len).sum::<usize>() > MAX_PATH_BYTES
    {
        return Err("파일 작업은 한 번에 1~4,096개 항목, 경로 합계 2MiB까지 가능합니다.".into());
    }
    let paths = top_level_paths(paths);
    let indexed: std::collections::HashMap<_, _> = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    struct Pidl(*mut ITEMIDLIST);
    impl Drop for Pidl {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0.cast()));
            }
        }
    }
    let mut owned = Vec::with_capacity(paths.len());
    for path in &paths {
        if path.contains('\0') || !Path::new(path).is_absolute() {
            return Err("파일의 전체 경로가 필요합니다.".into());
        }
        let mut pidl = Pidl(std::ptr::null_mut());
        let context = indexed
            .get(path.as_str())
            .map(|file| indexed_bind_context(file))
            .transpose()
            .map_err(|e| format!("파일 드래그 준비 실패: {e}"))?;
        unsafe {
            SHParseDisplayName(
                PCWSTR(wide(path).as_ptr()),
                context.as_ref(),
                &mut pidl.0,
                0,
                None,
            )
        }
        .map_err(|e| format!("파일을 찾을 수 없습니다: {e}"))?;
        owned.push(pidl);
    }
    let data: IDataObject = unsafe { SHCreateDataObject(None, None, None) }
        .map_err(|e| format!("파일 복사 준비 실패: {e}"))?;
    // A desktop-relative CIDA supports results from different folders too.
    // The desktop parent is an empty PIDL; each child is an absolute PIDL.
    let mut cida = vec![0u8; 4 + (owned.len() + 1) * 4];
    cida[..4].copy_from_slice(&(owned.len() as u32).to_le_bytes());
    let parent_offset = cida.len() as u32;
    cida[4..8].copy_from_slice(&parent_offset.to_le_bytes());
    cida.extend_from_slice(&[0, 0]);
    for (i, pidl) in owned.iter().enumerate() {
        let offset = cida.len() as u32;
        cida[8 + i * 4..12 + i * 4].copy_from_slice(&offset.to_le_bytes());
        let bytes = unsafe {
            std::slice::from_raw_parts(pidl.0.cast::<u8>(), ILGetSize(Some(pidl.0)) as usize)
        };
        cida.extend_from_slice(bytes);
    }
    set_bytes(
        &data,
        registered(PCWSTR(wide("Shell IDList Array").as_ptr()))?,
        &cida,
    )?;
    let mut drop_files = vec![0u8; 20]; // DROPFILES, followed by a double-NUL UTF-16 list.
    drop_files[..4].copy_from_slice(&20u32.to_le_bytes());
    drop_files[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in &paths {
        for ch in wide(path) {
            drop_files.extend_from_slice(&ch.to_le_bytes());
        }
    }
    drop_files.extend_from_slice(&[0, 0]);
    set_bytes(&data, 15, &drop_files)?;
    if let Some(effect) = effect {
        set_bytes(
            &data,
            registered(CFSTR_PREFERREDDROPEFFECT)?,
            &effect.to_le_bytes(),
        )?;
    }
    let text: Vec<u8> = wide(&paths.join("\r\n"))
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    set_bytes(&data, 13, &text)?;
    Ok(data)
}
pub fn has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(15) != 0 }
}
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}
pub fn copy(path: &str, cut: bool) -> Result<(), String> {
    copy_many(&[path.to_owned()], cut)
}
pub fn copy_many(paths: &[String], cut: bool) -> Result<(), String> {
    let _apartment = Apartment::new()?;
    let data = prepare_many(paths, cut)?;
    unsafe {
        OleSetClipboard(&data).map_err(|e| format!("파일을 클립보드에 넣을 수 없습니다: {e}"))?;
        // Render Shell formats now, so files can still be pasted after exit.
        OleFlushClipboard().map_err(|e| format!("파일 클립보드 저장 실패: {e}"))?;
    }
    Ok(())
}
/// Run on the UI STA: the Shell pumps messages until release or Escape, and
/// supplies the drag cursor/image and standard Ctrl/Shift copy/move behavior.
/// Do not set Preferred DropEffect here; let the target choose its default.
/// This data object is independent of the user's clipboard.
pub fn drag_many(owner: isize, files: &[DragFile]) -> Result<bool, String> {
    let _apartment = Apartment::new()?;
    let data = drag_data(files)?;
    let effect = unsafe {
        SHDoDragDrop(
            Some(HWND(owner as *mut _)),
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
    }
    .map_err(|e| format!("파일 드래그 실패: {e}"))?;
    Ok(effect != DROPEFFECT_NONE)
}
fn source_paths(data: &IDataObject) -> Result<Vec<PathBuf>, String> {
    let array: IShellItemArray = unsafe { SHCreateShellItemArrayFromDataObject(data) }
        .map_err(|_| "클립보드에 붙여넣을 파일이 없습니다.")?;
    let count = unsafe { array.GetCount() }.map_err(|e| e.to_string())?;
    if count == 0 || count > MAX_PATHS {
        return Err("한 번에 1~4,096개 파일을 붙여넣을 수 있습니다.".into());
    }
    let mut paths = Vec::with_capacity(count as usize);
    let mut bytes = 0;
    for i in 0..count {
        let shell = unsafe { array.GetItemAt(i) }.map_err(|e| e.to_string())?;
        let raw = unsafe { shell.GetDisplayName(SIGDN_FILESYSPATH) }
            .map_err(|_| "디스크에 저장된 파일만 붙여넣을 수 있습니다.")?;
        let text = unsafe { raw.to_string() };
        unsafe { CoTaskMemFree(Some(raw.0.cast())) };
        let text = text.map_err(|e| e.to_string())?;
        bytes += text.len();
        if bytes > MAX_PATH_BYTES {
            return Err("파일 경로가 너무 많습니다. 나누어 붙여넣어 주세요.".into());
        }
        let path = PathBuf::from(text);
        if !path.is_absolute() || path.file_name().is_none() {
            return Err("드라이브 루트는 붙여넣을 수 없습니다.".into());
        }
        paths.push(path);
    }
    Ok(paths)
}
fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}
pub fn top_level_paths(paths: &[String]) -> Vec<String> {
    let keys: Vec<_> = paths.iter().map(|path| path_key(Path::new(path))).collect();
    let all: std::collections::HashSet<&str> = keys.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    paths
        .iter()
        .zip(&keys)
        .filter_map(|(path, key)| {
            let nested = key
                .match_indices('\\')
                .any(|(i, _)| all.contains(&key[..i]));
            (!nested && seen.insert(key.as_str())).then(|| path.clone())
        })
        .collect()
}
fn validate_transfer(paths: &[PathBuf], destination: &Path, moving: bool) -> Result<bool, String> {
    if !destination.is_absolute() || !destination.is_dir() {
        return Err("붙여넣을 폴더를 선택하세요.".into());
    }
    let real_destination = std::fs::canonicalize(destination).map_err(|e| e.to_string())?;
    let destination_key = path_key(&real_destination);
    let mut same_parent = true;
    for path in paths {
        let source = std::fs::canonicalize(path)
            .map_err(|e| format!("원본 파일을 읽을 수 없습니다: {e}"))?;
        let source_key = path_key(&source);
        if source_key == destination_key
            || (source.is_dir() && destination_key.starts_with(&(source_key + "\\")))
        {
            return Err("폴더를 자기 자신이나 하위 폴더에 붙여넣을 수 없습니다.".into());
        }
        let equal_parent = source
            .parent()
            .is_some_and(|p| path_key(p) == destination_key);
        if moving && equal_parent {
            return Err("같은 폴더로는 이동할 수 없습니다.".into());
        }
        same_parent &= equal_parent;
    }
    Ok(same_parent)
}
fn transfer(
    owner: isize,
    data: &IDataObject,
    paths: &[PathBuf],
    destination: &Path,
    moving: bool,
) -> Result<bool, String> {
    let same_parent = validate_transfer(paths, destination, moving)?;
    let target = item(destination)?;
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
        operation
            .SetOwnerWindow(HWND(owner as _))
            .map_err(|e| e.to_string())?;
        let mut flags = FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR;
        if same_parent && !moving {
            flags |= FOF_RENAMEONCOLLISION;
        }
        // Keep Windows conflict/error dialogs. Never silently replace a file.
        operation
            .SetOperationFlags(flags)
            .map_err(|e| e.to_string())?;
        if moving {
            operation.MoveItems(data, &target)
        } else {
            operation.CopyItems(data, &target)
        }
        .map_err(|e| e.to_string())?;
        let performed = operation.PerformOperations();
        let aborted = operation.GetAnyOperationsAborted();
        performed.map_err(|e| format!("파일 붙여넣기 실패: {e}"))?;
        Ok(!aborted.map_err(|e| e.to_string())?.as_bool())
    }
}
pub struct PasteResult {
    pub completed: Result<bool, String>,
    pub attempted: bool,
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
}
pub fn paste(owner: isize, destination: PathBuf, expected_sequence: u32) -> PasteResult {
    let mut outcome = PasteResult {
        completed: Ok(false),
        attempted: false,
        sources: Vec::new(),
        destination,
    };
    outcome.completed = (|| {
        let _apartment = Apartment::new()?;
        if expected_sequence == 0 || sequence() != expected_sequence {
            return Err("클립보드가 변경되었습니다. 다시 붙여넣어 주세요.".into());
        }
        let data = unsafe { OleGetClipboard() }
            .map_err(|e| format!("파일 클립보드를 읽을 수 없습니다: {e}"))?;
        outcome.sources = source_paths(&data)?;
        let moving = preferred_effect(&data)? == MOVE;
        if sequence() != expected_sequence {
            return Err("클립보드가 변경되었습니다. 다시 붙여넣어 주세요.".into());
        }
        validate_transfer(&outcome.sources, &outcome.destination, moving)?;
        outcome.attempted = true;
        let complete = transfer(owner, &data, &outcome.sources, &outcome.destination, moving)?;
        if complete {
            let effect = if moving { MOVE } else { COPY };
            let _ = set_bytes(
                &data,
                registered(CFSTR_PERFORMEDDROPEFFECT)?,
                &effect.to_le_bytes(),
            );
            let _ = set_bytes(
                &data,
                registered(CFSTR_PASTESUCCEEDED)?,
                &effect.to_le_bytes(),
            );
            // A successful cut is consumed once. Preserve any newer clipboard.
            if moving && sequence() == expected_sequence {
                unsafe {
                    let _ = OleSetClipboard(None);
                }
            }
        }
        Ok(complete)
    })();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "manual drag startup timing"]
    fn drag_startup_timing() {
        let started = std::time::Instant::now();
        let _ole = Apartment::new().unwrap();
        eprintln!("OLE: {:?}", started.elapsed());
        let fixture = Fixture::new();
        let paths = vec![
            fixture
                .0
                .join("원본 문서.txt")
                .to_string_lossy()
                .into_owned(),
        ];
        let files = test_drag_files(&paths);
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let _data = drag_data(&files).unwrap();
            eprintln!("indexed drag: {:?}", started.elapsed());
            let started = std::time::Instant::now();
            let _data = shell_data(&paths, None).unwrap();
            eprintln!("legacy drag: {:?}", started.elapsed());
        }
        for i in 0..128 {
            std::fs::write(fixture.0.join(format!("timing-{i}.txt")), b"fixture").unwrap();
        }
        let paths: Vec<_> = (0..128)
            .map(|i| {
                fixture
                    .0
                    .join(format!("timing-{i}.txt"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let files = test_drag_files(&paths);
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let _data = drag_data(&files).unwrap();
            eprintln!("indexed 128 files: {:?}", started.elapsed());
            let started = std::time::Instant::now();
            let _data = shell_data(&paths, None).unwrap();
            eprintln!("legacy 128 files: {:?}", started.elapsed());
        }
    }
    fn test_drag_files(paths: &[String]) -> Vec<DragFile> {
        paths
            .iter()
            .map(|path| {
                let metadata = std::fs::metadata(path).ok();
                DragFile {
                    path: path.clone(),
                    is_dir: metadata.as_ref().is_some_and(|m| m.is_dir()),
                    size: metadata.as_ref().map(|m| m.len()),
                    created: None,
                    modified: None,
                }
            })
            .collect()
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "quickfind-clipboard-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(path.join("대상 폴더")).unwrap();
            std::fs::write(path.join("원본 문서.txt"), b"quickfind fixture").unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Check the resolved target before recursive fixture cleanup.
            if let (Ok(target), Ok(base)) =
                (self.0.canonicalize(), std::env::temp_dir().canonicalize())
            {
                if target.parent() == Some(base.as_path())
                    && target.file_name().is_some_and(|name| {
                        name.to_string_lossy().starts_with("quickfind-clipboard-")
                    })
                {
                    let _ = std::fs::remove_dir_all(target);
                }
            }
        }
    }
    #[test]
    fn shell_data_contains_real_files_text_and_copy_cut_effect_without_global_clipboard() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let source = fixture.0.join("원본 문서.txt");
        for cut in [false, true] {
            let data = prepare(&source, cut).unwrap();
            assert_eq!(source_paths(&data).unwrap(), vec![source.clone()]);
            assert_eq!(
                preferred_effect(&data).unwrap(),
                if cut { MOVE } else { COPY }
            );
            assert!(unsafe { data.QueryGetData(&format(15)) }.is_ok());
            assert!(unsafe { data.QueryGetData(&format(13)) }.is_ok());
        }
    }
    #[test]
    fn drag_data_keeps_cross_folder_selection_and_leaves_default_effect_to_target() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let other = fixture.0.join("다른 폴더");
        std::fs::create_dir(&other).unwrap();
        let second = other.join("두번째.txt");
        std::fs::write(&second, b"second fixture").unwrap();
        let paths = vec![
            fixture
                .0
                .join("원본 문서.txt")
                .to_string_lossy()
                .into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        let data = drag_data(&test_drag_files(&paths)).unwrap();
        assert_eq!(
            source_paths(&data).unwrap(),
            paths.iter().map(PathBuf::from).collect::<Vec<_>>()
        );
        assert!(unsafe { data.QueryGetData(&format(15)) }.is_ok());
        assert!(unsafe { data.QueryGetData(&format(13)) }.is_ok());
        let preferred = registered(CFSTR_PREFERREDDROPEFFECT).unwrap();
        // Shell's QueryGetData can advertise delayed formats that GetData
        // cannot supply; inspect the actual medium rather than that hint.
        assert!(
            unsafe { data.GetData(&format(preferred)) }
                .map(Medium)
                .is_err()
        );

        let nested = vec![other.to_string_lossy().into_owned(), paths[1].clone()];
        assert_eq!(
            source_paths(&drag_data(&test_drag_files(&nested)).unwrap()).unwrap(),
            vec![other]
        );
    }
    #[test]
    fn invalid_drag_selection_is_rejected_before_starting_shell_drag() {
        let _ole = Apartment::new().unwrap();
        assert!(shell_data(&[], None).is_err());
        assert!(shell_data(&vec!["C:\\test.txt".into(); MAX_PATHS as usize + 1], None).is_err());
        assert!(shell_data(&["x".repeat(MAX_PATH_BYTES + 1)], None).is_err());
        assert!(shell_data(&["relative.txt".into()], None).is_err());
        assert!(shell_data(&["C:\\invalid\0.txt".into()], None).is_err());
        for paths in [
            vec![],
            vec!["relative.txt".into()],
            vec!["C:\\invalid\0.txt".into()],
            vec!["C:\\test.txt".into(); MAX_PATHS as usize + 1],
            vec!["x".repeat(MAX_PATH_BYTES + 1)],
        ] {
            assert!(drag_data(&test_drag_files(&paths)).is_err());
        }
    }
    #[test]
    fn indexed_drag_parsing_uses_cached_metadata_without_opening_source_files() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let missing = fixture.0.join("아직 없는 폴더").join("문서.txt");
        let paths = vec![missing.to_string_lossy().into_owned()];
        let data = drag_data(&test_drag_files(&paths)).unwrap();
        assert_eq!(source_paths(&data).unwrap(), vec![missing.clone()]);
        assert!(!missing.parent().unwrap().exists());
    }
    #[test]
    fn indexed_drag_data_can_copy_and_move_files_with_shell_operations() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let source = fixture.0.join("원본 문서.txt");
        let folder = fixture.0.join("하위 폴더");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("내부.txt"), b"nested fixture").unwrap();
        let paths = vec![
            source.to_string_lossy().into_owned(),
            folder.to_string_lossy().into_owned(),
        ];
        for moving in [false, true] {
            let destination = fixture.0.join(if moving {
                "이동 시험"
            } else {
                "복사 시험"
            });
            std::fs::create_dir(&destination).unwrap();
            let data = drag_data(&test_drag_files(&paths)).unwrap();
            assert!(
                transfer(
                    0,
                    &data,
                    &[source.clone(), folder.clone()],
                    &destination,
                    moving
                )
                .unwrap()
            );
            assert_eq!(
                std::fs::read(destination.join("원본 문서.txt")).unwrap(),
                b"quickfind fixture"
            );
            assert_eq!(
                std::fs::read(destination.join("하위 폴더").join("내부.txt")).unwrap(),
                b"nested fixture"
            );
            assert_eq!(source.exists(), !moving);
            assert_eq!(folder.exists(), !moving);
        }
    }
    #[test]
    fn shell_copy_and_move_use_only_owned_fixture_files() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let source = fixture.0.join("원본 문서.txt");
        let destination = fixture.0.join("대상 폴더");
        let copied = prepare(&source, false).unwrap();
        assert!(transfer(0, &copied, &[source.clone()], &destination, false).unwrap());
        assert_eq!(
            std::fs::read(destination.join("원본 문서.txt")).unwrap(),
            b"quickfind fixture"
        );
        assert!(source.exists());
        let move_target = fixture.0.join("이동 대상");
        std::fs::create_dir(&move_target).unwrap();
        let cut = prepare(&source, true).unwrap();
        assert!(transfer(0, &cut, &[source.clone()], &move_target, true).unwrap());
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(move_target.join("원본 문서.txt")).unwrap(),
            b"quickfind fixture"
        );
    }
    #[test]
    fn multi_parent_copy_and_cut_keep_every_file_without_touching_global_clipboard() {
        let _ole = Apartment::new().unwrap();
        let fixture = Fixture::new();
        let other = fixture.0.join("다른 폴더");
        std::fs::create_dir(&other).unwrap();
        let second = other.join("두번째.txt");
        std::fs::write(&second, b"second fixture").unwrap();
        let sources = vec![fixture.0.join("원본 문서.txt"), second];
        let paths: Vec<String> = sources
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        for cut in [false, true] {
            let destination = fixture.0.join(if cut {
                "다중 이동"
            } else {
                "다중 복사"
            });
            std::fs::create_dir(&destination).unwrap();
            let data = prepare_many(&paths, cut).unwrap();
            let mut actual = source_paths(&data).unwrap();
            actual.sort();
            let mut expected = sources.clone();
            expected.sort();
            assert_eq!(actual, expected);
            assert_eq!(
                preferred_effect(&data).unwrap(),
                if cut { MOVE } else { COPY }
            );
            assert!(transfer(0, &data, &sources, &destination, cut).unwrap());
            assert_eq!(
                std::fs::read(destination.join("원본 문서.txt")).unwrap(),
                b"quickfind fixture"
            );
            assert_eq!(
                std::fs::read(destination.join("두번째.txt")).unwrap(),
                b"second fixture"
            );
            assert_eq!(sources[0].exists(), !cut);
            assert_eq!(sources[1].exists(), !cut);
        }
    }
    #[test]
    fn parent_and_child_selection_uses_parent_once_without_confusing_sibling_names() {
        let paths = [r"C:\A", r"c:\a\child.txt", r"C:\AB\other.txt", r"C:\A"].map(str::to_owned);
        assert_eq!(top_level_paths(&paths), vec![r"C:\A", r"C:\AB\other.txt"]);
    }
    #[test]
    fn self_move_and_copy_into_descendant_fail_without_touching_files() {
        let fixture = Fixture::new();
        let source = fixture.0.join("원본 문서.txt");
        assert!(validate_transfer(&[source.clone()], &fixture.0, true).is_err());
        assert!(
            validate_transfer(&[fixture.0.clone()], &fixture.0.join("대상 폴더"), false).is_err()
        );
        assert!(validate_transfer(&[source.clone()], &fixture.0, false).unwrap());
        assert!(source.exists());
    }
}
