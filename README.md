# WebM Studio

A desktop WebM editor/renderer written in Rust (egui + FFmpeg).

## Building (Windows)

1. Install Rust and LLVM (for `libclang`, expected at `C:\Program Files\LLVM\bin`).
2. Download the FFmpeg **shared** build from https://www.gyan.dev/ffmpeg/builds/ and extract it to `third_party/ffmpeg` (so that `third_party/ffmpeg/bin`, `include` and `lib` exist).
3. Build:

   ```sh
   cd app
   cargo build --release
   ```

The FFmpeg DLLs and `ffmpeg.exe` are copied next to the binary automatically by `build.rs`.

## License

FFmpeg is licensed under the GPL v3 (gyan.dev full build).
