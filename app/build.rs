//! Copies the FFmpeg runtime (DLLs + ffmpeg.exe used for the final encode) next to the binary.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let Ok(dir) = std::env::var("FFMPEG_DIR") else { return };
    let bin = PathBuf::from(dir).join("bin");
    // OUT_DIR = target/<profile>/build/<pkg>/out → target/<profile>
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    let Ok(entries) = std::fs::read_dir(&bin) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().unwrap().to_string_lossy().to_lowercase();
        if name.ends_with(".dll") || name == "ffmpeg.exe" {
            let dst = profile_dir.join(p.file_name().unwrap());
            let stale = std::fs::metadata(&dst).map(|m| m.len()).ok() != std::fs::metadata(&p).map(|m| m.len()).ok();
            if stale {
                let _ = std::fs::copy(&p, &dst);
            }
        }
    }
}
