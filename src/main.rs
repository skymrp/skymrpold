mod window;

use std::path::PathBuf;

const USAGE: &str = "\
Usage:
    skymrp path/to/example.mrp

Options:
    --help
        Print this help text.
";

fn main() -> Result<(), String> {
    let mut args = std::env::args();
    let _ = args.next().unwrap(); // skip argv[0]

    let mut mrp_path: Option<PathBuf> = None;
    for arg in args {
        if arg == "--help" {
            println!("{}", USAGE);
            return Ok(());
        } else if mrp_path.is_none() {
            mrp_path = Some(PathBuf::from(arg));
        } else {
            eprintln!("{}", USAGE);
            return Err(format!("Unexpected arguments: {:?}", arg));
        }
    }

    let Some(mrp_path) = mrp_path else {
        eprintln!("{}", USAGE);
        return Err("Path to mrp must be specified".to_string());
    };

    println!("mrp path is: {}", mrp_path.to_str().unwrap());

    let mut window = window::Window::new("skymrp");
    let mut events = Vec::new();

    loop {
        window.poll_for_events(&mut events);
        for event in events.drain(..) {
            match event {
                window::Event::Quit => {
                    print!("User requested quit, exiting...");
                    return Ok(());
                }
            }
        }
    }
}
