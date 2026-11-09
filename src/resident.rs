//! Per-user Windows service. Privileged writes are confined to its protected
//! Program Files directory. User settings/requests are bounded, read-only input.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::Authorization::*,
    Security::*,
    Storage::FileSystem::*,
    System::{Services::*, Threading::*},
    UI::Shell::*,
};

pub const SERVICE_ARG: &str = "--index-service";
pub const INSTALL_ARG: &str = "--install-index-service";
pub const REMOVE_ARG: &str = "--remove-index-service";
static STOP: AtomicBool = AtomicBool::new(false);
static CONTEXT: OnceLock<(String, PathBuf)> = OnceLock::new();
static STATUS_HANDLE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn unique() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
fn error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}
fn valid_sid(sid: &str) -> bool {
    sid.starts_with("S-1-5-")
        && sid.len() <= 160
        && sid
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-')
}
pub fn owner_sid() -> Result<String, String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(error("사용자 확인"));
        }
        let mut needed = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(error("사용자 정보"));
        }
        let mut text = null_mut();
        if ConvertSidToStringSidW(
            (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid,
            &mut text,
        ) == 0
        {
            return Err(error("사용자 SID"));
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(sid)
    }
}
fn program_files() -> Result<PathBuf, String> {
    unsafe {
        let mut ptr = null_mut();
        if SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, null_mut(), &mut ptr) < 0 {
            return Err(error("설치 폴더"));
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
            ptr, len,
        )));
        windows_sys::Win32::System::Com::CoTaskMemFree(ptr.cast());
        Ok(path)
    }
}
fn directory(sid: &str) -> Result<PathBuf, String> {
    if !valid_sid(sid) {
        return Err("사용자 SID가 올바르지 않습니다.".into());
    }
    Ok(program_files()?.join("HyperfinderIndexer").join(sid))
}
pub fn current_directory() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("QUICKFIND_RESIDENT_TEST_DIR") {
        return Ok(path.into());
    }
    directory(&owner_sid()?)
}
pub fn enabled_for_process() -> bool {
    std::env::var_os("QUICKFIND_DATA_DIR").is_none()
        || std::env::var_os("QUICKFIND_RESIDENT_TEST_DIR").is_some()
}
fn service_name(sid: &str) -> String {
    format!("HyperfinderIndexer-{sid}")
}

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    owner: String,
    settings: PathBuf,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub generation: u128,
    pub entries: usize,
    pub roots: Vec<PathBuf>,
    pub excluded: Vec<PathBuf>,
    pub updated: u64,
    pub changed_records: u64,
    pub full_scans: u64,
    #[serde(default)]
    pub request_id: u128,
    #[serde(default)]
    pub content_generation: u128,
}
impl Manifest {
    pub fn content_checkpoint(&self) -> u128 {
        if self.content_generation == 0 {
            self.generation
        } else {
            self.content_generation
        }
    }
    pub fn index_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("snapshot-{}.bin", self.generation))
    }
    pub fn matches(&self, roots: &[PathBuf], excluded: &[PathBuf]) -> bool {
        self.roots == roots && self.excluded == excluded
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub running: bool,
    pub heartbeat: u64,
    pub phase: String,
    pub error: Option<String>,
    pub version: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Request {
    id: u128,
    rebuild: bool,
    #[serde(default)]
    foreground: bool,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("설정 파일이 너무 큽니다.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    use std::io::Write;
    let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), unique()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
pub fn manifest() -> Option<(PathBuf, Manifest)> {
    let dir = current_directory().ok()?;
    Some((dir.clone(), read_json(&dir.join("current.json")).ok()?))
}
pub fn status() -> Option<Status> {
    read_json(&current_directory().ok()?.join("status.json")).ok()
}
pub fn available() -> bool {
    // Isolated --data-dir fixtures and portable copies must never attach to a
    // different user's live service or alter its configuration.
    if !enabled_for_process() {
        return false;
    }
    if std::env::var_os("QUICKFIND_RESIDENT_TEST_DIR").is_some() {
        return status().is_some_and(|s| s.running);
    }
    owner_sid().is_ok_and(|sid| service_running(&service_name(&sid)))
}
pub fn configured() -> bool {
    if !enabled_for_process() {
        return false;
    }
    if std::env::var_os("QUICKFIND_RESIDENT_TEST_DIR").is_some() {
        return status().is_some();
    }
    owner_sid().is_ok_and(|sid| service_state(&service_name(&sid)).is_some())
}
pub fn request(rebuild: bool) -> Result<u128, String> {
    let dir = crate::platform::local_data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = unique();
    write_json(
        &dir.join("indexer-request.json"),
        &Request {
            id,
            rebuild,
            foreground: true,
        },
    )?;
    Ok(id)
}
pub struct LoadedView {
    pub index: crate::model::Index,
    pub delta_stamp: Option<SystemTime>,
    pub request_id: u128,
}
pub fn delta_stamp(directory: &Path) -> Option<SystemTime> {
    std::fs::metadata(directory.join("changes.json"))
        .ok()?
        .modified()
        .ok()
}
pub fn load_view(
    directory: &Path,
    manifest: &Manifest,
    base: Option<&crate::model::Index>,
) -> Result<LoadedView, String> {
    let mut index = match base {
        Some(index) => index.clone(),
        None => crate::cache::load(&manifest.index_path(directory), &manifest.roots)?,
    };
    index.share();
    let stamp = delta_stamp(directory);
    let mut request_id = manifest.request_id;
    if stamp.is_some() {
        match crate::resident_delta::read(&directory.join("changes.json")) {
            Ok(delta) if delta.base == manifest.content_checkpoint() => {
                index = crate::resident_delta::apply(&index, &delta)?;
                request_id = request_id.max(delta.request_id);
            }
            Ok(_) => {}
            Err(error) if directory.join("changes.json").exists() => return Err(error),
            Err(_) => {}
        }
    }
    Ok(LoadedView {
        index,
        delta_stamp: stamp,
        request_id,
    })
}
pub fn viewing() {
    let _ = write_json(
        &crate::platform::local_data_dir().join("indexer-view.json"),
        &now(),
    );
}
pub fn install_elevated(remove: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let sid = owner_sid()?;
    let settings = crate::platform::local_data_dir().join("settings.json");
    let params = format!(
        "{} {} {}",
        if remove { REMOVE_ARG } else { INSTALL_ARG },
        quote(&sid),
        quote(&settings.to_string_lossy())
    );
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        let file = wide(&exe.to_string_lossy());
        let args = wide(&params);
        let verb = wide("runas");
        info.cbSize = std::mem::size_of_val(&info) as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpFile = file.as_ptr();
        info.lpParameters = args.as_ptr();
        info.lpVerb = verb.as_ptr();
        info.nShow = 0;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(error("백그라운드 인덱서 등록"));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if code != 0 {
            return Err("백그라운드 인덱서 등록이 완료되지 않았습니다.".into());
        }
    }
    Ok(())
}
fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        result.push(c);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}
fn protect_directory(path: &Path, sid: &str) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    if std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    {
        return Err("설치 폴더가 연결 경로이므로 중단했습니다.".into());
    }
    // Program Files already excludes writes by ordinary users. The per-user
    // directory adds a protected ACL; neither snapshots nor the service image
    // can be replaced by an unelevated process.
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    unsafe {
        let sddl = wide(&format!(
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;{sid})"
        ));
        let mut sd = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        ) == 0
        {
            return Err(error("설치 권한 준비"));
        }
        let ok = SetFileSecurityW(
            wide(&path.to_string_lossy()).as_ptr(),
            OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sd,
        );
        LocalFree(sd);
        if ok == 0 {
            return Err(error("설치 권한 설정"));
        }
    }
    Ok(())
}
fn sc(args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let system = unsafe {
        let mut pointer = null_mut();
        if SHGetKnownFolderPath(&FOLDERID_System, 0, null_mut(), &mut pointer) < 0 {
            return Err(error("시스템 경로"));
        }
        let mut len = 0;
        while *pointer.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
            pointer, len,
        )));
        windows_sys::Win32::System::Com::CoTaskMemFree(pointer.cast());
        path
    };
    let output = std::process::Command::new(system.join("sc.exe"))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
