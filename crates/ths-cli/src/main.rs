fn main() -> anyhow::Result<std::process::ExitCode> {
    match thus_spoke_zakura::run_from(std::env::args_os()) {
        Ok(code) => Ok(code),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}
