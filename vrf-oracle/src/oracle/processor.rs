use crate::blockhash_cache::BlockhashCache;
use crate::oracle::client::OracleClient;
use anyhow::Result;
use ephemeral_vrf::vrf::{compute_vrf, verify_vrf};
use ephemeral_vrf_api::{
    prelude::{
        provide_randomness_with_identity_mode, purge_expired_requests, EphemeralVrfError, Queue,
        QueueAccount, QueueItem, QUEUE_TTL_SLOTS,
    },
    state::oracle_queue_pda,
    ID as PROGRAM_ID,
};
use futures_util::future::join_all;
use futures_util::FutureExt;
use log::{error, info, trace, warn};
use serde_json::json;
use solana_account_decoder::UiAccountEncoding;
use solana_client::client_error::ClientError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::RpcFilterType;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_response::{OptionalContext, RpcKeyedAccount};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_curve25519::{ristretto::PodRistrettoPoint, scalar::PodScalar};
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    pubkey::Pubkey,
    signature::Signer,
    transaction::{Transaction, TransactionError},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::task;

const ANCHOR_CONSTRAINT_ADDRESS_ERROR: u32 = 2012;
const BLOCKHASH_MAX_AGE: Duration = Duration::from_secs(3);
const MAX_PRIORITY_FEE_LAMPORTS: u64 = 60_000;

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
        with_context: Some(true),
        ..Default::default()
    };

    // The response's context slot orders this view against notifications.
    // Never demand a minimum slot: the tracker can run ahead of the scan
    // node, failing the request (-32016) instead of returning a usable view.
    let response = rpc_client
        .send::<OptionalContext<Vec<RpcKeyedAccount>>>(
            RpcRequest::GetProgramAccounts,
            json!([PROGRAM_ID.to_string(), config]),
        )
        .await?;
    let (view_slot, keyed_accounts) = match response {
        OptionalContext::Context(response) => (response.context.slot, response.value),
        // A contextless response cannot be ordered against live
        // notifications; treat it as a failed scan and retry later.
        OptionalContext::NoContext(_) => {
            anyhow::bail!("getProgramAccounts returned no context slot")
        }
    };
    let accounts: Vec<(Pubkey, Account)> = keyed_accounts
        .into_iter()
        .filter_map(|entry| Some((entry.pubkey.parse().ok()?, entry.account.decode()?)))
        .collect();

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
                    Some(view_slot),
                    true,
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

