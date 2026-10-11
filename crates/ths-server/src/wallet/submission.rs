use super::*;

const LIGHTWALLETD_BROADCAST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn check_broadcast_response(response: SendResponse) -> Result<()> {
    // Zakura reports AlreadyQueued for a hash already being downloaded. The
    // submitted bytes are unchanged, but queue acceptance is not confirmation.
    // Keep all other node rejections as errors, including a full download queue.
    if response.error_code != 0
        && response.error_message != "transaction dropped because it is already queued for download"
    {
        bail!(
            "lightwalletd rejected transaction: {}",
            response.error_message
        );
    }
    Ok(())
}

impl RealWallet {
    pub async fn broadcast(&self, raw_transaction: &[u8]) -> Result<()> {
        let result = self.send_transaction(raw_transaction.to_vec()).await?;
        check_broadcast_response(result)
    }

    pub(super) async fn send_transaction(&self, data: Vec<u8>) -> Result<SendResponse> {
        tokio::time::timeout(LIGHTWALLETD_BROADCAST_TIMEOUT, async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            Ok::<_, anyhow::Error>(
                client
                    .send_transaction(RawTransaction { data, height: 0 })
                    .await?
                    .into_inner(),
            )
        })
        .await
        .context("lightwalletd transaction broadcast timed out")?
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wallet/submission/tests.rs"]
mod tests;
