use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=UCX_NO_PKG_CONFIG");

    // Determine whether to use system UCX or build from source
    let (include_path, use_system) = if env::var("UCX_NO_PKG_CONFIG").is_ok() {
        println!("cargo:warning=UCX_NO_PKG_CONFIG set, building from source");
        (build_from_source(), false)
    } else if let Some(include) = try_system_ucx() {
        println!("cargo:warning=Using system UCX installation");
        (include, true)
    } else {
        println!("cargo:warning=System UCX not found or incompatible, building from source");
        (build_from_source(), false)
    };

    // Generate bindings
    let bindings = bindgen::Builder::default()
        .clang_arg(format!("-I{}", include_path))
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .allowlist_function("uc[tsmp]_.*")
        .allowlist_var("uc[tsmp]_.*")
        .allowlist_var("UC[TSMP]_.*")
        .allowlist_type("uc[tsmp]_.*")
        .rustified_enum(".*")
        .bitfield_enum("ucp_feature")
        .bitfield_enum(".*_field")
        .bitfield_enum(".*_flags(_t)?")
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // If we built from source, tell cargo where to find the libraries
    if !use_system {
        println!("cargo:rustc-link-search=native={}/lib", out_path.display());
    }
}

/// Try to use system UCX via pkg-config.
/// Returns the include path if successful, None otherwise.
fn try_system_ucx() -> Option<String> {
    match pkg_config::Config::new()
        .atleast_version("1.19")
        .cargo_metadata(true)
        .probe("ucx")
    {
        Ok(library) => {
            // Check that version is < 2.0
            let version = &library.version;
            let parts: Vec<&str> = version.split('.').collect();
            if let Some(major) = parts.first().and_then(|s| s.parse::<u32>().ok()) {
                if major >= 2 {
                    println!(
                        "cargo:warning=Found UCX version {} but require < 2.0",
                        version
                    );
                    return None;
                }
            }

            // pkg-config automatically adds link directives via cargo_metadata(true)
            // Now we need to return an include path for bindgen
            if let Some(include_path) = library.include_paths.first() {
                return Some(include_path.display().to_string());
            }
            None
        }
        Err(e) => {
            println!("cargo:warning=pkg-config failed: {}", e);
            None
        }
    }
}

/// Build UCX from source and return the include path.
fn build_from_source() -> String {
    let dst = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // Return if the outputs exist.
    if dst.join("lib/libuct.a").exists()
        && dst.join("lib/libucs.a").exists()
        && dst.join("lib/libucm.a").exists()
        && dst.join("lib/libucp.a").exists()
    {
        return dst.join("include").display().to_string();
    }

    // Initialize git submodule if necessary.
    if !Path::new("ucx/.git").exists() {
        let _ = Command::new("git")
            .args(&["submodule", "update", "--init"])
            .status();
    }

    // Create build directory.
    let _ = std::fs::create_dir(&dst.join("build"));

    // Copy source.
    if !dst.join("ucx").exists() {
        let _ = Command::new("cp")
            .arg("-r")
            .arg("ucx")
            .arg(dst.join("ucx"))
            .status();
    }

    // autogen.sh
    Command::new("bash")
        .current_dir(&dst.join("ucx"))
        .arg("./autogen.sh")
        .status()
        .expect("failed to run autogen.sh");

    // configure
    Command::new("bash")
        .current_dir(&dst.join("build"))
        .arg(&dst.join("ucx/contrib/configure-release"))
        .arg(&format!("--prefix={}", dst.display()))
        .status()
        .expect("failed to configure");

    // make
    Command::new("make")
        .current_dir(&dst.join("build"))
        .arg(&format!("-j{}", env::var("NUM_JOBS").unwrap()))
        .status()
        .expect("failed to make");

    // make install
    Command::new("make")
        .current_dir(&dst.join("build"))
        .arg("install")
        .status()
        .expect("failed to make install");

    // Tell cargo to link the library (only needed when building from source)
    println!("cargo:rustc-link-lib=ucp");

    dst.join("include").display().to_string()
}
