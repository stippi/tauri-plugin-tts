const COMMANDS: &[&str] = &[
    "speak",
    "stop",
    "get_voices",
    "is_speaking",
    "is_initialized",
    "pause_speaking",
    "resume_speaking",
    "preview_voice",
    "register_listener",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .ios_path("ios")
        .build();

    // The buffer-producing synthesizer (`src/synthesizer.rs`) is one
    // Objective-C file shared by iOS and macOS; it lands in this crate's
    // object files, so no SwiftPM product has to be linked for it.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "ios" || target_os == "macos" {
        println!("cargo:rerun-if-changed=apple/TtsStream.m");
        cc::Build::new()
            .file("apple/TtsStream.m")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .flag("-Wno-unused-parameter")
            .compile("tts_stream");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }
}
