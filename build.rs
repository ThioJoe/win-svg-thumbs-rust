use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    // Detect target architecture
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    // let target_triple = env::var("TARGET").unwrap();
    
    // Calculate the target directory path
    let binding = PathBuf::from(&out_dir);
    let target_dir = binding
        .ancestors()
        .nth(3) // Go up to target/{target}/{profile}
        .unwrap();
    
    match target_arch.as_str() {
        "x86_64" => {
            let dll_path = target_dir.join("win_svg_thumbs_x64.dll");
            println!( "cargo:rustc-link-arg-cdylib=/OUT:{}", dll_path.display() );
        },
        "x86" => {
            let dll_path = target_dir.join("win_svg_thumbs_x86.dll");
            println!( "cargo:rustc-link-arg-cdylib=/OUT:{}", dll_path.display() );
        },
        "aarch64" => {
            let dll_path = target_dir.join("win_svg_thumbs_arm64.dll");
            println!( "cargo:rustc-link-arg-cdylib=/OUT:{}", dll_path.display() );
        },
        _ => {}
    }

    // println!("cargo:warning=Target arch: {}", target_arch);
    // println!("cargo:warning=Target directory: {}", target_dir.display());
}