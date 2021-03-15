use futures::{future::ready, stream, StreamExt};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tokio::{task::JoinHandle, time};
use web3::types::{Block, BlockId, BlockNumber, H256, U64};

use zksync_config::ZkSyncConfig;
use zksync_eth_client::{EthereumGateway, MultiplexerEthereumClient};
use zksync_utils::retry_opt;

/// Watcher which checks multiplexed client once within specified timeout.
pub struct MultiplexedGatewayWatcher {
    /// Any kind of client to be verified.
    client: MultiplexerEthereumClient,
    /// How often client will be checked. In milliseconds.
    interval: Duration,
    /// Time to wait before request again in case of unsuccessful request. In milliseconds.
    retry_delay: Duration,
    /// Max request timeout. In milliseconds.
    req_timeout: Duration,
    /// How many requests are allowed to be done within a single task.
    req_per_task_limit: Option<usize>,
    /// How many tasks are allowed to simultaneously make requests.
    task_limit: Option<usize>,
}

#[derive(Error, Debug, PartialEq)]
enum BlockVerificationError {
    #[error("Hash verification failed: {0:?} != {1:?}")]
    IncorrectHash(H256, H256),
    #[error("Difference between block numbers is greater than 1: {0:?} > {1:?}")]
    LargeNumDiff(U64, U64),
    #[error("Invalid block: {0:?}")]
    InvalidBlock(Box<Block<H256>>),
}

const MAX_BLOCK_NUMBER_DIFFERENCE: u64 = 1;

impl MultiplexedGatewayWatcher {
    /// Instantiates `MultiplexedGatewayWatcher` for provided multiplexed ethereum gateway.
    ///
    /// # Panics
    ///
    /// If given ethereum gateway is not `Multiplexed`.
    pub fn new(
        gateway: EthereumGateway,
        interval: Duration,
        retry_delay: Duration,
        req_timeout: Duration,
        req_per_task_limit: Option<usize>,
        task_limit: Option<usize>,
    ) -> Self {
        Self {
            client: match gateway {
                EthereumGateway::Multiplexed(client) => client,
                _ => {
                    panic!("Ethereum Gateway Watcher: Multiplexed client expected")
                }
            },
            interval,
            retry_delay,
            req_timeout,
            req_per_task_limit,
            task_limit,
        }
    }

    pub async fn run(self) {
        vlog::info!("Ethereum Gateway Watcher started");

        time::interval(self.interval)
            .for_each_concurrent(self.task_limit, |_| self.check_client_gateways())
            .await
    }

    fn verify_blocks(
        latest_block: &Block<H256>,
        block_to_check: &Block<H256>,
    ) -> Result<(), BlockVerificationError> {
        macro_rules! block_opt {
            ($block: expr, $opt: ident) => {
                $block
                    .$opt
                    .ok_or_else(|| BlockVerificationError::InvalidBlock(Box::new($block.clone())))?
            };
        }

        let (last_parent_hash, last_hash, last_num) = (
            latest_block.parent_hash,
            block_opt!(latest_block, hash),
            block_opt!(latest_block, number),
        );
        let (hash, num) = (
            block_opt!(block_to_check, hash),
            block_opt!(block_to_check, number),
        );

        if last_num - num > U64::from(MAX_BLOCK_NUMBER_DIFFERENCE) {
            Err(BlockVerificationError::LargeNumDiff(last_num, num))
        } else if last_num == num && last_hash != hash {
            Err(BlockVerificationError::IncorrectHash(last_hash, hash))
        } else if last_num == num + U64::one() && last_parent_hash != hash {
            Err(BlockVerificationError::IncorrectHash(
                last_parent_hash,
                hash,
            ))
        } else {
            Ok(())
        }
    }