#[allow(clippy::too_many_arguments)]
pub async fn process_oracle_queue(
    oracle_client: &Arc<OracleClient>,
    rpc_client: &Arc<RpcClient>,
    blockhash_cache: &BlockhashCache,
    queue: &Pubkey,
    oracle_queue: &Queue,
    account_bytes: Arc<Vec<u8>>,
    notification_slot: Option<u64>,
    is_snapshot: bool,
) {
    if oracle_queue_pda(&oracle_client.keypair.pubkey(), oracle_queue.index).0 == *queue
        && oracle_client.should_process_queue(rpc_client, queue).await
    {
        // Apply views in per-queue slot order, holding the guard across the
        // task-map mutations below so a stale view cannot resurrect or cancel
        // requests. Snapshots lack intra-slot order: require strictly newer.
        let _view_order_guard = {
            let mut latest_views = oracle_client.latest_view_slots.write().await;
            if let Some(view_slot) = notification_slot {
                let latest = latest_views.entry(queue.to_string()).or_insert(0);
                let stale = if is_snapshot {
                    view_slot <= *latest
                } else {
                    view_slot < *latest
                };
                if stale {
                    return;
                }
                *latest = view_slot;
            }
            latest_views
        };

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
                    // Removal happens strictly after enqueue, so an absence
                    // from a view no newer than the enqueue slot predates the
                    // request: skip it so a stale snapshot cannot cancel a
                    // live fulfillment task.
                    if let (Some(enqueue_slot), Some(view_slot)) =
                        (inflight_for_queue.get(&tracked_id), notification_slot)
                    {
                        if view_slot <= *enqueue_slot {
                            continue;
                        }
                    }
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

            // Keep the inflight reservation locked through task publication so a
            // queue-removal update cannot leave an untracked task between the maps.
            let mut inflight_all = oracle_client.inflight_requests.write().await;
            let inflight_for_queue = inflight_all.entry(queue_key_spawn.clone()).or_default();
            if inflight_for_queue.contains_key(&item.id) {
                continue;
            }
            inflight_for_queue.insert(item.id, item.slot);

            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                if start_rx.await.is_err() {
                    return;
                }
                let mut attempts = 0;
                let mut backoff_attempts = 0;
                let mut first_legal_attempt_pending = false;
                blockhash_cache.refresh_if_stale(BLOCKHASH_MAX_AGE).await;
                let prepared_vrf =
                    ProcessableItem::prepare_vrf(&oracle_client_for_proc, &input_seed);
                let mut prepared_transaction = Some(
                    ProcessableItem(item)
                        .prepare_transaction(
                            &oracle_client_for_proc,
                            &rpc_client,
                            &blockhash_cache,
                            &input_seed,
                            &prepared_vrf,
                            &queue,
                            &oracle_queue,
                            account_bytes_task.as_slice(),
                            attempts,
                        )
                        .await,
                );

                while attempts < 100 {
                    let first_valid_slot = item.slot.saturating_add(1);
                    if oracle_client_for_proc.slot_tracker.current() < first_valid_slot {
                        if attempts == 0 {
                            oracle_client_for_proc
                                .slot_tracker
                                .wait_for_slot_early(first_valid_slot)
                                .await;
                        } else {
                            oracle_client_for_proc
                                .slot_tracker
                                .wait_for_slot(first_valid_slot)
                                .await;
                        }
                    }
                    let attempt_slot = oracle_client_for_proc.slot_tracker.current();
                    let sent_early = attempt_slot < first_valid_slot;
                    let mut use_backoff = false;
                    let transaction = match prepared_transaction.take() {
                        Some(transaction)
                            if attempt_slot.saturating_sub(item.slot) <= QUEUE_TTL_SLOTS =>
                        {
                            transaction
                        }
                        _ => {
                            ProcessableItem(item)
                                .prepare_transaction(
                                    &oracle_client_for_proc,
                                    &rpc_client,
                                    &blockhash_cache,
                                    &input_seed,
                                    &prepared_vrf,
                                    &queue,
                                    &oracle_queue,
                                    account_bytes_task.as_slice(),
                                    attempts,
                                )
                                .await
                        }
                    };

                    // Retry transient send errors on the normal backoff; deterministic
                    // callback address errors wait for expiry and purge.
                    let skip_preflight = attempts == 0 || first_legal_attempt_pending;
                    first_legal_attempt_pending = sent_early;
                    let result = ProcessableItem::send_transaction(
                        &oracle_client_for_proc,
                        &rpc_client,
                        &transaction,
                        skip_preflight,
                    )
                    .await;
                    let early_send_accepted = sent_early && result.is_ok();
                    match result {
                        Ok(signature) => trace!(
                            "Transaction: {}, for id {}",
                            signature,
                            Pubkey::new_from_array(item.id)
                        ),
                        Err(error) => {
                            use_backoff = true;
                            if let Some(TransactionError::InstructionError(
                                1,
                                InstructionError::Custom(code),
                            )) = error
                                .downcast_ref::<ClientError>()
                                .and_then(ClientError::get_transaction_error)
                            {
                                if code == EphemeralVrfError::RandomnessRequestNotFound as u32 {
                                    break;
                                }
                                if code
                                    == EphemeralVrfError::OracleMustProvideInDifferentSlot as u32
                                {
                                    use_backoff = false;
                                }
                                if code == ANCHOR_CONSTRAINT_ADDRESS_ERROR {
                                    let purge_slot =
                                        item.slot.saturating_add(QUEUE_TTL_SLOTS).saturating_add(1);
                                    oracle_client_for_proc
                                        .slot_tracker
                                        .wait_for_slot(purge_slot)
                                        .await;
                                    blockhash_cache.refresh_if_stale(BLOCKHASH_MAX_AGE).await;
                                    continue;
                                }
                            }
                        }
                    }

                    // Fulfillment is observed via the queue subscription: this task
                    // is aborted once the item disappears. Accepted transactions retry
                    // next slot; transient errors back off by 1, 2, 4, ... slots.
                    let next_attempt = attempts + 1;
                    if first_legal_attempt_pending {
                        prepared_transaction = Some(
                            ProcessableItem(item)
                                .prepare_transaction(
                                    &oracle_client_for_proc,
                                    &rpc_client,
                                    &blockhash_cache,
                                    &input_seed,
                                    &prepared_vrf,
                                    &queue,
                                    &oracle_queue,
                                    account_bytes_task.as_slice(),
                                    next_attempt,
                                )
                                .await,
                        );
                    }
                    let retry_slots = if use_backoff {
                        (1u64 << backoff_attempts.min(5)).min(32)
                    } else {
                        backoff_attempts = 0;
                        1
                    };
                    blockhash_cache.refresh_if_stale(BLOCKHASH_MAX_AGE).await;
                    oracle_client_for_proc
                        .slot_tracker
                        .wait_for_slot(attempt_slot.saturating_add(retry_slots))
                        .await;
                    if early_send_accepted {
                        tokio::time::sleep(oracle_client_for_proc.slot_tracker.early_send_grace())
                            .await;
                    }
                    attempts = next_attempt;
                    if use_backoff {
                        backoff_attempts += 1;
                    }
                }

                // Task stopped or attempts exhausted. Remove from active_tasks and
                // clear inflight so a later queue snapshot can retry it if needed.
                // (Successful items never reach here: their tasks are aborted by the
                // queue-diff path, which also cleans both maps.)
                {
                    let mut tasks_all = oracle_client_for_cleanup.active_tasks.write().await;
                    if let Some(tasks_for_queue) = tasks_all.get_mut(&queue_key_spawn) {
                        tasks_for_queue.remove(&item.id);
                    }
                }

                {
                    let mut inflight_all =
                        oracle_client_for_cleanup.inflight_requests.write().await;
                    if let Some(inflight_for_queue) = inflight_all.get_mut(&queue_key_spawn) {
                        inflight_for_queue.remove(&item.id);
                    }
                }
            });

            {
                let mut tasks_all = oracle_client.active_tasks.write().await;
                let tasks_for_queue = tasks_all.entry(queue_key.clone()).or_default();
                tasks_for_queue.insert(item.id, handle);
            }
            drop(inflight_all);
            let _ = start_tx.send(());
        }
    }
}

