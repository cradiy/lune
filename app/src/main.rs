#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match lune::run_desktop() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Lune: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    compile_error!("Lune desktop requires Linux with Wayland.");
}
