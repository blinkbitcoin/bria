use bdk::FeeRate;
use tracing::instrument;

use super::{blockstream::*, config::*, error::*, mempool_space::*};
use crate::primitives::TxPriority;

#[derive(Clone, Debug)]
pub struct FeesClient {
    mempool_space: MempoolSpaceClient,
    blockstream: BlockstreamClient,
}

impl FeesClient {
    pub fn new(config: FeesConfig) -> Self {
        Self {
            mempool_space: MempoolSpaceClient::new(config.mempool_space),
            blockstream: BlockstreamClient::new(config.blockstream),
        }
    }

    #[instrument(name = "fees.fee_rate", skip(self), fields(fee_rate), err)]
    pub async fn fee_rate(&self, priority: TxPriority) -> Result<FeeRate, FeeEstimationError> {
        let fee_rate = match self.mempool_space.fee_rate(priority).await {
            Ok(fee_rate) => fee_rate,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "mempool_space fee estimation failed, falling back to blockstream"
                );
                self.blockstream.fee_rate(priority).await?
            }
        };
        tracing::Span::current()
            .record("fee_rate", tracing::field::display(format!("{fee_rate:?}")));
        Ok(fee_rate)
    }
}
