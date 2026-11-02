#![cfg(windows)]

use std::{
    path::PathBuf,
    process::{Command, ExitStatus},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::CreateMutexW};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        if let Ok(path) = self.0.canonicalize() {
            assert!(path.starts_with(parent));
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn wait(mut child: std::process::Child) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("indexer mode did not exit; it may have opened the GUI");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn copied_executable_runs_indexer_without_helper_or_gui_lock() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "hyperfinder 단일 실행파일 {}-{unique}",
        std::process::id()
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    let executable = fixture.0.join("이름 변경.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_hyperfinder"), &executable).unwrap();
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);

    // Simulate the ordinary GUI already running. A worker must not acquire
    // this mutex, restore the GUI, or load/save the user's settings.
    let name: Vec<u16> = "Local\\QuickFind.SingleInstance"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    assert!(!mutex.is_null());
    let output = fixture.0.join("인덱스 저장 폴더").join("index.bin");
    let error_path = output.with_extension("error.txt");
    let status_path = output.with_extension("status.json");

    let status = wait(Command::new(&executable).arg("--indexer").spawn().unwrap());
    assert_eq!(status.code(), Some(2));

    let status = wait(
        Command::new(&executable)
            .args(["--indexer", "--output"])
            .arg(&output)
            .arg("--root")
            .spawn()
            .unwrap(),
    );
    assert_eq!(status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&error_path).unwrap(),
        "--root 값이 없습니다."
    );
    let status: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
    assert_eq!(status["state"], "error");

    let status = wait(
        Command::new(&executable)
            .args(["--indexer", "--output"])
            .arg(&output)
            .arg("--exclude")
            .spawn()
            .unwrap(),
    );
    assert_eq!(status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&error_path).unwrap(),
        "--exclude 값이 없습니다."
    );

    // Valid worker arguments reach the NTFS entry point and replace stale
    // status/error files. A nonexistent root avoids requiring elevation here.
    std::fs::write(&error_path, "stale error").unwrap();
    let status = wait(
        Command::new(&executable)
            .args(["--indexer", "--output"])
            .arg(&output)
            .arg("--root")
            .arg(fixture.0.join("없는 폴더"))
            .arg("--exclude")
            .arg(&fixture.0)
            .args(["--exclude", "C:\\"])
            .arg("--rebuild")
            .arg("--background")
            .spawn()
            .unwrap(),
    );
    assert_eq!(status.code(), Some(1));
    let status: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
    assert_eq!(status["state"], "error");
    let error = std::fs::read_to_string(&error_path).unwrap();
    assert!(error.starts_with("검색 폴더를 열 수 없습니다:"), "{error}");
    assert_eq!(status["error"], error);
    assert!(!output.exists());
    assert!(!fixture.0.join("settings.json").exists());
    unsafe {
        CloseHandle(mutex);
    }
}
