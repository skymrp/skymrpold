pub fn main() {
    let mut args = std::env::args();
    let _ = args.next().unwrap(); // skip argv[0]
    match (args.next(), args.next()) {
        (Some(x), None) if x == "--branding" => println!("{}", skymrp_version::branding()),
        (None, _) => println!("{}", skymrp_version::VERSION),
        _ => panic!(),
    }
}
