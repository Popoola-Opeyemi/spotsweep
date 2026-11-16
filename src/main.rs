fn main() {
    if let Err(error) = spotlight_dev_exclusions::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
