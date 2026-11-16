fn main() {
    if let Err(error) = spotsweep::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