#[repr(transparent)]
pub struct ProcessableItem(pub QueueItem);

struct PreparedVrf {
    output: PodRistrettoPoint,
    commitment_base: PodRistrettoPoint,
    commitment_hash: PodRistrettoPoint,
    s: PodScalar,
}

impl ProcessableItem {
    fn prepare_vrf(oracle_client: &OracleClient, vrf_input: &[u8; 32]) -> PreparedVrf {
        let (output, (commitment_base, commitment_hash, s)) =
            compute_vrf(oracle_client.oracle_vrf_sk, vrf_input);

        assert!(verify_vrf(
            oracle_client.oracle_vrf_pk,
            vrf_input,
            output,
            (commitment_base, commitment_hash, s),
        ));

        PreparedVrf {
            output: PodRistrettoPoint(output.to_bytes()),
            commitment_base: PodRistrettoPoint(commitment_base.to_bytes()),
            commitment_hash: PodRistrettoPoint(commitment_hash.to_bytes()),
            s: PodScalar(s.to_bytes()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_transaction(
        &self,
        oracle_client: &OracleClient,
        rpc_client: &RpcClient,
        blockhash_cache: &BlockhashCache,
        vrf_input: &[u8; 32],
        prepared_vrf: &PreparedVrf,
        queue_pubkey: &Pubkey,
        queue_meta: &Queue,
        account_bytes: &[u8],
        attempt: u64,
    ) -> Transaction {
        let (blockhash, _) = blockhash_cache.get_blockhash_and_slot().await;
        let current_slot = oracle_client.slot_tracker.current();

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
                prepared_vrf.output,
                prepared_vrf.commitment_base,
                prepared_vrf.commitment_hash,
                prepared_vrf.s,
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
        // Nonce: vary the compute limit so each retry is a distinct
        // transaction under the same cached blockhash.
        let budget = budget + (attempt % 256) as u32;
        // Escalate the fee roughly every 3s the request stays unlanded; the
        // cap bounds the total priority spend per attempt for any CU limit.
        let max_price = MAX_PRIORITY_FEE_LAMPORTS.saturating_mul(1_000_000) / u64::from(budget);
        let priority_fee =
            (oracle_client.priority_fee(rpc_client).await << (attempt / 8).min(4)).min(max_price);
        let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_limit(budget), ix];
        if priority_fee > 0 {
            // Appended after the VRF instruction so its error index stays 1,
            // which the send loop's error matching relies on.
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                priority_fee,
            ));
        }
        Transaction::new_signed_with_payer(
            &instructions,
            Some(&oracle_client.keypair.pubkey()),
            &[&oracle_client.keypair],
            blockhash,
        )
    }

    async fn send_transaction(
        oracle_client: &OracleClient,
        rpc_client: &Arc<RpcClient>,
        transaction: &Transaction,
        first_legal_attempt: bool,
    ) -> Result<String> {
        use solana_client::rpc_config::RpcSendTransactionConfig;
        let sig = rpc_client
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: oracle_client.skip_preflight && first_legal_attempt,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .await?;
        Ok(sig.to_string())
    }
}
