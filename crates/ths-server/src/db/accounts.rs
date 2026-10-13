use super::*;

/// The order must match `row_account`.
const ACCOUNT_COLUMNS: &str =
    "id,name,unified_address,transparent_address,transparent_zatoshi,ironwood_zatoshi";
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: u8,
    pub name: String,
    pub unified_address: String,
    pub transparent_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_full_viewing_key: Option<String>,
    pub transparent_zatoshi: u64,
    pub ironwood_zatoshi: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentAccountSecret {
    pub id: u8,
    pub unified_address: String,
    pub unified_spending_key_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentSecrets {
    pub mnemonic: String,
    pub accounts: Vec<super::DevelopmentAccountSecret>,
}

impl Store {
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut accounts = {
            let db = self.0.lock().unwrap();
            let mut query = db.prepare(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY id"
            ))?;
            query
                .query_map([], row_account)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let seed = hex::decode(self.seed()?).context("invalid wallet seed")?;
        let network = regtest_network();
        for account in &mut accounts {
            if (1..=USER_ACCOUNT_COUNT).contains(&account.id) {
                account.unified_full_viewing_key =
                    Some(derived_full_viewing_key(&seed, account.id)?.encode(&network));
            }
        }
        Ok(accounts)
    }

    #[cfg(test)]
    pub fn user_accounts(&self) -> Result<Vec<Account>> {
        Ok(self
            .accounts()?
            .into_iter()
            .filter(|account| account.id <= USER_ACCOUNT_COUNT)
            .collect())
    }

    pub fn account(&self, id: u8) -> Result<Account> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id=?1"),
                [id],
                row_account,
            )
            .with_context(|| format!("account {id} does not exist"))
    }

    pub fn development_secrets(&self) -> Result<super::DevelopmentSecrets> {
        let seed = hex::decode(self.seed()?).context("invalid wallet seed")?;
        let mnemonic = self.mnemonic()?;
        let network = regtest_network();
        let mut accounts = Vec::with_capacity(usize::from(USER_ACCOUNT_COUNT));
        for id in 1..=USER_ACCOUNT_COUNT {
            let account_index = zip32::AccountId::try_from(u32::from(id - 1))
                .map_err(|_| anyhow::anyhow!("invalid ZIP-32 account {id}"))?;
            let spending_key = UnifiedSpendingKey::from_seed(&network, &seed, account_index)
                .map_err(|error| anyhow::anyhow!("deriving account {id}: {error:?}"))?;
            accounts.push(DevelopmentAccountSecret {
                id,
                unified_address: self.account(id)?.unified_address,
                unified_spending_key_hex: hex::encode(spending_key.to_bytes(Era::Orchard)),
            });
        }
        Ok(DevelopmentSecrets { mnemonic, accounts })
    }
}

fn row_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        name: row.get(1)?,
        unified_address: row.get(2)?,
        transparent_address: row.get(3)?,
        unified_full_viewing_key: None,
        transparent_zatoshi: row.get(4)?,
        ironwood_zatoshi: row.get(5)?,
    })
}
fn derived_full_viewing_key(seed: &[u8], id: u8) -> Result<UnifiedFullViewingKey> {
    let index = id.checked_sub(1).context("invalid account id")?;
    let account = zip32::AccountId::try_from(u32::from(index))
        .map_err(|_| anyhow::anyhow!("invalid ZIP-32 account {id}"))?;
    let usk = UnifiedSpendingKey::from_seed(&regtest_network(), seed, account)
        .map_err(|error| anyhow::anyhow!("deriving account {id}: {error:?}"))?;
    Ok(usk.to_unified_full_viewing_key())
}

pub(super) fn derived_addresses(seed: &[u8], id: u8) -> Result<(String, String)> {
    let network = regtest_network();
    let (ua, _) = derived_full_viewing_key(seed, id)?
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .map_err(|error| anyhow::anyhow!("deriving account {id} address: {error:?}"))?;
    let transparent = ua
        .transparent()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("account {id} has no transparent receiver"))?;
    Ok((
        ua.encode(&network),
        Address::Transparent(transparent).encode(&network),
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/db/accounts/tests.rs"]
mod tests;
