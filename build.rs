use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/featherdock.ico");
    println!("cargo:rerun-if-changed=assets/featherdock.res.o");
    println!("cargo:rerun-if-changed=assets/featherdock.rc");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        // Prebuilt from featherdock.rc + featherdock.ico. The pinned Rustup GNU
        // toolchain does not ship an x64 windres, so linking the object keeps
        // normal builds free of a machine-specific resource compiler dependency.
        let resource = manifest_dir.join("assets").join("featherdock.res.o");
        println!("cargo:rustc-link-arg={}", resource.display());
    }
}
