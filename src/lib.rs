#![allow(rustdoc::private_intra_doc_links)]
#[macro_use]
mod log;
mod abi;
pub mod audio;
mod bootstrap;
mod compat;
mod cpu;
mod direct_mrp;
mod environment;
mod file;
mod gdb;
mod image;
mod mem;
mod mrp;
mod mythroad_host;
mod network;
mod options;
mod paths;
mod syscall;
mod window;

use environment::Environment;
pub use skymrp_version::*;

pub fn main<T: Iterator<Item = String>>(mut args: T) -> Result<(), String> {
    echo!(
        "skymrp {}{}{} — https://skrymrp.org/",
        branding(),
        if branding().is_empty() { "" } else { " " },
        VERSION,
    );
    echo!();

    let _program_name = args.next();
    if let Some(mrp_path) = args.find(|arg| {
        std::path::Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mrp"))
    }) {
        return direct_mrp::run_mrp_file(std::path::Path::new(&mrp_path));
    }

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
    let _ = env.run();
    Ok(())
}
