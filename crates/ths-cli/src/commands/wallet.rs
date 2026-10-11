use anyhow::{Result, bail};

use crate::{
    args::{Pool, SendArgs},
    runtime::{InstanceName, Runtime},
};

fn check_send(args: &SendArgs) -> Result<()> {
    if args.from == args.to && args.source_pool == args.destination_pool {
        bail!("choose a different --to account or a different --destination-pool");
    }
    if args.memo.is_some() && args.destination_pool == Pool::Transparent {
        bail!(
            "--memo requires --destination-pool ironwood; transparent outputs cannot carry a memo"
        );
    }
    Ok(())
}

pub(super) fn send(
    runtime: &Runtime,
    name: &InstanceName,
    args: SendArgs,
    json: bool,
) -> Result<()> {
    check_send(&args)?;
    runtime.wallet_send(
        name,
        args.from,
        args.to,
        args.source_pool.as_str(),
        args.destination_pool.as_str(),
        args.amount.zatoshi(),
        args.memo.as_deref(),
        json,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/commands/wallet/tests.rs"]
mod tests;
