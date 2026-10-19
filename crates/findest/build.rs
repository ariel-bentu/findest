fn main() {
    #[cfg(target_os = "linux")]
    {
        // When libxkbcommon-dev is not installed, the shared library only exists as
        // libxkbcommon.so.0 in /usr/lib/*-linux-gnu. Provide a library search path
        // with compatibility symlinks so rustc's -lxkbcommon and -lxkbcommon-x11 resolve.
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let compat_dir = out_dir.join("compat-libs");
        let _ = std::fs::create_dir_all(&compat_dir);

        for dir in ["/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib64", "/usr/lib"] {
            let path = std::path::Path::new(dir);
            for name in ["libxkbcommon", "libxkbcommon-x11"] {
                let soname = path.join(format!("{name}.so.0"));
                let symlink = compat_dir.join(format!("{name}.so"));
                if soname.exists() && !symlink.exists() {
                    let _ = std::os::unix::fs::symlink(&soname, &symlink);
                }
            }
        }
        println!("cargo:rustc-link-search=native={}", compat_dir.display());
    }
}
