//! The engine process. It owns capture, encoding, the replay buffer and recording, and takes
//! commands as JSON lines on stdin. It runs apart from the UI so that a crash or hang in a
//! graphics driver can be recovered from by replacing this process. Anything that merely
//! returns an error is handled here without losing the process.

pub mod backend;
mod engine;
pub mod fake;
pub mod log;
mod mix;
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

/// Entry point for `tape --self-test <file>`, which needs no UI, so it runs on a build machine.
pub fn self_test(path: &std::path::Path) -> i32 {
    let backend = platform_backend();
    let report = backend.self_test(&tape_core::settings::Settings::default());
    match std::fs::write(path, &report) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}\n{report}");
            1
        }
    }
}
