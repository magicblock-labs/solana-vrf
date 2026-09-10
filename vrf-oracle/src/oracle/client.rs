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
use solana_vrf_api::prelude::QUEUE_TTL_SECONDS;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
use log::{error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signer::Signer;
use solana_vrf::vrf::generate_vrf_keypair;
use solana_vrf_api::prelude::AccountDiscriminator;
use solana_vrf_api::{prelude::Queue, ID as PROGRAM_ID};

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

const PRIORITY_FEE_MAX_AGE: Duration = Duration::from_secs(10);
const PRIORITY_FEE_FETCH_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 10_000;
const EARLY_SEND_DIVISOR: u32 = 20;
const EARLY_SEND_MAX: Duration = Duration::from_millis(20);
const NON_ER_EARLY_SEND_BONUS: Duration = Duration::from_millis(400);
const NON_ER_MIN_SLOT_DURATION: Duration = Duration::from_millis(200);
const MIN_SLOT_SAMPLE: Duration = Duration::from_millis(1);
const MIN_SLOT_SAMPLES: u8 = 3;
/// Assumed slot duration before the tracker has measured one.
const NOMINAL_SLOT_DURATION: Duration = Duration::from_millis(500);

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

    /// Number of slots that currently approximates `QUEUE_TTL_SECONDS` of
    /// wall-clock time, using the measured slot duration so it tracks changes
    /// to Solana's block time. Falls back to a nominal rate until enough
    /// samples have been collected, so a single noisy measurement can't shrink
    /// the threshold and cause premature purging.
    pub fn ttl_slots(&self) -> u64 {
        let timing = self.timing.lock().unwrap();
        let slot_duration = if timing.samples >= MIN_SLOT_SAMPLES {
            timing.slot_duration.unwrap_or(NOMINAL_SLOT_DURATION)
        } else {
            NOMINAL_SLOT_DURATION
        };
        let slot_ms = slot_duration.as_millis().max(1);
        (QUEUE_TTL_SECONDS as u128 * 1000 / slot_ms) as u64
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
    priority_fee_cache: Arc<RwLock<Option<(u64, Instant)>>>,
    priority_fee_refresh: Arc<tokio::sync::Mutex<()>>,
    // Newest view slot processed per queue; older views are discarded.
    pub latest_view_slots: Arc<RwLock<HashMap<QueueKey, u64>>>,
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
            priority_fee_cache: Arc::new(RwLock::new(None)),
            priority_fee_refresh: Arc::new(tokio::sync::Mutex::new(())),
            latest_view_slots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Recommended priority fee in micro-lamports per CU, cached for
    /// PRIORITY_FEE_MAX_AGE. Zero on ephemeral rollups; falls back to the
    /// default when the endpoint does not support getPriorityFeeEstimate.
    pub async fn priority_fee(&self, rpc_client: &RpcClient) -> u64 {
        if self.delegated_queue_statuses.read().await.is_some() {
            return 0;
        }
        if let Some(fee) = self.cached_priority_fee().await {
            return fee;
        }
        // Serialize refreshes so concurrent tasks share one estimate.
        let _refresh_guard = self.priority_fee_refresh.lock().await;
        if let Some(fee) = self.cached_priority_fee().await {
            return fee;
        }
        // Bound the refresh so a slow endpoint cannot delay sends; fall back
        // to the last known value and retry next window.
        let fee = match tokio::time::timeout(
            PRIORITY_FEE_FETCH_TIMEOUT,
            Self::fetch_priority_fee(rpc_client),
        )
        .await
        {
            Ok(Ok(fee)) => fee,
            _ => (*self.priority_fee_cache.read().await)
                .map(|(fee, _)| fee)
                .unwrap_or(DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS),
        };
        *self.priority_fee_cache.write().await = Some((fee, Instant::now()));
        fee
    }

    async fn cached_priority_fee(&self) -> Option<u64> {
        (*self.priority_fee_cache.read().await)
            .filter(|(_, fetched_at)| fetched_at.elapsed() < PRIORITY_FEE_MAX_AGE)
            .map(|(fee, _)| fee)
    }

    async fn fetch_priority_fee(rpc_client: &RpcClient) -> Result<u64, ClientError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PriorityFeeEstimate {
            priority_fee_estimate: f64,
        }
        rpc_client
            .send::<PriorityFeeEstimate>(
                RpcRequest::Custom {
                    method: "getPriorityFeeEstimate",
                },
                json!([{
                    "accountKeys": [PROGRAM_ID.to_string()],
                    "options": { "recommended": true }
                }]),
            )
            .await
            .map(|estimate| estimate.priority_fee_estimate as u64)
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

        // Serialize full-program scans: the periodic and reconnect paths
        // share one flight so a slow RPC cannot accumulate overlapping scans.
        let scan_lock = Arc::new(tokio::sync::Mutex::new(()));
        let scan_queued = Arc::new(AtomicBool::new(false));

        // Periodically refresh and process program accounts every 5 seconds
        {
            let self_clone = Arc::clone(&self);
            let rpc_client_clone = Arc::clone(&rpc_client);
            let blockhash_cache_clone = Arc::clone(&blockhash_cache);
            let scan_lock_clone = Arc::clone(&scan_lock);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let _scan_guard = scan_lock_clone.lock().await;
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
                    // Requests that landed while the source was down produce no
                    // account notification: snapshot on every (re)connect,
                    // spawned so a slow scan cannot stall stream consumption
                    // and queued behind any scan already in flight (whose bank
                    // may predate the gap). At most one snapshot waits: a
                    // pending one covers every later gap as well.
                    if !scan_queued.swap(true, Ordering::SeqCst) {
                        let scan_lock = Arc::clone(&scan_lock);
                        let scan_queued = Arc::clone(&scan_queued);
                        let self_clone = Arc::clone(&self);
                        let rpc_client_clone = Arc::clone(&rpc_client);
                        let blockhash_cache_clone = Arc::clone(&blockhash_cache);
                        tokio::spawn(async move {
                            let _scan_guard = scan_lock.lock_owned().await;
                            scan_queued.store(false, Ordering::SeqCst);
                            if let Err(err) = fetch_and_process_program_accounts(
                                &self_clone,
                                &rpc_client_clone,
                                &blockhash_cache_clone,
                                queue_memcmp_filter(),
                            )
                            .await
                            {
                                error!(
                                    "Post-connect fetch_and_process_program_accounts failed: {err:?}"
                                );
                            }
                        });
                    }
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
                            false,
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
