use std::fs;
use std::path::{Path, PathBuf};

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn emit_build_id(manifest_dir: &Path) {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cef");

    let mut hash = FNV_OFFSET_BASIS;
    hash = fnv1a_update(
        hash,
        std::env::var("CARGO_PKG_VERSION")
            .unwrap_or_default()
            .as_bytes(),
    );
    hash = fnv1a_update(
        hash,
        if std::env::var("CARGO_FEATURE_CRONET").is_ok() {
            b"|cronet=1"
        } else {
            b"|cronet=0"
        },
    );

    let mut files = vec![
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("Cargo.lock"),
    ];
    collect_files(&manifest_dir.join("src"), &mut files);
    collect_files(&manifest_dir.join("cef"), &mut files);
    files.sort();

    for path in files {
        let rel = path.strip_prefix(manifest_dir).unwrap_or(&path);
        println!("cargo:rerun-if-changed={}", rel.display());
        hash = fnv1a_update(hash, rel.to_string_lossy().as_bytes());
        hash = fnv1a_update(hash, &[0]);
        if let Ok(bytes) = fs::read(&path) {
            hash = fnv1a_update(hash, &bytes);
        }
        hash = fnv1a_update(hash, &[0xff]);
    }

    println!("cargo:rustc-env=WICK_BUILD_ID={hash:016x}");
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    emit_build_id(Path::new(&dir));

    if std::env::var("CARGO_FEATURE_CRONET").is_ok() {
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            // macOS arm64: static link libcronet.a
            println!("cargo:rustc-link-search=native={}/lib/darwin_arm64", dir);
            println!("cargo:rustc-link-lib=static=cronet");

            cc::Build::new()
                .file("lib/darwin_arm64/stub.c")
                .compile("cronet_stub");

            for fw in &[
                "CoreFoundation",
                "CoreGraphics",
                "CoreText",
                "Foundation",
                "Security",
                "ApplicationServices",
                "AppKit",
                "IOKit",
                "OpenDirectory",
                "CFNetwork",
                "CoreServices",
                "Network",
                "SystemConfiguration",
                "UniformTypeIdentifiers",
                "CryptoTokenKit",
                "LocalAuthentication",
            ] {
                println!("cargo:rustc-link-lib=framework={}", fw);
            }
            println!("cargo:rustc-link-lib=bsm");
            println!("cargo:rustc-link-lib=pmenergy");
            println!("cargo:rustc-link-lib=pmsample");
            println!("cargo:rustc-link-lib=resolv");
        } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            // Linux amd64: dynamic link libcronet.so
            // The .so must be next to the binary at runtime, or in LD_LIBRARY_PATH
            println!("cargo:rustc-link-search=native={}/lib/linux_amd64", dir);
            println!("cargo:rustc-link-lib=dylib=cronet");
        } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
            // Linux arm64: dynamic link libcronet.so
            println!("cargo:rustc-link-search=native={}/lib/linux_arm64", dir);
            println!("cargo:rustc-link-lib=dylib=cronet");
        } else {
            panic!(
                "Cronet linking not configured for this platform. \
                 Build without --features cronet, or add support in build.rs."
            );
        }
    }
}
