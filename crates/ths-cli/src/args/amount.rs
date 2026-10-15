use std::str::FromStr;

use anyhow::{Result, bail};

fn parse_zec_zatoshi(value: &str) -> Result<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 8
    {
        bail!("amount must be a decimal ZEC value with at most 8 decimal places");
    }
    let whole = whole.parse::<u64>()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>()? * 10u64.pow(8 - fraction.len() as u32)
    };
    whole
        .checked_mul(100_000_000)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(|| anyhow::anyhow!("amount is too large"))
}

/// A ZEC amount limited to 5, matching the server's per-request faucet cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZecAmount(pub(super) u64);

impl ZecAmount {
    pub(crate) fn zatoshi(self) -> u64 {
        self.0
    }
}

impl FromStr for ZecAmount {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let zatoshi = parse_zec_zatoshi(value)?;
        if zatoshi == 0 || zatoshi > 500_000_000 {
            bail!("amount must be greater than zero and no more than 5 ZEC");
        }
        Ok(Self(zatoshi))
    }
}

/// A ZEC amount with no upper bound, for transfers between accounts that are
/// only limited by the sending account's balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SendAmount(pub(crate) u64);

impl SendAmount {
    pub(crate) fn zatoshi(self) -> u64 {
        self.0
    }
}

impl FromStr for SendAmount {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let zatoshi = parse_zec_zatoshi(value)?;
        if zatoshi == 0 {
            bail!("amount must be greater than zero");
        }
        Ok(Self(zatoshi))
    }
}

#[cfg(test)]
#[path = "../../tests/unit/args/amount/tests.rs"]
mod tests;
