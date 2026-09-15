#[cfg(windows)]
fn main() {
    if let Err(error) = afunix_agent::cli::run() {
        eprintln!("afunix-agent: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("afunix-agent: this program requires Windows");
    std::process::exit(1);
}
