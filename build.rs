fn main() {
    println!("cargo:rerun-if-changed=vendor/sonivox/");

    if std::env::var_os("CARGO_FEATURE_SONIVOX").is_some() {
        let dst = cmake::Config::new("vendor/sonivox")
            .define("BUILD_SHARED_LIBS", "OFF")
            .define("BUILD_TESTING", "OFF")
            .define("BUILD_APPLICATION", "OFF")
            .define("SF2_SUPPORT", "OFF")
            .define("ZLIB_SUPPORT", "OFF")
            .build();

        println!("cargo:rustc-link-search=native={}/lib", dst.display());
        println!("cargo:rustc-link-lib=static=sonivox");
        // The package also builds an rlib; pass the archive to final binaries explicitly.
        println!("cargo:rustc-link-arg=-lsonivox");
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-arg=-lm");
        }
    }

    // sdl2-sys links `-lSDL2`; Homebrew installs it here on Apple Silicon.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
    }
}
