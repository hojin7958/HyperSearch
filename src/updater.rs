//! GitHub Releases updater. Network and staging run outside the UI thread.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0},
    Networking::WinHttp::*,
    Storage::FileSystem::ReplaceFileW,
    System::Threading::{
        CreateEventW, EVENT_MODIFY_STATE, OpenEventW, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
        SetEvent, WaitForSingleObject,
    },
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASES_URL: &str = "https://github.com/hojin7958/HyperSearch/releases";
pub const APPLY_ARG: &str = "--apply-update";
pub const CLEANUP_ARG: &str = "--cleanup-update";
const API: &str = "https://api.github.com/repos/hojin7958/HyperSearch/releases/latest";
const MAX_EXE: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: usize,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<ApiAsset>,
}
#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: usize,
    state: String,
}

fn release_from_json(bytes: &[u8], current: &str) -> Result<Option<Release>, String> {
    let release: ApiRelease = serde_json::from_slice(bytes).map_err(|_| {
        "GitHub의 버전 정보를 읽지 못했습니다. 잠시 후 다시 확인하세요.".to_string()
    })?;
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let next =
        semver::Version::parse(version).map_err(|_| "릴리스 버전 형식이 올바르지 않습니다.")?;
    let current = semver::Version::parse(current).map_err(|e| e.to_string())?;
    if release.draft || release.prerelease || !next.pre.is_empty() || next <= current {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == "Hyperfinder.exe")
        .ok_or("새 버전에 Windows 실행파일이 없습니다. 릴리스 페이지를 확인하세요.")?;
    let expected = format!(
        "{RELEASES_URL}/download/{}/Hyperfinder.exe",
        release.tag_name
    );
    if asset.browser_download_url != expected
        || asset.state != "uploaded"
        || !(1..=MAX_EXE).contains(&asset.size)
    {
        return Err("릴리스 파일 정보가 올바르지 않아 업데이트를 중단했습니다.".into());
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|v| v.strip_prefix("sha256:"))
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("릴리스 파일의 SHA-256 정보가 없습니다. 릴리스 페이지를 확인하세요.")?;
    Ok(Some(Release {
        version: version.into(),
        url: asset.browser_download_url,
        sha256: digest.into(),
        size: asset.size,
    }))
}

pub fn check() -> Result<Option<Release>, String> {
    check_for_version(VERSION)
}

pub fn check_for_version(current: &str) -> Result<Option<Release>, String> {
    let bytes = get(API, 1024 * 1024)?;
    release_from_json(&bytes, current)
}

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(Some(0)).collect()
}

struct Internet(*mut std::ffi::c_void);
impl Internet {
    fn new(handle: *mut std::ffi::c_void) -> Result<Self, String> {
        if handle.is_null() {
            Err(network_error())
        } else {
            Ok(Self(handle))
        }
    }
}
impl Drop for Internet {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}
fn network_error() -> String {
    format!(
        "GitHub에 연결하지 못했습니다 (Windows 오류 {}). 인터넷 연결을 확인하고 다시 시도하세요.",
        unsafe { GetLastError() }
    )
}

