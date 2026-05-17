#[macro_use]
mod log;
mod abi;
mod app;
pub mod audio;
mod bridge;
mod compat;
mod cpu;
mod debug;
mod environment;
mod file;
mod gdb;
mod image;
mod mem;
mod mrp;
mod network;
mod options;
mod paths;
mod runtime;
mod syscall;
mod unicorn;
mod window;

pub use app::run as run_app;
use environment::Environment;
pub use runtime::{event, start_runtime, timer};
pub use skymrp_version::*;

pub fn main<T: Iterator<Item = String>>(mut args: T) -> Result<(), String> {
    let mut options = options::Options::default();
    let mut app_args = None::<Vec<String>>;
    let _ = args.next().unwrap(); // skip argv[0]
    let mut env = Environment::new(options)?;
    env.run();
    Ok(())
}
