//! The engine on its fake backend, for the end-to-end tests. Real runs go through `tape --engine`.

fn main() {
    if let Ok(path) = std::env::var("TAPE_LOG") {
        tape_engine::log::init(std::path::Path::new(&path));
    }
    std::process::exit(tape_engine::main());
}
