use std::env;

fn main() {
    // Build the C++ engine (../engine) with CMake and link it into the app.
    let dst = cmake::Config::new("../engine")
        .define("MUSIKORE_BUILD_TESTS", "OFF")
        .build();

    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=musikore_engine");
    println!("cargo:rustc-link-lib=static=rtaudio");

    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rustc-link-lib=framework=CoreAudio");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=c++");
        }
        Ok("windows") => {
            for lib in ["ksuser", "mfplat", "mfuuid", "wmcodecdspuuid", "winmm", "ole32"] {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        _ => {
            println!("cargo:rustc-link-lib=asound");
            println!("cargo:rustc-link-lib=stdc++");
        }
    }

    println!("cargo:rerun-if-changed=../engine/CMakeLists.txt");
    println!("cargo:rerun-if-changed=../engine/include");
    println!("cargo:rerun-if-changed=../engine/src");

    tauri_build::build();
}
