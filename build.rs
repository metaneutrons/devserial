// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

fn main() {
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
