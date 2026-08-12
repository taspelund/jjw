use clap::Parser;

fn main() {
    if let Err(error) = jjw::run(jjw::cli::Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(jjw::process_exit_code(&error));
    }
}