    async fn check_client_gateways(&self) {
        // Fetch latest block for each client.
        // Each request will resolve to (client key, client latest block) pair.
        let latest_block_reqs: Vec<_> =
            self.client
                .clients()
                .map(|(key, client)| async move {
                    let block_fut = retry_opt! {
                        client
                            .block(BlockId::from(BlockNumber::Latest))
                            .await
                            .ok()
                            .flatten(),
                        vlog::error!("Request to Ethereum Gateway `{}` failed", key),
                        self.retry_delay,
                        self.req_timeout
                    };

                    if let Ok(block) = block_fut.await {
                        Some((key, block))
                    } else {
                        vlog::error!(
                            "Failed to get latest block from Ethereum Gateway `{}` within specified timeout",
                            key
                        );
                        None
                    }
                })
                .collect();

        // Execute all requests concurrently.
        // Max amount of concurrent tasks is limited by `req_per_task_limit`.
        let client_latest_blocks = stream::iter(latest_block_reqs.into_iter())
            .buffer_unordered(self.req_per_task_limit.unwrap_or(usize::MAX))
            .filter_map(ready)
            .collect::<Vec<_>>()
            .await;

        // Latest hash distribution across all clients.
        let hash_counts = client_latest_blocks
            .iter()
            .fold(HashMap::new(), |mut map, (_, cur)| {
                map.entry(&cur.hash)
                    .and_modify(|val| *val += 1)
                    .or_insert(1);
                map
            });

        // Preferred client must have longest chain with the most frequent hash and
        // lowest latency in its category.
        let preferred_client =
            client_latest_blocks
                .iter()
                .rev()
                .max_by(
                    |(_, block1), (_, block2)| match block1.number.cmp(&block2.number) {
                        Ordering::Equal => hash_counts
                            .get(&block1.hash)
                            .cmp(&hash_counts.get(&block2.hash)),
                        other => other,
                    },
                );

        if let Some((preferred_client_name, latest_block)) = preferred_client {
            if self.client.prioritize_client(preferred_client_name) {
                vlog::info!("Prioritized Ethereum Gateway: `{}`", preferred_client_name);
            }
            for (key, block) in &client_latest_blocks {
                if let Err(err) = Self::verify_blocks(latest_block, block) {
                    vlog::error!("Ethereum Gateway `{}` - check failed: {}", key, err);
                }
            }
        }
    }
}

#[must_use]
pub fn run_multiplexed_gateway_watcher(
    eth_gateway: EthereumGateway,
    config: &ZkSyncConfig,
) -> JoinHandle<()> {
    let gateway_watcher = MultiplexedGatewayWatcher::new(
        eth_gateway,
        config.gateway_watcher.check_interval(),
        config.gateway_watcher.retry_delay(),
        config.gateway_watcher.request_timeout(),
        Some(config.gateway_watcher.request_per_task_limit()),
        Some(config.gateway_watcher.task_limit()),
    );

    tokio::spawn(gateway_watcher.run())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_depth_block_hash_check() {
        let h1 = H256::random();
        let h2 = H256::random();
        let mut b1 = Block::default();
        let mut b2 = Block::default();

        b1.hash = Some(h1);
        b2.hash = Some(h1);
        b1.number = Some(U64::from(1u64));
        b2.number = Some(U64::from(1u64));

        assert_eq!(MultiplexedGatewayWatcher::verify_blocks(&b1, &b2), Ok(()));

        b2.hash = Some(h2);

        assert_eq!(
            MultiplexedGatewayWatcher::verify_blocks(&b1, &b2),
            Err(BlockVerificationError::IncorrectHash(h1, h2))
        );
    }

    #[test]
    fn test_different_depth_block_hash_check() {
        let h1 = H256::random();
        let h2 = H256::random();
        let mut b1 = Block::default();
        let mut b2 = Block::default();

        b1.hash = Some(h1);
        b1.parent_hash = h2;
        b2.hash = Some(h2);
        b1.number = Some(U64::from(1u64));
        b2.number = Some(U64::from(0u64));

        assert_eq!(MultiplexedGatewayWatcher::verify_blocks(&b1, &b2), Ok(()));

        b2.hash = Some(h1);

        assert_eq!(
            MultiplexedGatewayWatcher::verify_blocks(&b1, &b2),
            Err(BlockVerificationError::IncorrectHash(h2, h1))
        );
    }

    #[test]
    fn test_block_incorrect_depth_check() {
        let h1 = H256::random();
        let h2 = H256::random();
        let mut b1 = Block::default();
        let mut b2 = Block::default();

        b1.hash = Some(h1);
        b2.hash = Some(h2);
        b1.number = Some(U64::from(2u64));
        b2.number = Some(U64::from(0u64));

        assert_eq!(
            MultiplexedGatewayWatcher::verify_blocks(&b1, &b2),
            Err(BlockVerificationError::LargeNumDiff(
                U64::from(2u64),
                U64::from(0u64)
            ))
        );
    }
}
