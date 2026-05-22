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
pub use window::run as run_app;

pub fn main<T: Iterator<Item = String>>(mut args: T) -> Result<(), String> {
    let mut options = options::Options::default();
    let mut app_args = None::<Vec<String>>;
    let _ = args.next().unwrap(); // skip argv[0]
    let mut env = Environment::new(options)?;
    env.run();
    Ok(())
}
