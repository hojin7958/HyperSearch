#![windows_subsystem = "windows"]

use quickfind::{native_app, platform, settings::Settings, updater};

mod indexer_process;

fn main() {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| {
        arg == quickfind::resident::SERVICE_ARG
            || arg == quickfind::resident::INSTALL_ARG
            || arg == quickfind::resident::REMOVE_ARG
    }) {
        let mode = args.next().unwrap();
        let sid = args
            .next()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_default();
        let result = if mode == quickfind::resident::SERVICE_ARG {
            quickfind::resident::dispatch(&sid)
        } else {
            args.next()
                .map(std::path::PathBuf::from)
                .ok_or_else(|| "설정 경로가 없습니다.".to_string())
                .and_then(|settings| {
                    quickfind::resident::install(
                        &sid,
                        &settings,
                        mode == quickfind::resident::REMOVE_ARG,
                    )
                })
        };
        if let Err(error) = result {
            show_error(&error);
            std::process::exit(1);
        }
        return;
    }
    if args.peek().is_some_and(|arg| arg == updater::APPLY_ARG) {
        args.next();
        let result = args
            .next()
            .and_then(|arg| arg.to_str().and_then(|v| v.parse::<u32>().ok()))
            .ok_or_else(|| "업데이트 대상 프로세스가 없습니다.".to_string())
            .and_then(updater::apply);
        if let Err(error) = result {
            show_error(&error);
            std::process::exit(1);
        }
        return;
    }
    // Dispatch before acquiring the GUI mutex or loading user settings: the
    // elevated worker must run while the ordinary search window is open.
    if args
        .peek()
        .is_some_and(|arg| arg == indexer_process::MODE_ARG)
    {
        args.next();
        std::process::exit(indexer_process::run(args));
    }
    if let Err(error) = run(args) {
        if error == "already-running" {
            return;
        }
        show_error(&error);
    }
}

fn show_error(error: &str) {
    let text: Vec<u16> = error.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Hyperfinder".encode_utf16().chain(Some(0)).collect();
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION,
        );
    }
}

fn run(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let mut roots = Vec::new();
    let mut cleanup_dir = None;
    while let Some(arg) = args.next() {
        if arg == updater::CLEANUP_ARG {
            cleanup_dir = Some(args.next().ok_or("업데이트 정리 폴더가 없습니다.")?.into());
        } else if arg == "--root" {
            roots.push(std::path::PathBuf::from(
                args.next().ok_or("--root 뒤에 폴더 경로를 입력하세요.")?,
            ));
        } else if arg == "--data-dir" {
            let path = args
                .next()
                .ok_or("--data-dir 뒤에 폴더 경로를 입력하세요.")?;
            // Before any application threads start.
            unsafe {
                std::env::set_var("QUICKFIND_DATA_DIR", path);
            }
        } else if arg == "--quit" {
            platform::request_exit_existing();
            return Ok(());
        } else {
            return Err("사용법: Hyperfinder.exe [--root 폴더] [--data-dir 설정폴더]".into());
        }
    }
    if let Some(directory) = cleanup_dir {
        updater::cleanup(directory);
    }
    let _instance = platform::single_instance()?;
    let mut settings = Settings::load();
    if !roots.is_empty() {
        settings.roots = roots;
    }
    native_app::run(settings)
}
