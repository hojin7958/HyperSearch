fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let resource =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("quickfind.res");
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let numeric = format!("{},0", version.replace('.', ","));
        let template = std::fs::read_to_string(root.join("assets/quickfind.rc")).unwrap();
        let generated = template
            .replace("0,2,0,0", &numeric)
            .replace("0.2.0", &version);
        let versioned_rc = resource.with_extension("rc");
        std::fs::write(&versioned_rc, generated).unwrap();
        let rc = std::env::var_os("RC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let sdk = std::path::PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
                std::fs::read_dir(sdk)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path().join("x64/rc.exe"))
                    .filter(|path| path.is_file())
                    .max()
                    .unwrap_or_else(|| "rc.exe".into())
            });
        let status = std::process::Command::new(rc)
            .current_dir(root.join("assets"))
            .arg("/nologo")
            .arg(format!("/fo{}", resource.display()))
            .arg("/I")
            .arg(root.join("assets"))
            .arg(&versioned_rc)
            .status()
            .expect("Windows resource compiler could not start");
        assert!(status.success(), "Windows icon resource compilation failed");
        println!(
            "cargo:rustc-link-arg-bin=hyperfinder={}",
            resource.display()
        );
        println!("cargo:rerun-if-changed=assets/quickfind.ico");
        println!("cargo:rerun-if-changed=assets/quickfind.rc");
        let main_manifest =
            std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
                .join("src/bin/hyperfinder.manifest");
        println!("cargo:rustc-link-arg-bin=hyperfinder=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=hyperfinder=/MANIFESTINPUT:{}",
            main_manifest.display()
        );
        println!("cargo:rerun-if-changed=src/bin/hyperfinder.manifest");
    }
}