fn get(url: &str, maximum: usize) -> Result<Vec<u8>, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("HTTPS 주소만 사용할 수 있습니다.")?;
    let (host, path) = rest
        .split_once('/')
        .ok_or("다운로드 주소가 올바르지 않습니다.")?;
    if !matches!(host, "api.github.com" | "github.com") {
        return Err("허용되지 않은 다운로드 주소입니다.".into());
    }
    unsafe {
        let session = Internet::new(WinHttpOpen(
            wide(format!("Hyperfinder/{VERSION}").as_ref()).as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        if WinHttpSetTimeouts(session.0, 10000, 10000, 15000, 30000) == 0 {
            return Err(network_error());
        }
        let connection = Internet::new(WinHttpConnect(
            session.0,
            wide(host.as_ref()).as_ptr(),
            443,
            0,
        ))?;
        let request = Internet::new(WinHttpOpenRequest(
            connection.0,
            wide("GET".as_ref()).as_ptr(),
            wide(format!("/{path}").as_ref()).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE | WINHTTP_FLAG_REFRESH,
        ))?;
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP;
        if WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&policy as *const u32).cast(),
            4,
        ) == 0
        {
            return Err(network_error());
        }
        let headers = wide("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\nCache-Control: no-cache\r\n".as_ref());
        if WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            (headers.len() - 1) as u32,
            std::ptr::null(),
            0,
            0,
            0,
        ) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return Err(network_error());
        }
        let mut status = 0u32;
        let mut length = 4;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut length,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(network_error());
        }
        if status != 200 {
            return Err(match status {
                403 | 429 => "GitHub 요청 한도에 도달했습니다. 잠시 후 다시 확인하세요.".into(),
                404 => "공개된 릴리스를 찾지 못했습니다. 잠시 후 다시 확인하세요.".into(),
                _ => format!("다운로드에 실패했습니다 (HTTP {status}). 다시 시도하세요."),
            });
        }
        let started = Instant::now();
        let mut result = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            if started.elapsed() > Duration::from_secs(180) {
                return Err("다운로드 시간이 초과되었습니다. 다시 시도하세요.".into());
            }
            let mut read = 0;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(network_error());
            }
            if read == 0 {
                break;
            }
            if result.len() + read as usize > maximum {
                return Err("다운로드 파일이 허용 크기를 초과했습니다.".into());
            }
            result.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(result)
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verify(bytes: &[u8], expected: &str) -> Result<(), String> {
    let digest = sha256(bytes);
    if !digest.eq_ignore_ascii_case(expected) {
        return Err("다운로드 파일의 SHA-256이 일치하지 않습니다. 기존 버전을 유지합니다.".into());
    }
    // Reject HTML/error documents and executables for another architecture.
    let offset = bytes
        .get(0x3c..0x40)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()) as usize);
    if bytes.get(..2) != Some(b"MZ")
        || !offset.is_some_and(|o| bytes.get(o..o.saturating_add(6)) == Some(b"PE\0\0\x64\x86"))
    {
        return Err("Windows x64 실행파일이 아니므로 업데이트를 중단했습니다.".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Plan {
    target: PathBuf,
    sha256: String,
    args: Vec<OsString>,
    working_dir: PathBuf,
    ready_event: String,
}

pub struct StagedUpdate {
    directory: PathBuf,
    launched: bool,
}
impl Drop for StagedUpdate {
    fn drop(&mut self) {
        if !self.launched {
            remove_stage_files(&self.directory);
        }
    }
}
fn remove_stage_files(directory: &Path) {
    for name in ["helper.exe", "payload.exe", "plan.json"] {
        let _ = fs::remove_file(directory.join(name));
    }
    let _ = fs::remove_dir(directory);
}
fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub fn download(release: &Release) -> Result<StagedUpdate, String> {
    let bytes = get(&release.url, release.size)?;
    if bytes.len() != release.size {
        return Err("다운로드가 완료되지 않았습니다. 다시 시도하세요.".into());
    }
    verify(&bytes, &release.sha256)?;
    let directory = std::env::temp_dir().join(format!("hyperfinder-update-{}", nonce()));
    fs::create_dir(&directory).map_err(|e| e.to_string())?;
    let staged = StagedUpdate {
        directory,
        launched: false,
    };
    let target = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut args = std::env::args_os().skip(1);
    let mut restart_args = Vec::new();
    while let Some(arg) = args.next() {
        if arg == CLEANUP_ARG {
            args.next();
        } else {
            restart_args.push(arg);
        }
    }
    let plan = Plan {
        target: target.clone(),
        sha256: release.sha256.clone(),
        args: restart_args,
        working_dir: std::env::current_dir().map_err(|e| e.to_string())?,
        ready_event: format!("Local\\Hyperfinder.UpdateReady.{}", nonce()),
    };
    fs::write(staged.directory.join("payload.exe"), bytes).map_err(|e| e.to_string())?;
    fs::copy(target, staged.directory.join("helper.exe")).map_err(|e| e.to_string())?;
    fs::write(
        staged.directory.join("plan.json"),
        serde_json::to_vec(&plan).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(staged)
}

impl StagedUpdate {
    pub fn launch(mut self) -> Result<(), String> {
        // Check write access before closing the app, without touching its EXE.
        let plan: Plan = serde_json::from_slice(
            &fs::read(self.directory.join("plan.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let probe = plan.target.with_extension(format!("update-{}", nonce()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|_| {
                "앱 폴더에 쓰기 권한이 없습니다. 쓰기 가능한 폴더로 앱을 옮긴 뒤 다시 시도하세요."
                    .to_string()
            })?;
        file.write_all(b"Hyperfinder update")
            .map_err(|e| e.to_string())?;
        drop(file);
        fs::remove_file(probe).map_err(|e| e.to_string())?;
        let event = unsafe {
            CreateEventW(
                std::ptr::null(),
                1,
                0,
                wide(plan.ready_event.as_ref()).as_ptr(),
            )
        };
        if event.is_null() {
            return Err("업데이트 준비 상태를 확인할 수 없습니다.".into());
        }
        let child = Command::new(self.directory.join("helper.exe"))
            .arg(APPLY_ARG)
            .arg(std::process::id().to_string())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                unsafe {
                    CloseHandle(event);
                }
                return Err(format!("업데이트 도우미를 시작하지 못했습니다: {error}"));
            }
        };
        let ready = unsafe { WaitForSingleObject(event, 10000) };
        unsafe {
            CloseHandle(event);
        }
        if ready != WAIT_OBJECT_0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("업데이트 도우미가 준비되지 않았습니다. 기존 버전으로 계속 실행합니다. 다시 시도하세요.".into());
        }
        self.launched = true;
        Ok(())
    }
}

fn signal_ready(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Ok(());
    }
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, wide(name.as_ref()).as_ptr());
        if event.is_null() {
            return Err("업데이트 준비 신호를 전달하지 못했습니다.".into());
        }
        let result = SetEvent(event);
        CloseHandle(event);
        if result == 0 {
            return Err("업데이트 준비 신호를 전달하지 못했습니다.".into());
        }
    }
    Ok(())
}

