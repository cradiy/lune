#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let options = match lune::parse_cli() {
        Ok(lune::CliAction::Run(options)) => options,
        Ok(lune::CliAction::Help) => {
            print!("{}", lune::HELP_TEXT);
            return std::process::ExitCode::SUCCESS;
        }
        Ok(lune::CliAction::Version) => {
            println!("{}", lune::VERSION_TEXT);
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("lune: {error}\nTry 'lune --help' for more information.");
            return std::process::ExitCode::from(2);
        }
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lune=info"))
        .format_timestamp_millis()
        .init();

    log::debug!("Creating the Wayland event loop");
    match lune::run_desktop(options) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            log::error!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    compile_error!("Lune desktop requires Linux with Wayland.");
}
