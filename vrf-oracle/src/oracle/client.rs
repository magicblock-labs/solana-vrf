use anyhow::Result;
use async_trait::async_trait;
use solana_client::{
    pubsub_client::PubsubClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use helius_laserstream::{
    grpc::{
        subscribe_request_filter_accounts_filter::Filter as AccountsFilterOneof,
        subscribe_request_filter_accounts_filter_memcmp::Data as AccountsFilterMemcmpOneof,
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeRequestFilterAccountsFilterMemcmp, SubscribeRequestFilterBlocksMeta,
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
        let blockhash_cache = Arc::new(BlockhashCache::new(Arc::clone(&rpc_client)).await);
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

            let (stream, _handle) = subscribe(
                config,
                SubscribeRequest {
                    accounts: filters,
                    blocks_meta,
                    ..Default::default()
                },
            );
            Ok(Box::new(LaserstreamSource {
                stream: Box::pin(stream),
                blockhash_cache: Arc::clone(blockhash_cache),
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
            Ok(Box::new(WebSocketSource {
                client,
                subscription: sub,
            }))
        }
    }
}
