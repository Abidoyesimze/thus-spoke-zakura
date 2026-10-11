use super::accounts::derived_addresses;
use super::*;

impl Store {
    pub fn initialize(&self) -> Result<()> {
        let db = self.0.lock().unwrap();
        db.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, unified_address TEXT NOT NULL,
                transparent_address TEXT NOT NULL, transparent_zatoshi INTEGER NOT NULL DEFAULT 0,
                ironwood_zatoshi INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS activity (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER, to_account INTEGER NOT NULL,
                source_pool TEXT NOT NULL, destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS idempotency (
                key TEXT PRIMARY KEY, activity_id TEXT NOT NULL, memo TEXT
            );
            CREATE TABLE IF NOT EXISTS prepared_payments (
                activity_id TEXT PRIMARY KEY, raw_transaction BLOB NOT NULL,
                expiry_height INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS address_faucets (
                key TEXT PRIMARY KEY, id TEXT UNIQUE NOT NULL, address TEXT NOT NULL,
                amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL DEFAULT '',
                block_hash TEXT, status TEXT NOT NULL DEFAULT 'preparing'
            );
        "#)?;
        let has_memo = db
            .prepare("PRAGMA table_info(idempotency)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "memo");
        if !has_memo {
            db.execute("ALTER TABLE idempotency ADD COLUMN memo TEXT", [])?;
        }
        let stored_seed = db
            .query_row("SELECT value FROM metadata WHERE key='seed'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let seed = if let Some(seed) = stored_seed {
            hex::decode(seed).context("invalid wallet seed")?
        } else {
            let entropy = [0u8; 32];
            let mnemonic = Mnemonic::from_entropy(&entropy)
                .context("encoding wallet entropy as a BIP-39 mnemonic")?;
            let seed = mnemonic.to_seed("");
            db.execute(
                "INSERT INTO metadata(key, value) VALUES('seed', ?1)",
                [hex::encode(seed)],
            )?;
            db.execute(
                "INSERT INTO metadata(key, value) VALUES('mnemonic', ?1)",
                [mnemonic.to_string()],
            )?;
            seed.to_vec()
        };
        for id in 1u8..=TREASURY_ACCOUNT_ID {
            let (ua, taddr) = derived_addresses(&seed, id)?;
            db.execute("INSERT OR IGNORE INTO accounts(id,name,unified_address,transparent_address) VALUES(?1,?2,?3,?4)", params![id, format!("Account {id}"), ua, taddr])?;
        }
        Ok(())
    }

    pub fn seed(&self) -> Result<String> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT value FROM metadata WHERE key='seed'",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn mnemonic(&self) -> Result<String> {
        self.0
            .lock()
            .unwrap()
            .query_row("SELECT value FROM metadata WHERE key='mnemonic'", [], |r| {
                r.get(0)
            })
            .context("wallet mnemonic is missing; restart this disposable environment")
    }
}

#[cfg(test)]
#[path = "../../tests/unit/db/schema/tests.rs"]
mod tests;
