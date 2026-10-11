mod args;
mod client;
mod commands;
mod intents;
mod lifecycle;
mod runtime;
#[cfg(test)]
#[path = "../tests/support/unit.rs"]
mod test_support;
mod updater;

pub fn run_from<I, T>(argv: I) -> anyhow::Result<std::process::ExitCode>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::Parser;
    commands::run(args::Cli::try_parse_from(argv)?)
}
