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
        self.broadcast_until(
            raw_transaction,
            tokio::time::Instant::now() + LIGHTWALLETD_BROADCAST_TIMEOUT,
        )
        .await
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wallet/submission/tests.rs"]
mod tests;

impl RealWallet {
    pub(crate) async fn broadcast_until(
        &self,
        raw_transaction: &[u8],
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let result = self
            .send_transaction_until(raw_transaction.to_vec(), deadline)
            .await?;
        check_broadcast_response(result)
    }
}

impl RealWallet {
    async fn send_transaction_until(
        &self,
        data: Vec<u8>,
        deadline: tokio::time::Instant,
    ) -> Result<SendResponse> {
        let result = crate::reconcile::within_deadline(
            deadline,
            "broadcasting lightwalletd transaction",
            async {
                let mut client =
                    CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
                Ok::<_, anyhow::Error>(
                    client
                        .send_transaction(RawTransaction { data, height: 0 })
                        .await?
                        .into_inner(),
                )
            },
        )
        .await;
        match result {
            Err(error)
                if error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some() =>
            {
                Err(error.context("lightwalletd transaction broadcast timed out"))
            }
            result => result,
        }
    }
}
