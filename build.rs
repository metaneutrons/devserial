// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

fn main() {
    build_identity();

    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=resources/macos.m");
        cc::Build::new()
            .file("resources/macos.m")
            .compile("devserial_macos");
        println!("cargo:rustc-link-lib=framework=Cocoa");
    }

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=resources/icon.ico");
        let mut res = winres::WindowsResource::new();
        res.set_icon("resources/icon.ico");
        res.set("ProductName", "devserial");
        res.set("FileDescription", "devserial MCP & Serial Monitor");
        res.set("LegalCopyright", "Copyright (C) 2026 Fabian Schmieder");
        res.set("OriginalFilename", "devserial.exe");
        if let Err(e) = res.compile() {
            eprintln!("Warning: failed to compile Windows resource: {e}");
        }
    }
}

/// The commit the binary is built from and whether the tree had changes, so
/// a client can tell a daemon from another build (#115). Without git, as from
/// a source archive, both stay empty and a comparison says it cannot tell.
fn build_identity() {
    // The tree's state can change without a commit, so any source change has
    // to run this again; a stale "clean" would hide the development case.
    for path in [
        "src",
        "resources",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        ".git/HEAD",
        ".git/index",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    let dirty = if commit.is_empty() {
        ""
    } else {
        match git(&["status", "--porcelain", "--untracked-files=no"]) {
            Some(changes) if changes.is_empty() => "0",
            Some(_) => "1",
            None => "",
        }
    };
    // Tells two builds of the same modified tree apart: the daemon may still
    // run the binary from before the last rebuild. Only compared when both
    // sides are modified.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    println!("cargo:rustc-env=DEVSERIAL_COMMIT={commit}");
    println!("cargo:rustc-env=DEVSERIAL_DIRTY={dirty}");
    println!("cargo:rustc-env=DEVSERIAL_BUILD_STAMP={stamp}");
}
