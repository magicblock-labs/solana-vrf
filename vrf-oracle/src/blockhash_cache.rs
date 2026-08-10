use serde_json::json;
use solana_client::nonblocking;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_response::{Response, RpcBlockhash};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
pub struct BlockhashCache {
    inner: Arc<RwLock<CacheData>>,
    client: Arc<nonblocking::rpc_client::RpcClient>,
    refresh_lock: Arc<Mutex<()>>,
}

struct CacheData {
    blockhash: Hash,
    slot: u64,
    timestamp: Instant,
}

impl CacheData {
    fn apply(&mut self, blockhash: Hash, slot: u64) {
        if slot < self.slot {
            return;
        }
        self.blockhash = blockhash;
        self.slot = slot;
        self.timestamp = Instant::now();
    }
}

impl BlockhashCache {
    pub async fn new(client: Arc<nonblocking::rpc_client::RpcClient>) -> Self {
        let (blockhash, slot) = Self::fetch_blockhash_and_slot(&client).await.unwrap();
        let inner = Arc::new(RwLock::new(CacheData {
            blockhash,
            slot,
            timestamp: Instant::now(),
        }));

        let cache = Self {
            inner,
            client,
            refresh_lock: Arc::new(Mutex::new(())),
        };

        cache.spawn_refresh_task();
        cache
    }

    fn spawn_refresh_task(&self) {
        let cache = self.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                cache.refresh_if_stale(Duration::from_secs(60)).await;
            }
        });
    }

    #[allow(dead_code)]
    pub async fn get_blockhash(&self) -> Hash {
        let cache = self.inner.read().await;
        cache.blockhash
    }

    pub async fn get_blockhash_and_slot(&self) -> (Hash, u64) {
        let cache = self.inner.read().await;
        (cache.blockhash, cache.slot)
    }

    /// Push writer for subscription-fed sources (e.g. LaserStream blocks_meta).
    /// Ignores updates older than the cached slot.
    pub async fn set_blockhash(&self, blockhash: Hash, slot: u64) {
        self.inner.write().await.apply(blockhash, slot);
    }

    pub async fn refresh_blockhash(&self) {
        if let Ok((blockhash, slot)) = Self::fetch_blockhash_and_slot(&self.client).await {
            self.inner.write().await.apply(blockhash, slot);
        }
    }

    /// Refresh only when the cached value is older than `max_age`; retry loops
    /// call this instead of refresh_blockhash so a hot loop cannot fetch faster.
    pub async fn refresh_if_stale(&self, max_age: Duration) {
        let _refresh_guard = self.refresh_lock.lock().await;
        let stale = { self.inner.read().await.timestamp.elapsed() > max_age };
        if stale {
            self.refresh_blockhash().await;
        }
    }

    async fn fetch_blockhash_and_slot(
        client: &nonblocking::rpc_client::RpcClient,
    ) -> anyhow::Result<(Hash, u64)> {
        let resp: Response<RpcBlockhash> = client
            .send(
                RpcRequest::GetLatestBlockhash,
                json!([CommitmentConfig::processed()]),
            )
            .await?;
        let blockhash = resp.value.blockhash.parse()?;
        Ok((blockhash, resp.context.slot))
    }
}
