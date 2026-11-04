//! Embeds the app icon in cleangroove.exe on Windows, so Explorer, shortcuts and the taskbar show
//! it. The window icon itself is set at run time in main.rs. The icon is cosmetic: if the
//! resource compiler is missing, the build carries on with a warning.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = manifest.join("assets").join("icon.ico");
    let rc = out.join("app_icon.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/")))
        .expect("write app_icon.rc");

    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("gnu") => {
            let obj = out.join("app_icon.o");
            let status = Command::new("windres")
                .args(["-O", "coff", "-i"])
                .arg(&rc)
                .arg("-o")
                .arg(&obj)
                .status();
            match status {
                Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
                _ => println!("cargo:warning=windres not found or failed: cleangroove.exe gets no embedded icon"),
            }
        }
        Ok("msvc") => {
            let res = out.join("app_icon.res");
            let status = Command::new("rc.exe").arg("/nologo").arg("/fo").arg(&res).arg(&rc).status();
            match status {
                Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", res.display()),
                _ => println!("cargo:warning=rc.exe not found or failed: cleangroove.exe gets no embedded icon"),
            }
        }
        _ => {}
    }
}
