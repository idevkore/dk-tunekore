//! Builds the C++ shim that wraps every call into a plugin (see `shim/tk_shim.cpp`).

fn main() {
    println!("cargo:rerun-if-changed=shim/tk_shim.cpp");
    println!("cargo:rerun-if-changed=build.rs");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("shim/tk_shim.cpp")
        .flag_if_supported("-fexceptions")
        .warnings(true)
        .compile("tk_shim");
}
