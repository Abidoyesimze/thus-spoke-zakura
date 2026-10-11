mod wallet;

use std::process::ExitCode;

use crate::{
    args::{Cli, Command, Pool, SendArgs, WalletCommand},
    runtime::Runtime,
    updater,
};
use wallet::send;

pub(crate) fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    if let Some(Command::Update { version, check }) = &cli.command {
        return updater::run(version.as_deref(), *check, cli.json);
    }
    if matches!(cli.command, Some(Command::Uninstall)) {
        return updater::uninstall();
    }
    if should_check_for_updates(&cli)
        && let Some(notice) = updater::startup_notice()
    {
        println!("{notice}");
    }
    let runtime = Runtime::discover()?;
    match cli.command.unwrap_or(Command::Start {
        no_open: false,
        port_offset: 0,
    }) {
        Command::Start {
            no_open,
            port_offset,
        } => runtime.start(&cli.name, no_open, cli.json, port_offset),
        Command::Build { dev } => runtime.build(dev),
        Command::Pull => runtime.pull(),
        Command::Update { .. } => unreachable!("update is handled before runtime discovery"),
        Command::Uninstall => unreachable!("uninstall is handled before runtime discovery"),
        Command::Status => runtime.status(&cli.name, cli.json),
        Command::Open => runtime.open(&cli.name),
        Command::Endpoints => runtime.endpoints(&cli.name, cli.json),
        Command::Mine { blocks } => runtime.mine(&cli.name, blocks, cli.json),
        Command::Faucet { address, amount } => {
            runtime.faucet(&cli.name, &address, amount.zatoshi(), cli.json)
        }
        Command::Wallet { action } => match action {
            WalletCommand::Faucet {
                accounts,
                amount,
                pool,
            } => runtime.wallet_faucet(
                &cli.name,
                &accounts,
                amount.zatoshi(),
                pool.as_str(),
                cli.json,
            ),
            WalletCommand::Send(args) => send(&runtime, &cli.name, args, cli.json),
            WalletCommand::Shield {
                from,
                to,
                amount,
                memo,
            } => send(
                &runtime,
                &cli.name,
                SendArgs {
                    from,
                    to,
                    amount,
                    source_pool: Pool::Transparent,
                    destination_pool: Pool::Ironwood,
                    memo,
                },
                cli.json,
            ),
            WalletCommand::Unshield { from, to, amount } => send(
                &runtime,
                &cli.name,
                SendArgs {
                    from,
                    to,
                    amount,
                    source_pool: Pool::Ironwood,
                    destination_pool: Pool::Transparent,
                    memo: None,
                },
                cli.json,
            ),
        },
        Command::Logs {
            service,
            follow,
            tail,
            head,
        } => runtime.logs(&cli.name, service.as_deref(), follow, tail, head),
        Command::Stop => runtime.stop(&cli.name),
        Command::Reset { force } => runtime.reset(&cli.name, force),
        Command::List => runtime.list(cli.json),
        Command::Doctor => runtime.doctor(cli.json),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn should_check_for_updates(cli: &Cli) -> bool {
    !cli.json && matches!(cli.command, None | Some(Command::Start { .. }))
}

#[cfg(test)]
#[path = "../tests/unit/commands/tests.rs"]
mod tests;
