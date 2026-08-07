use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_client::{
    client_error::ClientError,
    pubsub_client::PubsubClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_request::RpcRequest,
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use helius_laserstream::{
    grpc::{
        subscribe_request_filter_accounts_filter::Filter as AccountsFilterOneof,
        subscribe_request_filter_accounts_filter_memcmp::Data as AccountsFilterMemcmpOneof,
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeRequestFilterAccountsFilterMemcmp, SubscribeRequestFilterBlocksMeta,
        SubscribeRequestFilterSlots,
    },
    subscribe, LaserstreamConfig,
};

use crate::blockhash_cache::BlockhashCache;
use crate::oracle::processor::{fetch_and_process_program_accounts, process_oracle_queue};
use crate::oracle::sources::{LaserstreamSource, WebSocketSource};
use crate::oracle::utils::queue_memcmp_filter;
use curve25519_dalek::{RistrettoPoint, Scalar};
use ephemeral_vrf::vrf::generate_vrf_keypair;
use ephemeral_vrf_api::prelude::AccountDiscriminator;
use ephemeral_vrf_api::{prelude::Queue, ID as PROGRAM_ID};
use log::{error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signer::Signer;

pub type RequestId = [u8; 32];
pub type QueueKey = String;
pub type InflightById = HashMap<RequestId, u64>;
pub type InflightRequestsMap = HashMap<QueueKey, InflightById>;
pub type ActiveTasksById = HashMap<RequestId, JoinHandle<()>>;
pub type ActiveTasksMap = HashMap<QueueKey, ActiveTasksById>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DelegationStatusResponse {
    is_delegated: bool,
}

const EARLY_SEND_DIVISOR: u32 = 20;
const EARLY_SEND_MAX: Duration = Duration::from_millis(20);
const NON_ER_EARLY_SEND_BONUS: Duration = Duration::from_millis(50);
const NON_ER_MIN_SLOT_DURATION: Duration = Duration::from_millis(200);
const MIN_SLOT_SAMPLE: Duration = Duration::from_millis(1);
const MIN_SLOT_SAMPLES: u8 = 3;

#[derive(Clone)]
pub struct SlotTracker {
    sender: watch::Sender<u64>,
    timing: Arc<Mutex<SlotTiming>>,
}

#[derive(Default)]
struct SlotTiming {
    last_boundary: Option<(u64, Instant)>,
    slot_duration: Option<Duration>,
    samples: u8,
    predictive_lead_bonus: Duration,
}

impl SlotTracker {
    fn early_send_lead(slot_duration: Duration) -> Duration {
        (slot_duration / EARLY_SEND_DIVISOR).min(EARLY_SEND_MAX)
    }

    fn new() -> Self {
        let (sender, _) = watch::channel(0);
        Self {
            sender,
            timing: Arc::new(Mutex::new(SlotTiming::default())),
        }
    }

    fn set_is_er(&self, is_er: bool) {
        self.timing.lock().unwrap().predictive_lead_bonus = if is_er {
            Duration::ZERO
        } else {
            NON_ER_EARLY_SEND_BONUS
        };
    }

    pub fn current(&self) -> u64 {
        *self.sender.borrow()
    }

    pub fn update(&self, slot: u64) {
        self.sender.send_if_modified(|current| {
            if slot > *current {
                *current = slot;
                true
            } else {
                false
            }
        });
    }

    pub fn observe_slot(&self, slot: u64) {
        self.observe_slot_at(slot, Instant::now());
    }

    fn observe_slot_at(&self, slot: u64, now: Instant) {
        if slot < self.current() {
            return;
        }
        let mut timing = self.timing.lock().unwrap();
        let mut timing_changed = false;
        if let Some((previous_slot, previous_at)) = timing.last_boundary {
            if slot > previous_slot {
                let slot_delta = (slot - previous_slot).min(u32::MAX as u64) as u32;
                let sample = now.duration_since(previous_at) / slot_delta;
                if sample >= MIN_SLOT_SAMPLE {
                    timing.slot_duration = Some(match timing.slot_duration {
                        Some(duration) => (duration * 3 + sample) / 4,
                        None => sample,
                    });
                    timing.samples = timing.samples.saturating_add(1);
                } else {
                    timing.slot_duration = None;
                    timing.samples = 0;
                }
                timing.last_boundary = Some((slot, now));
                timing_changed = true;
            }
        } else {
            timing.last_boundary = Some((slot, now));
            timing_changed = true;
        }
        drop(timing);
        if timing_changed {
            self.sender
                .send_modify(|current| *current = (*current).max(slot));
        } else {
            self.update(slot);
        }
    }

    fn early_window(&self, target: u64) -> Option<(Instant, Instant)> {
        let timing = self.timing.lock().unwrap();
        if timing.samples < MIN_SLOT_SAMPLES {
            return None;
        }
        let (slot, observed_at) = timing.last_boundary?;
        let slot_duration = timing.slot_duration?;
        let slots = u32::try_from(target.checked_sub(slot)?).ok()?;
        let predicted = observed_at + slot_duration.checked_mul(slots)?;
        let lead = Self::early_send_lead(slot_duration);
        let predictive_lead = lead
            + if slot_duration >= NON_ER_MIN_SLOT_DURATION {
                timing.predictive_lead_bonus
            } else {
                Duration::ZERO
            };
        Some((predicted.checked_sub(predictive_lead)?, predicted + lead))
    }

    pub fn early_send_grace(&self) -> Duration {
        let timing = self.timing.lock().unwrap();
        timing
            .slot_duration
            .map(Self::early_send_lead)
            .unwrap_or_default()
    }

    pub async fn wait_for_slot(&self, target: u64) {
        let mut receiver = self.sender.subscribe();
        while *receiver.borrow_and_update() < target {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    pub async fn wait_for_slot_early(&self, target: u64) {
        let mut receiver = self.sender.subscribe();
        while *receiver.borrow_and_update() < target {
            if let Some((deadline, expires)) = self.early_window(target) {
                if Instant::now() > expires {
                    if receiver.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return,
                    result = receiver.changed() => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            } else if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SlotTracker;
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn slot_tracker_predicts_an_early_boundary() {
        let tracker = SlotTracker::new();
        let start = Instant::now();

        tracker.observe_slot_at(8, start);
        tracker.observe_slot_at(9, start + Duration::from_millis(400));
        tracker.observe_slot_at(10, start + Duration::from_millis(800));
        assert!(tracker.early_window(11).is_none());

        // Account progress may arrive before the boundary notification.
        tracker.update(11);
        tracker.observe_slot_at(11, start + Duration::from_millis(1200));

        assert_eq!(
            tracker.early_window(12),
            Some((
                start + Duration::from_millis(1580),
                start + Duration::from_millis(1620)
            ))
        );
        assert_eq!(tracker.early_send_grace(), Duration::from_millis(20));

        tracker.set_is_er(false);
        let (deadline, expires) = tracker.early_window(12).unwrap();
        assert_eq!(deadline, start + Duration::from_millis(1530));
        assert_eq!(expires, start + Duration::from_millis(1620));
        assert_eq!(tracker.early_send_grace(), Duration::from_millis(20));

        tracker.set_is_er(true);
        assert_eq!(
            tracker.early_window(12).unwrap().0,
            start + Duration::from_millis(1580)
        );

        let fast_tracker = SlotTracker::new();
        fast_tracker.set_is_er(false);
        fast_tracker.observe_slot_at(8, start);
        fast_tracker.observe_slot_at(9, start + Duration::from_millis(50));
        fast_tracker.observe_slot_at(10, start + Duration::from_millis(100));
        fast_tracker.observe_slot_at(11, start + Duration::from_millis(150));
        assert_eq!(
            fast_tracker.early_window(12).unwrap().0,
            start + Duration::from_micros(197_500)
        );
    }

    #[tokio::test]
    async fn slot_tracker_can_wake_before_the_target_slot() {
        let tracker = SlotTracker::new();
        let now = Instant::now();
        tracker.observe_slot_at(8, now - Duration::from_millis(200));
        tracker.observe_slot_at(9, now - Duration::from_millis(150));
        tracker.observe_slot_at(10, now - Duration::from_millis(100));
        tracker.update(11);

        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_for_slot_early(12).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        // The boundary observation must wake the waiter even though account
        // progress already advanced the watched slot to the same value.
        tracker.observe_slot_at(11, now - Duration::from_millis(50));

        tokio::time::timeout(Duration::from_millis(10), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tracker.current(), 11);
    }

    #[tokio::test]
    async fn slot_tracker_ignores_a_stale_prediction() {
        let tracker = SlotTracker::new();
        let now = Instant::now();
        tracker.observe_slot_at(8, now - Duration::from_millis(800));
        tracker.observe_slot_at(9, now - Duration::from_millis(700));
        tracker.observe_slot_at(10, now - Duration::from_millis(600));
        tracker.observe_slot_at(11, now - Duration::from_millis(500));

        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_for_slot_early(12).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        tracker.update(12);
        tokio::time::timeout(Duration::from_millis(10), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn slot_tracker_is_monotonic_and_race_free() {
        let tracker = SlotTracker::new();
        tracker.update(4);
        tracker.update(3);

        assert_eq!(tracker.current(), 4);
        tracker.wait_for_slot(4).await;

        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_for_slot(5).await })
        };
        tokio::task::yield_now().await;
        tracker.update(5);

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();

        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_for_slot(7).await })
        };
        tokio::task::yield_now().await;
        tracker.update(6);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        tracker.update(7);

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}

pub struct OracleClient {
    pub keypair: Keypair,
    pub rpc_url: String,
    pub websocket_url: String,
    pub oracle_vrf_sk: Scalar,
    pub oracle_vrf_pk: RistrettoPoint,
    pub laserstream_api_key: Option<String>,
    pub laserstream_endpoint: Option<String>,
    pub queue_stats: Arc<RwLock<HashMap<String, usize>>>,
    // Average response slots per queue (running average)
    pub avg_response_slots: Arc<RwLock<HashMap<String, f64>>>,
    // Response counts per queue to compute running average
    pub response_counts: Arc<RwLock<HashMap<String, u64>>>,
    // In-flight requests per queue: request_id -> enqueue slot
    pub inflight_requests: Arc<RwLock<InflightRequestsMap>>,
    // Active task handles per queue: request_id -> JoinHandle
    pub active_tasks: Arc<RwLock<ActiveTasksMap>>,
    // Whether to skip preflight when sending transactions
    pub skip_preflight: bool,
    pub slot_tracker: SlotTracker,
    delegated_queue_statuses: Arc<RwLock<Option<HashMap<Pubkey, bool>>>>,
}

#[async_trait]
pub trait QueueUpdateSource: Send {
    // Returns: (queue pubkey, queue data, full account bytes, optional notification slot)
    async fn next(&mut self) -> Option<(Pubkey, Queue, Vec<u8>, u64)>;
}

impl OracleClient {
    pub fn new(
        keypair: Keypair,
        rpc_url: String,
        websocket_url: String,
        laserstream_endpoint: Option<String>,
        laserstream_api_key: Option<String>,
        skip_preflight: bool,
    ) -> Self {
        let (oracle_vrf_sk, oracle_vrf_pk) = generate_vrf_keypair(&keypair);
        Self {
            keypair,
            rpc_url,
            websocket_url,
            oracle_vrf_sk,
            oracle_vrf_pk,
            laserstream_api_key,
            laserstream_endpoint,
            queue_stats: Arc::new(RwLock::new(HashMap::new())),
            avg_response_slots: Arc::new(RwLock::new(HashMap::new())),
            response_counts: Arc::new(RwLock::new(HashMap::new())),
            inflight_requests: Arc::new(RwLock::new(HashMap::new())),
            active_tasks: Arc::new(RwLock::new(HashMap::new())),
            skip_preflight,
            slot_tracker: SlotTracker::new(),
            delegated_queue_statuses: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!(
            "Starting VRF Oracle with public key: {}",
            self.keypair.pubkey()
        );
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            self.rpc_url.clone(),
            CommitmentConfig::processed(),
        ));
        self.initialize_delegated_queue_filter(&rpc_client).await?;
        let blockhash_cache = Arc::new(BlockhashCache::new(Arc::clone(&rpc_client)).await);
        let (_, initial_slot) = blockhash_cache.get_blockhash_and_slot().await;
        self.slot_tracker.update(initial_slot);
        fetch_and_process_program_accounts(
            &self,
            &rpc_client,
            &blockhash_cache,
            queue_memcmp_filter(),
        )
        .await?;

        // Periodically refresh and process program accounts every 30 seconds
        {
            let self_clone = Arc::clone(&self);
            let rpc_client_clone = Arc::clone(&rpc_client);
            let blockhash_cache_clone = Arc::clone(&blockhash_cache);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    if let Err(err) = fetch_and_process_program_accounts(
                        &self_clone,
                        &rpc_client_clone,
                        &blockhash_cache_clone,
                        queue_memcmp_filter(),
                    )
                    .await
                    {
                        error!("Periodic fetch_and_process_program_accounts failed: {err:?}");
                    }
                }
            });
        }

        loop {
            match self.create_update_source(&blockhash_cache).await {
                Ok(mut source) => {
                    info!("Update source connected successfully");
                    while let Some((pubkey, queue, bytes, notification_slot)) = source.next().await
                    {
                        let bytes = Arc::new(bytes);
                        process_oracle_queue(
                            &self,
                            &rpc_client,
                            &blockhash_cache,
                            &pubkey,
                            &queue,
                            bytes,
                            Some(notification_slot),
                        )
                        .await;
                    }
                    drop(source);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    warn!("Update source stream ended. Attempting to reconnect...");
                }
                Err(err) => {
                    error!("Failed to create update source: {err:?}. Retrying in 5 seconds...");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn initialize_delegated_queue_filter(&self, rpc_client: &RpcClient) -> Result<()> {
        let version = rpc_client
            .send::<Value>(RpcRequest::GetVersion, Value::Null)
            .await?;
        let is_er = version.get("magicblock-core").is_some();
        self.slot_tracker.set_is_er(is_er);
        if is_er {
            *self.delegated_queue_statuses.write().await = Some(HashMap::new());
        }
        Ok(())
    }

    pub async fn should_process_queue(&self, rpc_client: &RpcClient, queue: &Pubkey) -> bool {
        {
            let statuses = self.delegated_queue_statuses.read().await;
            let Some(statuses) = statuses.as_ref() else {
                return true;
            };
            if let Some(is_delegated) = statuses.get(queue) {
                return *is_delegated;
            }
        }

        match Self::get_delegation_status(rpc_client, queue).await {
            Ok(is_delegated) => {
                if let Some(statuses) = self.delegated_queue_statuses.write().await.as_mut() {
                    statuses.insert(*queue, is_delegated);
                }
                if !is_delegated {
                    info!("Ignoring undelegated queue: {queue}");
                }
                is_delegated
            }
            Err(error) => {
                warn!("Failed to check delegation status for queue {queue}: {error}");
                false
            }
        }
    }

    async fn get_delegation_status(
        rpc_client: &RpcClient,
        queue: &Pubkey,
    ) -> Result<bool, ClientError> {
        rpc_client
            .send::<DelegationStatusResponse>(
                RpcRequest::Custom {
                    method: "getDelegationStatus",
                },
                json!([queue.to_string()]),
            )
            .await
            .map(|status| status.is_delegated)
    }

    async fn create_update_source(
        self: &Arc<Self>,
        blockhash_cache: &Arc<BlockhashCache>,
    ) -> Result<Box<dyn QueueUpdateSource>> {
        if let (Some(api_key), Some(endpoint)) =
            (&self.laserstream_api_key, &self.laserstream_endpoint)
        {
            info!("Connecting to gRPC: {endpoint}");
            let config = LaserstreamConfig {
                api_key: api_key.clone(),
                endpoint: endpoint.parse()?,
                ..Default::default()
            };

            let mut filters = HashMap::new();
            filters.insert(
                "oracle".to_string(),
                SubscribeRequestFilterAccounts {
                    owner: vec![PROGRAM_ID.to_string()],
                    filters: vec![SubscribeRequestFilterAccountsFilter {
                        filter: Some(AccountsFilterOneof::Memcmp(
                            SubscribeRequestFilterAccountsFilterMemcmp {
                                offset: 0,
                                data: Some(AccountsFilterMemcmpOneof::Bytes(
                                    AccountDiscriminator::Queue.to_bytes().to_vec(),
                                )),
                            },
                        )),
                    }],
                    ..Default::default()
                },
            );

            // Block metadata on the same stream feeds the blockhash cache.
            let mut blocks_meta = HashMap::new();
            blocks_meta.insert(
                "all".to_string(),
                SubscribeRequestFilterBlocksMeta::default(),
            );

            let mut slots = HashMap::new();
            slots.insert(
                "all".to_string(),
                SubscribeRequestFilterSlots {
                    filter_by_commitment: Some(false),
                    interslot_updates: Some(true),
                },
            );

            let (stream, _handle) = subscribe(
                config,
                SubscribeRequest {
                    accounts: filters,
                    slots,
                    blocks_meta,
                    ..Default::default()
                },
            );
            Ok(Box::new(LaserstreamSource {
                stream: Box::pin(stream),
                blockhash_cache: Arc::clone(blockhash_cache),
                slot_tracker: self.slot_tracker.clone(),
            }))
        } else {
            info!("Connecting to WebSocket: {}", self.websocket_url);
            let config = RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    commitment: Some(CommitmentConfig::processed()),
                    encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
                    ..Default::default()
                },
                filters: Some(queue_memcmp_filter()),
                ..Default::default()
            };
            let (client, sub) =
                PubsubClient::program_subscribe(&self.websocket_url, &PROGRAM_ID, Some(config))?;
            let (slot_client, slot_sub) = PubsubClient::slot_subscribe(&self.websocket_url)?;
            Ok(Box::new(WebSocketSource {
                client,
                subscription: sub,
                slot_client,
                slot_subscription: slot_sub,
                slot_tracker: self.slot_tracker.clone(),
            }))
        }
    }
}
