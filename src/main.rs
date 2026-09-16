use std::process::ExitCode;

fn main() -> ExitCode {
    match sweep::cli::main() {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("sweep: {err:#}");
            ExitCode::from(1)
        }
    }
}