fn wait_for_parent(pid: u32, target: &Path, ready_event: &str) -> Result<(), String> {
    unsafe {
        let process = OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if process.is_null() {
            return if GetLastError() == ERROR_INVALID_PARAMETER {
                signal_ready(ready_event)
            } else {
                Err("기존 앱의 종료를 확인하지 못했습니다.".into())
            };
        }
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let valid = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) != 0
            && target.canonicalize().is_ok_and(|target| {
                PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))
                    .canonicalize()
                    .is_ok_and(|actual| actual == target)
            });
        let result = if valid {
            if let Err(error) = signal_ready(ready_event) {
                CloseHandle(process);
                return Err(error);
            }
            WaitForSingleObject(process, 60000)
        } else {
            u32::MAX
        };
        CloseHandle(process);
        if result == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err("기존 앱이 종료되지 않아 업데이트를 중단했습니다. 앱을 완전히 종료한 뒤 다시 시도하세요.".into())
        }
    }
}

fn replace_binary(target: &Path, payload: &Path, digest: &str) -> Result<PathBuf, String> {
    let bytes = fs::read(payload).map_err(|e| e.to_string())?;
    verify(&bytes, digest)?;
    let id = nonce();
    let candidate = target.with_extension(format!("update-{id}.exe"));
    let backup = target.with_extension(format!("previous-{id}.exe"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&candidate)
        .map_err(|e| e.to_string())?;
    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&candidate);
        return Err(error.to_string());
    }
    let started = Instant::now();
    loop {
        let replaced = unsafe {
            ReplaceFileW(
                wide(target.as_os_str()).as_ptr(),
                wide(candidate.as_os_str()).as_ptr(),
                wide(backup.as_os_str()).as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced != 0 {
            return Ok(backup);
        }
        // ReplaceFile can leave the original at the backup path on failure.
        if !target.exists() && backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        if started.elapsed() >= Duration::from_secs(10) || !candidate.exists() {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&candidate);
            return Err(format!(
                "실행파일을 교체하지 못했습니다: {error}. 앱 폴더 권한과 실행 중인 인덱서·보안 프로그램을 확인하세요."
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn apply(pid: u32) -> Result<(), String> {
    let directory = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("업데이트 폴더가 없습니다.")?
        .to_path_buf();
    apply_from_directory(&directory, pid)
}

fn apply_from_directory(directory: &Path, pid: u32) -> Result<(), String> {
    let plan: Plan =
        serde_json::from_slice(&fs::read(directory.join("plan.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    verify(
        &fs::read(directory.join("payload.exe")).map_err(|e| e.to_string())?,
        &plan.sha256,
    )?;
    wait_for_parent(pid, &plan.target, &plan.ready_event)?;
    let result = replace_binary(&plan.target, &directory.join("payload.exe"), &plan.sha256);
    match result {
        Ok(backup) => {
            match Command::new(&plan.target)
                .arg(CLEANUP_ARG)
                .arg(&directory)
                .args(&plan.args)
                .current_dir(&plan.working_dir)
                .spawn()
            {
                Ok(_) => {
                    let _ = fs::remove_file(backup);
                    Ok(())
                }
                Err(error) => {
                    // Restore the old binary if Windows cannot start the new one.
                    let _ = fs::remove_file(&plan.target);
                    fs::rename(&backup, &plan.target).map_err(|e| {
                        format!(
                            "재실행 실패: {error}. 이전 버전은 {}에 보관되어 있습니다: {e}",
                            backup.display()
                        )
                    })?;
                    let _ = Command::new(&plan.target)
                        .args(&plan.args)
                        .current_dir(&plan.working_dir)
                        .spawn();
                    Err(format!(
                        "새 버전을 실행하지 못해 이전 버전을 복원했습니다: {error}"
                    ))
                }
            }
        }
        Err(error) => {
            let _ = Command::new(&plan.target)
                .args(&plan.args)
                .current_dir(&plan.working_dir)
                .spawn();
            Err(error)
        }
    }
}

pub fn cleanup(directory: PathBuf) {
    // Only delete our three known staging files, never recursively delete a path
    // received on the command line. A plan must point at this running EXE.
    let allowed = directory
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("hyperfinder-update-"))
        && directory
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            == std::env::temp_dir().canonicalize().ok();
    let plan = fs::read(directory.join("plan.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Plan>(&b).ok());
    if !allowed
        || !plan.is_some_and(|p| {
            p.target.canonicalize().is_ok_and(|target| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|actual| actual == target)
            })
        })
    {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..100 {
            if fs::remove_file(directory.join("helper.exe")).is_ok()
                || !directory.join("helper.exe").exists()
            {
                remove_stage_files(&directory);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(version: &str) -> serde_json::Value {
        serde_json::json!({"tag_name": format!("v{version}"), "assets": [{"name": "Hyperfinder.exe", "browser_download_url": format!("{RELEASES_URL}/download/v{version}/Hyperfinder.exe"), "digest": format!("sha256:{}", "a".repeat(64)), "size": 4096, "state": "uploaded"}]})
    }
    fn parse(value: &serde_json::Value, current: &str) -> Result<Option<Release>, String> {
        release_from_json(&serde_json::to_vec(value).unwrap(), current)
    }
    #[test]
    fn compares_versions_numerically_and_skips_unpublished_or_prerelease() {
        assert!(parse(&release("0.10.0"), "0.9.0").unwrap().is_some());
        assert!(parse(&release("0.2.0"), "0.2.0").unwrap().is_none());
        assert!(parse(&release("0.2.0"), "0.3.0").unwrap().is_none());
        assert!(parse(&release("0.4.0-beta.1"), "0.3.0").unwrap().is_none());
        for field in ["draft", "prerelease"] {
            let mut value = release("1.0.0");
            value[field] = true.into();
            assert!(parse(&value, "0.3.0").unwrap().is_none());
        }
    }
    #[test]
    fn rejects_missing_digest_wrong_repository_truncated_and_oversize_metadata() {
        for (field, value) in [
            ("digest", serde_json::Value::Null),
            ("digest", "sha256:no".into()),
            (
                "browser_download_url",
                "https://github.com/attacker/app/Hyperfinder.exe".into(),
            ),
            ("size", (MAX_EXE + 1).into()),
            ("state", "new".into()),
            ("name", "Other.exe".into()),
        ] {
            let mut body = release("1.0.0");
            body["assets"][0][field] = value;
            assert!(parse(&body, VERSION).is_err(), "{field}");
        }
        assert!(release_from_json(b"{", VERSION).is_err());
    }
    #[test]
    fn hash_and_architecture_must_both_match() {
        let bytes = b"<html>failed</html>";
        assert!(verify(bytes, &sha256(bytes)).is_err());
        assert!(verify(bytes, &"0".repeat(64)).is_err());
    }
    #[test]
    fn replacement_preserves_original_on_bad_hash_and_keeps_backup_on_success() {
        let dir = std::env::temp_dir().join(format!("hyperfinder-replace-test-{}", nonce()));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("이름 변경 앱.exe");
        let payload = dir.join("payload.exe");
        let mut bytes = vec![0u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..70].copy_from_slice(b"PE\0\0\x64\x86");
        fs::write(&target, b"old").unwrap();
        fs::write(&payload, &bytes).unwrap();
        assert!(replace_binary(&target, &payload, &"0".repeat(64)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        let backup = replace_binary(&target, &payload, &sha256(&bytes)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        for path in [target, payload, backup] {
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn failed_restart_restores_the_previous_binary() {
        let dir = std::env::temp_dir().join(format!("hyperfinder-rollback-test-{}", nonce()));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("app.exe");
        let mut bytes = vec![0u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..70].copy_from_slice(b"PE\0\0\x64\x86");
        // The x64 signature passes validation but Windows rejects this truncated
        // PE when starting it, exercising the actual launch-failure rollback.
        fs::write(&target, b"original binary").unwrap();
        fs::write(dir.join("payload.exe"), &bytes).unwrap();
        let plan = Plan {
            target: target.clone(),
            sha256: sha256(&bytes),
            args: Vec::new(),
            working_dir: dir.clone(),
            ready_event: String::new(),
        };
        fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
        let error = apply_from_directory(&dir, 0).unwrap_err();
        assert!(error.contains("이전 버전을 복원"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), b"original binary");
        for name in ["app.exe", "payload.exe", "plan.json"] {
            fs::remove_file(dir.join(name)).unwrap();
        }
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    #[ignore = "requires no running Hyperfinder instance; uses isolated fixture data"]
    fn real_helper_waits_replaces_restarts_and_preserves_settings() {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let root = std::env::current_dir().unwrap();
        let new_exe = root.join("target/release/hyperfinder.exe");
        let old_exe = root.join("_workspace/releases/v0.2.0/Hyperfinder.exe");
        assert!(new_exe.is_file() && old_exe.is_file());
        let class = wide("QuickFind.MainWindow".as_ref());
        assert!(
            unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null(),
            "Close the user's app before this opt-in test"
        );
        let directory = std::env::temp_dir().join(format!("hyperfinder-update-{}", nonce()));
        fs::create_dir(&directory).unwrap();
        let fixture = root.join(format!("_workspace/update-e2e-{}", nonce()));
        let data = fixture.join("설정 보존");
        let search_root = fixture.join("검색 폴더");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir(&search_root).unwrap();
        fs::write(search_root.join("keep.txt"), b"keep").unwrap();
        let settings = crate::settings::Settings {
            roots: vec![search_root.clone()],
            query: "*keep*".into(),
            font_size: 13.0,
            fast_index: false,
            check_updates_on_startup: false,
            ..Default::default()
        };
        fs::write(
            data.join("settings.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let target = fixture.join("이름 변경 앱.exe");
        fs::copy(old_exe, &target).unwrap();
        fs::copy(&new_exe, directory.join("helper.exe")).unwrap();
        let bytes = fs::read(&new_exe).unwrap();
        fs::write(directory.join("payload.exe"), &bytes).unwrap();
        let plan = Plan {
            target: target.clone(),
            sha256: sha256(&bytes),
            args: vec!["--data-dir".into(), data.clone().into_os_string()],
            working_dir: fixture.clone(),
            ready_event: String::new(),
        };
        fs::write(
            directory.join("plan.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        let mut old = Command::new(&target).args(&plan.args).spawn().unwrap();
        let until = |condition: &mut dyn FnMut() -> bool| {
            let start = Instant::now();
            while !condition() {
                assert!(
                    start.elapsed() < Duration::from_secs(20),
                    "process/UI timeout"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        until(&mut || !unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null());
        let mut helper = Command::new(directory.join("helper.exe"))
            .arg(APPLY_ARG)
            .arg(old.id().to_string())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            helper.try_wait().unwrap().is_none(),
            "helper must wait for the old process"
        );
        assert_ne!(fs::read(&target).unwrap(), bytes);
        Command::new(&target).arg("--quit").status().unwrap();
        until(&mut || old.try_wait().unwrap().is_some());
        until(&mut || helper.try_wait().unwrap().is_some());
        assert!(helper.wait().unwrap().success());
        until(&mut || {
            let window = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
            let query = unsafe { GetDlgItem(window, 100) };
            let mut buffer = [0u16; 128];
            let count = unsafe { GetWindowTextW(query, buffer.as_mut_ptr(), 128) };
            String::from_utf16_lossy(&buffer[..count.max(0) as usize]) == "*keep*"
        });
        assert_eq!(fs::read(&target).unwrap(), bytes);
        let saved: crate::settings::Settings =
            serde_json::from_slice(&fs::read(data.join("settings.json")).unwrap()).unwrap();
        assert_eq!(saved.query, "*keep*");
        assert_eq!(saved.font_size, 13.0);
        assert_eq!(saved.roots, [search_root.clone()]);
        assert_eq!(fs::read(search_root.join("keep.txt")).unwrap(), b"keep");
        let window = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
        let mut new_pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut new_pid);
        }
        assert_ne!(new_pid, old.id());
        Command::new(&target).arg("--quit").status().unwrap();
        until(&mut || unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null());
        until(&mut || !directory.exists());
        println!(
            "update E2E passed: old PID {}, new PID {}, SHA256 {}, preserved query/font/roots/source, fixture {}",
            old.id(),
            new_pid,
            sha256(&bytes),
            fixture.display()
        );
    }
    #[test]
    #[ignore = "contacts the public GitHub Releases API"]
    fn live_github_release_check_and_verified_download() {
        let release = check_for_version("0.0.0").unwrap().unwrap();
        println!(
            "GitHub release: {} ({} bytes)",
            release.version, release.size
        );
        let staged = download(&release).unwrap();
        assert!(staged.directory.join("payload.exe").exists());
    }
}