pub fn install(sid: &str, settings: &Path, remove: bool) -> Result<(), String> {
    if !settings.is_absolute() || settings.file_name().is_none_or(|n| n != "settings.json") {
        return Err("설정 경로가 올바르지 않습니다.".into());
    }
    let dir = directory(sid)?;
    let name = service_name(sid);
    let _ = sc(&["stop".into(), name.clone()]);
    let started = Instant::now();
    while service_running(&name) && started.elapsed() < Duration::from_secs(45) {
        std::thread::sleep(Duration::from_millis(200));
    }
    if service_running(&name) {
        return Err("기존 인덱서 종료를 기다린 뒤 다시 시도하세요.".into());
    }
    if remove {
        return sc(&["delete".into(), name]);
    }
    protect_directory(dir.parent().unwrap(), sid)?;
    protect_directory(&dir, sid)?;
    let target = dir.join("HyperfinderIndexer.exe");
    let staged = dir.join("HyperfinderIndexer.new.exe");
    std::fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, &staged)
        .map_err(|e| e.to_string())?;
    std::fs::rename(staged, &target).map_err(|e| e.to_string())?;
    write_json(
        &dir.join("service.json"),
        &Config {
            owner: sid.into(),
            settings: settings.into(),
        },
    )?;
    let bin = format!(
        "{} {} {}",
        quote(&target.to_string_lossy()),
        SERVICE_ARG,
        quote(sid)
    );
    let exists = sc(&["query".into(), name.clone()]).is_ok();
    sc(&[
        if exists { "config" } else { "create" }.into(),
        name.clone(),
        "binPath=".into(),
        bin,
        "start=".into(),
        "auto".into(),
        "DisplayName=".into(),
        "Hyperfinder Background Indexer".into(),
    ])?;
    sc(&[
        "failure".into(),
        name.clone(),
        "reset=".into(),
        "86400".into(),
        "actions=".into(),
        "restart/30000/restart/60000/restart/120000".into(),
    ])?;
    sc(&["failureflag".into(), name.clone(), "1".into()])?;
    sc(&["start".into(), name])
}
fn service_running(name: &str) -> bool {
    service_state(name).is_some_and(|state| state != SERVICE_STOPPED)
}
fn service_state(name: &str) -> Option<u32> {
    unsafe {
        let manager = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return None;
        }
        let service = OpenServiceW(manager, wide(name).as_ptr(), SERVICE_QUERY_STATUS);
        CloseServiceHandle(manager);
        if service.is_null() {
            return None;
        }
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let ok = QueryServiceStatus(service, &mut status);
        CloseServiceHandle(service);
        (ok != 0).then_some(status.dwCurrentState)
    }
}
fn set_service_status(state: u32) {
    set_service_status_result(state, 0);
}
fn set_service_status_result(state: u32, code: u32) {
    unsafe {
        let handle = STATUS_HANDLE.load(Ordering::Acquire) as SERVICE_STATUS_HANDLE;
        if handle.is_null() {
            return;
        }
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if state == SERVICE_RUNNING {
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
            } else {
                0
            },
            dwWin32ExitCode: code,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: if state == SERVICE_STOP_PENDING {
                45000
            } else {
                0
            },
        };
        SetServiceStatus(handle, &status);
    }
}
unsafe extern "system" fn control(
    code: u32,
    _: u32,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> u32 {
    if code == SERVICE_CONTROL_STOP || code == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::Release);
        set_service_status(SERVICE_STOP_PENDING);
    }
    0
}
unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
    let (sid, dir) = CONTEXT.get().unwrap();
    unsafe {
        let handle = RegisterServiceCtrlHandlerExW(
            wide(&service_name(sid)).as_ptr(),
            Some(control),
            null_mut(),
        );
        STATUS_HANDLE.store(handle as isize, Ordering::Release);
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
    set_service_status(SERVICE_RUNNING);
    let result = run_loop(dir);
    let failed = result.is_err();
    if let Err(error) = result {
        crate::diagnostics::record(dir, "service_failed", serde_json::json!({"reason":error}));
    }
    let _ = write_json(
        &dir.join("status.json"),
        &Status {
            running: false,
            heartbeat: now(),
            phase: "stopped".into(),
            error: None,
            version: env!("CARGO_PKG_VERSION").into(),
        },
    );
    set_service_status_result(SERVICE_STOPPED, if failed { ERROR_GEN_FAILURE } else { 0 });
}
pub fn dispatch(sid: &str) -> Result<(), String> {
    let dir = directory(sid)?;
    CONTEXT
        .set((sid.into(), dir))
        .map_err(|_| "서비스가 이미 시작되었습니다.")?;
    let mut name = wide(&service_name(sid));
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(error("백그라운드 서비스 시작"));
    }
    Ok(())
}
fn run_loop(dir: &Path) -> Result<(), String> {
    let config: Config = read_json(&dir.join("service.json"))?;
    if !valid_sid(&config.owner) || directory(&config.owner)? != dir {
        return Err("서비스 설정 불일치".into());
    }
    let mut settings: crate::settings::Settings = read_json(&config.settings)?;
    let mut current: Option<Manifest> = read_json(&dir.join("current.json")).ok();
    let sidecar = current
        .as_ref()
        .map(|m| m.index_path(dir).with_extension("ntfs.bin"));
    let mut excluded = settings.excluded_folders.clone();
    excluded.push(dir.into());
    if let Some(parent) = config.settings.parent() {
        excluded.push(parent.into());
    }
    let mut index =
        crate::ntfs::ResidentIndex::new(settings.roots.clone(), excluded, sidecar.as_deref());
    let mut private_roots = vec![dir.to_owned()];
    if let Some(parent) = config.settings.parent() {
        private_roots.push(parent.to_owned());
    }
    index.set_private_roots(private_roots);
    if let Some(previous) = &current {
        index.changed_records = previous.changed_records;
        index.full_scans = previous.full_scans;
    }
    let mut status = Status {
        running: true,
        heartbeat: now(),
        phase: "starting".into(),
        error: None,
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let mut last_publish = Instant::now() - Duration::from_secs(60);
    let mut last_delta_publish = Instant::now() - Duration::from_secs(60);
    let mut retry_publish_at = Instant::now();
    let mut last_heartbeat = 0;
    let mut public_repair = current
        .as_ref()
        .is_some_and(|m| crate::cache::load(&m.index_path(dir), &m.roots).is_err());
    let mut last_request = current.as_ref().map_or(0, |m| m.request_id);
    while !STOP.load(Ordering::Acquire) {
        status.heartbeat = now();
        status.phase = "watching".into();
        status.error = None;
        if let Ok(latest) = read_json::<crate::settings::Settings>(&config.settings) {
            settings = latest;
        }
        let mut excluded = settings.excluded_folders.clone();
        excluded.push(dir.into());
        if let Some(parent) = config.settings.parent() {
            excluded.push(parent.into());
        }
        index.configure(settings.roots.clone(), excluded);
        let request: Request = config
            .settings
            .parent()
            .and_then(|p| read_json(&p.join("indexer-request.json")).ok())
            .unwrap_or_default();
        let mut requested =
            request.id != 0 && current.as_ref().is_none_or(|m| m.request_id != request.id);
        if request.id != last_request {
            last_request = request.id;
            if request.rebuild {
                index.rebuild();
            }
        }
        if now().saturating_sub(last_heartbeat) >= 10 {
            let _ = write_json(&dir.join("status.json"), &status);
            last_heartbeat = now();
        }
        let result = index.poll(&STOP, |event, details| {
            if event.ends_with("failed")
                || event.ends_with("retry")
                || event.ends_with("unavailable")
            {
                status.error = Some(
                    details
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or(event)
                        .to_owned(),
                );
            }
            crate::diagnostics::record(dir, event, details);
        });
        if let Err(error) = result {
            status.error = Some(error);
        }
        let needs_first = public_repair
            || current
                .as_ref()
                .is_none_or(|m| !m.matches(&settings.roots, &settings.excluded_folders));
        if requested
            && index.caught_up()
            && !index.delta_dirty()
            && !index.needs_checkpoint()
            && !needs_first
        {
            if let Some(previous) = &mut current {
                previous.request_id = last_request;
                write_json(&dir.join("current.json"), previous)?;
            }
            requested = false;
        }
        let viewed: u64 = config
            .settings
            .parent()
            .and_then(|p| read_json(&p.join("indexer-view.json")).ok())
            .unwrap_or(0);
        let interval = if now().saturating_sub(viewed) < 30 {
            15
        } else {
            60
        };
        let maintenance = index.raw_dirty() && last_publish.elapsed() >= Duration::from_secs(300);
        if !STOP.load(Ordering::Acquire)
            && (index.caught_up() || index.memory_pressure())
            && Instant::now() >= retry_publish_at
            && (index.needs_checkpoint() || needs_first || maintenance)
            && (needs_first
                || requested
                || index.memory_pressure()
                || last_publish.elapsed() >= Duration::from_secs(interval))
        {
            status.phase = "saving".into();
            status.heartbeat = now();
            let _ = write_json(&dir.join("status.json"), &status);
            let generation = unique();
            let checkpoint_started = Instant::now();
            let mut next = Manifest {
                generation,
                roots: settings.roots.clone(),
                excluded: settings.excluded_folders.clone(),
                updated: now(),
                changed_records: index.changed_records,
                full_scans: index.full_scans,
                entries: 0,
                request_id: if index.caught_up() {
                    last_request
                } else {
                    current.as_ref().map_or(0, |m| m.request_id)
                },
                content_generation: generation,
            };
            let checkpoint = if !index.dirty() && !needs_first {
                let previous = current.as_ref().unwrap();
                next.content_generation = previous.content_checkpoint();
                index
                    .checkpoint_unchanged(&next.index_path(dir), &previous.index_path(dir))
                    .map(|_| previous.entries)
            } else {
                index.checkpoint(&next.index_path(dir))
            };
            match checkpoint {
                Ok(entries) => {
                    public_repair = false;
                    next.entries = entries;
                    write_json(&dir.join("current.json"), &next)?;
                    let _ = std::fs::remove_file(dir.join("changes.json"));
                    crate::diagnostics::record(
                        dir,
                        "checkpoint",
                        serde_json::json!({"generation":generation,"entries":entries,"changed_records":index.changed_records,"full_scans":index.full_scans,"elapsed_ms":checkpoint_started.elapsed().as_millis()}),
                    );
                    // Retain the preceding generation for readers and recovery.
                    let old = current.as_ref().map(|m| m.generation);
                    current = Some(next);
                    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if let Some(number) = name
                            .strip_prefix("snapshot-")
                            .and_then(|n| n.split('.').next())
                            .and_then(|n| n.parse::<u128>().ok())
                        {
                            if number != generation
                                && Some(number) != old
                                && (name == format!("snapshot-{number}.bin")
                                    || name == format!("snapshot-{number}.ntfs.bin"))
                            {
                                let _ = std::fs::remove_file(entry.path());
                            }
                        }
                    }
                }
                Err(error) => {
                    status.error = Some(error);
                    retry_publish_at = Instant::now() + Duration::from_secs(30);
                }
            }
            last_publish = Instant::now();
        }
        if !STOP.load(Ordering::Acquire)
            && index.caught_up()
            && index.delta_dirty()
            && !index.needs_checkpoint()
            && (requested || last_delta_publish.elapsed() >= Duration::from_secs(interval))
        {
            if let Some(previous) = &mut current {
                let mut delta = index.delta(previous.content_checkpoint(), unique(), now());
                delta.request_id = last_request;
                write_json(&dir.join("changes.json"), &delta)?;
                previous.request_id = last_request;
                write_json(&dir.join("current.json"), previous)?;
                crate::diagnostics::record(
                    dir,
                    "delta_published",
                    serde_json::json!({"revision":delta.revision,"entries":delta.changes.len(),"full_scans":index.full_scans}),
                );
                index.delta_published();
                last_delta_publish = Instant::now();
            }
        }
        status.heartbeat = now();
        if status.error.is_some() || now().saturating_sub(last_heartbeat) >= 10 {
            let _ = write_json(&dir.join("status.json"), &status);
            last_heartbeat = now();
        }
        for _ in 0..10 {
            if STOP.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_is_unchanged_when_serialization_fails_and_metadata_only_keeps_content_id() {
        struct Fail;
        impl Serialize for Fail {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("simulated failure"))
            }
        }
        let dir = std::env::temp_dir().join(format!(
            "hyperfinder-commit-{}-{}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("current.json");
        let before = Manifest {
            generation: 7,
            content_generation: 5,
            ..Manifest::default()
        };
        write_json(&path, &before).unwrap();
        assert!(write_json(&path, &Fail).is_err());
        let after: Manifest = read_json(&path).unwrap();
        assert_eq!(after.generation, 7);
        assert_eq!(after.content_checkpoint(), 5);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn service_paths_and_requests_are_bounded() {
        assert!(!valid_sid("S-1-5-21\\..\\elsewhere"));
        assert!(!valid_sid("S-1-5-21\" & whoami"));
        assert!(valid_sid("S-1-5-21-111-222-333-1001"));
        assert_eq!(quote("C:\\a b\\"), "\"C:\\a b\\\\\"");
        let m = Manifest {
            generation: 42,
            ..Manifest::default()
        };
        assert_eq!(
            m.index_path(Path::new("C:\\safe")),
            PathBuf::from("C:\\safe\\snapshot-42.bin")
        );
    }
}
