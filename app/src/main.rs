#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lune=info"))
        .format_timestamp_millis()
        .init();

    log::debug!("Creating the Wayland event loop");
    match lune::run_desktop() {
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
