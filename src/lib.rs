#[macro_use]
mod log;
mod abi;
pub mod audio;
mod bootstrap;
mod compat;
mod cpu;
mod environment;
mod file;
mod gdb;
mod image;
mod mem;
mod mrp;
mod network;
mod options;
mod paths;
mod syscall;
mod window;

use environment::Environment;
pub use skymrp_version::*;

pub fn run_app() -> Result<(), String> {
    log!("Starting bootstrap from Rust...");

    let mythroad_dir = paths::ensure_mythroad_dir()?;
    let missing = paths::missing_required_system_files();
    if !missing.is_empty() {
        window::show_missing_system_files_message(&mythroad_dir, &missing);
        return Err(format!(
            "missing required system files in {}",
            mythroad_dir.display()
        ));
    }

    let options = options::Options::default();
    let mut env = Environment::new(options)?;
    env.start()?;
    env.run()
}

pub fn main<T: Iterator<Item = String>>(_args: T) -> Result<(), String> {
    run_app()
}
