//! The engine process. It owns capture, encoding, the replay buffer and recording, and takes
//! commands as JSON lines on stdin. It runs apart from the UI so that a crash or hang in a
//! graphics driver can be recovered from by replacing this process. Anything that merely
//! returns an error is handled here without losing the process.

pub mod backend;
mod engine;
pub mod fake;
pub mod log;
mod mix;
mod quality;
pub mod rotation;
#[cfg(windows)]
mod win;

use std::sync::Arc;

use backend::Backend;

pub use engine::run;

pub fn platform_backend() -> Arc<dyn Backend> {
    #[cfg(windows)]
    {
        if std::env::var_os("TAPE_FAKE").is_none() {
            return Arc::new(win::Windows::new());
        }
    }
    Arc::new(fake::Fake::from_env())
}

/// Entry point for `tape --engine`.
pub fn main() -> i32 {
    std::panic::set_hook(Box::new(|info| log::line(&format!("panic: {info}"))));
    let backend = platform_backend();
    engine::run(backend, std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}

/// Entry point for `tape --self-test <file> [settings.json]`, which needs no UI, so it runs on a
/// build machine. It's always a process of its own: a process can only duplicate an output once,
/// so inside a running engine the test would collide with replay and blame Desktop Duplication.
pub fn self_test(path: &std::path::Path, settings: Option<&std::path::Path>) -> i32 {
    let backend = platform_backend();
    let settings = settings
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let report = backend.self_test(&settings);
    match std::fs::write(path, &report) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}\n{report}");
            1
        }
    }
}

/// Entry point for `tape --share-copy <clip> <out.mp4> [log] [--range <start> <end>]`, which makes
/// the copy a shared link uploads, of the whole clip or the part between two times in seconds. A process of its own for the same reason as the engine: it drives the GPU's decoder
/// and encoder, and a driver crash there mustn't take the app down. Reports JSON lines on
/// stdout: `{"progress":0.5}`, then `{"done":{...}}` or `{"error":"..."}`.
pub fn share_copy(input: &std::path::Path, output: &std::path::Path, range: Option<(f64, f64)>) -> i32 {
    use std::io::Write;
    let emit = |value: serde_json::Value| {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    };
    let mut progress = |p: f64| emit(serde_json::json!({ "progress": p }));
    match copy_for_sharing(input, output, range, &mut progress) {
        Ok((width, height, seconds, path)) => {
            log::line(&format!("share copy of {} done by {path}: {width}x{height}, {seconds:.1} s", input.display()));
            emit(serde_json::json!({ "done": { "width": width, "height": height, "seconds": seconds } }));
            0
        }
        Err(e) => {
            log::line(&format!("share copy of {} failed: {e}", input.display()));
            emit(serde_json::json!({ "error": e }));
            1
        }
    }
}

type Range = Option<(f64, f64)>;

#[cfg(windows)]
fn copy_for_sharing(input: &std::path::Path, output: &std::path::Path, range: Range, progress: &mut dyn FnMut(f64)) -> Result<(u32, u32, f64, &'static str), String> {
    if std::env::var_os("TAPE_FAKE").is_some() {
        return fake::share_copy(input, output, range, progress);
    }
    let copied = win::share::share_copy(input, output, range, progress)?;
    Ok((copied.width, copied.height, copied.seconds, copied.path))
}

#[cfg(not(windows))]
fn copy_for_sharing(input: &std::path::Path, output: &std::path::Path, range: Range, progress: &mut dyn FnMut(f64)) -> Result<(u32, u32, f64, &'static str), String> {
    fake::share_copy(input, output, range, progress)
}
