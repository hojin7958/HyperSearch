//! Worker mode of the same executable. The GUI launches it with elevation
//! only while reading NTFS metadata and stays at ordinary integrity itself.
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub const MODE_ARG: &str = "--indexer";

pub fn run(mut args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
    let mut roots = Vec::new();
    let mut excluded_folders = Vec::new();
    let mut output = None;
    let mut rebuild = false;
    let mut background = false;
    let parsed = (|| -> Result<(), String> {
        while let Some(arg) = args.next() {
            match arg.to_str() {
                    Some("--root") => {
                        roots.push(PathBuf::from(args.next().ok_or("--root 값이 없습니다.")?))
                    }
                    Some("--output") => {
                        output = Some(PathBuf::from(args.next().ok_or("--output 값이 없습니다.")?))
                    }
                    Some("--exclude") => {
                        excluded_folders.push(PathBuf::from(args.next().ok_or("--exclude 값이 없습니다.")?))
                    }
                    Some("--rebuild") => rebuild = true,
                    Some("--background") => background = true,
                    _ => return Err(
                        "사용법: Hyperfinder.exe --indexer --output 경로 --root C:\\ [--root D:\\] [--exclude 제외폴더] [--rebuild]"
                            .into(),
                    ),
                }
        }
        Ok(())
    })();
    let Some(output) = output else {
        return 2;
    };
    let error_path = output.with_extension("error.txt");
    let _ = std::fs::remove_file(&error_path);
    if let Some(parent) = output.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let status_path = output.with_extension("status.json");
    write_json(
        &status_path,
        &serde_json::json!({"state":"running","entries":0,"elapsed_ms":0}),
    );
    let result=parsed.and_then(|_|{
        if background {
            unsafe {
                use windows_sys::Win32::System::Threading::*;
                SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
                SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
            }
        }
        if roots.is_empty(){roots=quickfind::indexer::default_roots();}
        quickfind::ntfs::run(&roots,&excluded_folders,&output,&AtomicBool::new(false),|p|{
            write_json(&status_path,&serde_json::json!({"state":"running","entries":p.entries,"folders":p.folders,"skipped":p.skipped,"elapsed_ms":p.elapsed_ms}));
        },rebuild)
    });
    match result {
        Ok(report) => {
            write_json(
                &status_path,
                &serde_json::json!({"state":"complete","report":report}),
            );
            0
        }
        Err(error) => {
            let _ = std::fs::write(error_path, &error);
            write_json(
                &status_path,
                &serde_json::json!({"state":"error","error":error}),
            );
            1
        }
    }
}
fn write_json(path: &Path, value: &serde_json::Value) {
    let temporary = path.with_extension(format!("status.tmp.{}", std::process::id()));
    if let Ok(bytes) = serde_json::to_vec(value) {
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(&temporary, path);
        }
    }
}
