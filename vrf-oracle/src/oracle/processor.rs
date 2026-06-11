use crate::blockhash_cache::BlockhashCache;
use crate::oracle::client::OracleClient;
use anyhow::Result;
use ephemeral_vrf::vrf::{compute_vrf, verify_vrf};
use ephemeral_vrf_api::{
    prelude::{
        provide_randomness_with_identity_mode, purge_expired_requests, Queue, QueueAccount,
        QueueItem, QUEUE_TTL_SLOTS,
    },
    state::oracle_queue_pda,
    ID as PROGRAM_ID,
};
use futures_util::future::join_all;
use futures_util::FutureExt;
use log::{error, info, trace, warn};
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::RpcFilterType;
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_curve25519::{ristretto::PodRistrettoPoint, scalar::PodScalar};
use solana_sdk::{pubkey::Pubkey, signature::Signer, transaction::Transaction};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::task;
use tokio::time::sleep;

pub async fn fetch_and_process_program_accounts(
    oracle_client: &Arc<OracleClient>,
    rpc_client: &Arc<RpcClient>,
    blockhash_cache: &Arc<BlockhashCache>,
    filters: Vec<RpcFilterType>,
) -> Result<()> {
    let config = RpcProgramAccountsConfig {
        account_config: RpcAccountInfoConfig {
            commitment: Some(CommitmentConfig::processed()),
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        filters: Some(filters),
        ..Default::default()
    };

    let accounts = rpc_client
        .get_program_accounts_with_config(&PROGRAM_ID, config)
        .await?;

    let tasks = accounts.into_iter().filter_map(|(pubkey, acc)| {
        if acc.owner != PROGRAM_ID {
            return None;
        }

        let bytes = Arc::new(acc.data);
        let oracle_client = Arc::clone(oracle_client);
        let rpc_client = Arc::clone(rpc_client);
        let blockhash_cache = Arc::clone(blockhash_cache);

        Some(task::spawn(async move {
            let queue = match Queue::try_from_bytes(&bytes[..]) {
                Ok(q) => q,
                Err(e) => {
                    warn!("Invalid queue for account {}: {}", pubkey, e);
                    return;
                }
            };

            let result = std::panic::AssertUnwindSafe(async {
                process_oracle_queue(
                    &oracle_client,
                    &rpc_client,
                    &blockhash_cache,
                    &pubkey,
                    queue,
                    Arc::clone(&bytes),
                    None,
                )
                .await
            })
            .catch_unwind()
            .await;

            if let Err(e) = result {
                error!("Queue task for {pubkey} panicked: {:?}", e);
            }
        }))
    });

    join_all(tasks).await;
    Ok(())
}

pub async fn process_oracle_queue(
    oracle_client: &Arc<OracleClient>,
    rpc_client: &Arc<RpcClient>,
    blockhash_cache: &BlockhashCache,
    queue: &Pubkey,
    oracle_queue: &Queue,
    account_bytes: Arc<Vec<u8>>,
    notification_slot: Option<u64>,
) {
    if oracle_queue_pda(&oracle_client.keypair.pubkey(), oracle_queue.index).0 == *queue {
        if oracle_queue.item_count > 0 {
            info!(
                "Processing queue: {}, with len: {}",
                queue, oracle_queue.item_count
            );
        }

        // Update web-exposed queue size map
        {
            let mut stats = oracle_client.queue_stats.write().await;
            stats.insert(queue.to_string(), oracle_queue.item_count as usize);
        }

        // Build a set of current request IDs and a map of their enqueue slots from the queue
        let mut current_ids: HashSet<[u8; 32]> = HashSet::new();
        let mut current_slots_by_id: HashMap<[u8; 32], u64> = HashMap::new();

        // Construct a read-only view over the queue items using a local mutable copy
        let mut acc_bytes = account_bytes[8..].to_vec(); // strip discriminator
        let queue_account = match QueueAccount::load(&mut acc_bytes[..]) {
            Ok(q) => q,
            Err(e) => {
                warn!("Failed to load QueueAccount for {}: {}", queue, e);
                return;
            }
        };

        for item in queue_account.iter_items() {
            current_ids.insert(item.id);
            current_slots_by_id.insert(item.id, item.slot);
        }

        // Update in-flight tracking and compute latencies for completed requests
        let queue_key = queue.to_string();
        {
            let mut inflight_all = oracle_client.inflight_requests.write().await;
            let mut tasks_all = oracle_client.active_tasks.write().await;
            let inflight_for_queue = inflight_all.entry(queue_key.clone()).or_default();
            let tasks_for_queue = tasks_all.entry(queue_key.clone()).or_default();

            // Identify requests that were in-flight but are no longer present -> responded or purged
            let previously_tracked: Vec<[u8; 32]> = inflight_for_queue.keys().cloned().collect();
            for tracked_id in previously_tracked {
                if !current_ids.contains(&tracked_id) {
                    // Cancel any running task for this id
                    if let Some(handle) = tasks_for_queue.remove(&tracked_id) {
                        handle.abort();
                    }

                    // Remove from inflight and, if we have a response slot hint, update latency stats
                    if let Some(enqueue_slot) = inflight_for_queue.remove(&tracked_id) {
                        if let Some(response_slot_hint) = notification_slot {
                            let latency = response_slot_hint.saturating_sub(enqueue_slot) as f64;

                            // Update running average and count for this queue
                            {
                                let mut counts = oracle_client.response_counts.write().await;
                                let mut avgs = oracle_client.avg_response_slots.write().await;
                                let count = counts.entry(queue_key.clone()).or_insert(0);
                                let prev_avg = avgs.entry(queue_key.clone()).or_insert(0.0);
                                let new_avg = ((*prev_avg) * (*count as f64) + latency)
                                    / (*count as f64 + 1.0);
                                *count += 1;
                                *prev_avg = new_avg;
                            }
                        }
                    }
                }
            }
        }

        // Process items (send transactions)
        // Take an owned snapshot of the queue metadata and items so spawned tasks don't borrow `oracle_queue`.
        let queue_meta = Arc::new(*oracle_queue);
        let items: Vec<QueueItem> = queue_account.iter_items().collect();

        for item in items.into_iter() {
            let oracle_client = Arc::clone(oracle_client);
            let rpc_client = Arc::clone(rpc_client);
            let blockhash_cache = blockhash_cache.clone();
            let queue = *queue;
            let oracle_queue = Arc::clone(&queue_meta);
            let account_bytes_task = Arc::clone(&account_bytes);
            let input_seed = item.id;
            let queue_key_spawn = queue_key.clone();
            // Separate clones to satisfy borrow checker across awaits
            let oracle_client_for_proc = Arc::clone(&oracle_client);
            let oracle_client_for_cleanup = Arc::clone(&oracle_client);

            // Only spawn a task if this request is not already in-flight for this queue
            let should_spawn = {
                let mut inflight_all = oracle_client.inflight_requests.write().await;
                let inflight_for_queue = inflight_all.entry(queue_key_spawn.clone()).or_default();
                if let std::collections::hash_map::Entry::Vacant(e) =
                    inflight_for_queue.entry(item.id)
                {
                    e.insert(item.slot);
                    true
                } else {
                    false
                }
            };

            if !should_spawn {
                continue;
            }

            let handle = tokio::spawn(async move {
                let mut attempts = 0;
                let mut confirmed_success = false;

                while attempts < 100 {
                    match ProcessableItem(item)
                        .process_item(
                            &oracle_client_for_proc,
                            &rpc_client,
                            &blockhash_cache,
                            &input_seed,
                            &queue,
                            &oracle_queue,
                            account_bytes_task.as_slice(),
                        )
                        .await
                    {
                        Ok(signature) => {
                            trace!(
                                "Transaction: {}, for id {}",
                                signature,
                                Pubkey::new_from_array(item.id)
                            );
                            let sig = match signature.parse::<solana_sdk::signature::Signature>() {
                                Ok(sig) => sig,
                                Err(_) => {
                                    continue;
                                }
                            };

                            let result = rpc_client
                                .confirm_transaction_with_commitment(
                                    &sig,
                                    CommitmentConfig::processed(),
                                )
                                .await;

                            match result {
                                Ok(success) => {
                                    if success.value {
                                        info!(
                                            "Transaction successfully confirmed: {}, for id: {}",
                                            signature,
                                            Pubkey::new_from_array(item.id)
                                        );
                                        confirmed_success = true;
                                        break;
                                    } else {
                                        attempts += 1;
                                        blockhash_cache.refresh_blockhash().await;
                                        if attempts > 20 {
                                            let delay_ms = 10 * (attempts - 20);
                                            sleep(Duration::from_millis(delay_ms)).await;
                                        }
                                    }
                                }
                                Err(err) => {
                                    warn!("Transaction {sig} failed to confirm: {err}");
                                    attempts += 3;
                                    blockhash_cache.refresh_blockhash().await;
                                }
                            }
                        }
                        Err(_) => {
                            // Response may be in the same slot, we retry with linear backoff
                            blockhash_cache.refresh_blockhash().await;
                            if attempts > 5 {
                                let delay_ms = 20 * (attempts - 5);
                                sleep(Duration::from_millis(delay_ms)).await;
                            }
                            attempts += 1;
                        }
                    }
                }

                // Task finished. Remove from active_tasks. If not confirmed, also clear inflight to allow retry.
                {
                    let mut tasks_all = oracle_client_for_cleanup.active_tasks.write().await;
                    if let Some(tasks_for_queue) = tasks_all.get_mut(&queue_key_spawn) {
                        tasks_for_queue.remove(&item.id);
                    }
                }

                if !confirmed_success {
                    let mut inflight_all =
                        oracle_client_for_cleanup.inflight_requests.write().await;
                    if let Some(inflight_for_queue) = inflight_all.get_mut(&queue_key_spawn) {
                        inflight_for_queue.remove(&item.id);
                    }
                }
            });

            // Track the task handle for potential cancellation if the item disappears from the queue
            {
                let mut tasks_all = oracle_client.active_tasks.write().await;
                let tasks_for_queue = tasks_all.entry(queue_key.clone()).or_default();
                tasks_for_queue.insert(item.id, handle);
            }
        }
    }
}

#[repr(transparent)]
pub struct ProcessableItem(pub QueueItem);

impl ProcessableItem {
    #[allow(clippy::too_many_arguments)]
    pub async fn process_item(
        &self,
        oracle_client: &OracleClient,
        rpc_client: &Arc<RpcClient>,
        blockhash_cache: &BlockhashCache,
        vrf_input: &[u8; 32],
        queue_pubkey: &Pubkey,
        queue_meta: &Queue,
        account_bytes: &[u8],
    ) -> Result<String> {
        let (output, (commitment_base, commitment_hash, s)) =
            compute_vrf(oracle_client.oracle_vrf_sk, vrf_input);

        assert!(verify_vrf(
            oracle_client.oracle_vrf_pk,
            vrf_input,
            output,
            (commitment_base, commitment_hash, s),
        ));

        let (blockhash, current_slot) = blockhash_cache.get_blockhash_and_slot().await;

        // Check whether the request is expired
        let age = current_slot.saturating_sub(self.0.slot);
        let is_purge = age > QUEUE_TTL_SLOTS;
        let ix = if is_purge {
            // Build purge instruction for the queue index
            purge_expired_requests(oracle_client.keypair.pubkey(), queue_meta.index)
        } else {
            // Build provide_randomness instruction
            let mut ix = provide_randomness_with_identity_mode(
                oracle_client.keypair.pubkey(),
                *queue_pubkey,
                Pubkey::new_from_array(self.0.callback_program_id),
                self.0.identity_mode,
                *vrf_input,
                PodRistrettoPoint(output.to_bytes()),
                PodRistrettoPoint(commitment_base.to_bytes()),
                PodRistrettoPoint(commitment_hash.to_bytes()),
                PodScalar(s.to_bytes()),
            );
            let metas = self.0.account_metas(&account_bytes[8..]);
            ix.accounts
                .extend(metas.iter().map(|a| a.to_account_meta()));
            ix
        };

        let budget = if is_purge {
            1_000_000
        } else {
            match self.0.priority_request {
                1 => 400_000,
                _ => 300_000,
            }
        };
        let tx = Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(budget), ix],
            Some(&oracle_client.keypair.pubkey()),
            &[&oracle_client.keypair],
            blockhash,
        );

        use solana_client::rpc_config::RpcSendTransactionConfig;
        let sig = rpc_client
            .send_transaction_with_config(
                &tx,
                RpcSendTransactionConfig {
                    skip_preflight: oracle_client.skip_preflight,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .await?;
        Ok(sig.to_string())
    }
}
