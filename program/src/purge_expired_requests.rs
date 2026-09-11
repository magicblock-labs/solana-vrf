use solana_program::msg;
use solana_vrf_api::prelude::*;

/// Remove all requests in the queue whose wall-clock age exceeds
/// `QUEUE_TTL_SECONDS`.
///
/// Accounts:
/// 0. `[]` oracle_info               – The oracle public key used in the queue PDA seeds
/// 1. `[writable]` oracle_queue_info – The oracle queue account (PDA)
///
/// Requirements:
/// - No signer needed (permissionless), anyone can call.
/// - oracle_queue_info must match seeds [QUEUE, oracle_info.key, [index]].
pub fn process_purge_expired_requests(accounts: &[AccountInfo<'_>], data: &[u8]) -> ProgramResult {
    let args = PurgeExpiredRequests::try_from_bytes(data)?;

    // Accounts
    let [oracle_info, oracle_queue_info] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Validate queue PDA seeds and ownership / writability
    oracle_queue_info
        .is_writable()?
        .has_owner(&solana_vrf_api::ID)?
        .has_seeds(
            &[QUEUE, oracle_info.key.to_bytes().as_ref(), &[args.index]],
            &solana_vrf_api::ID,
        )?;

    // Age is measured against the request's own creation timestamp, so expiry
    // does not depend on the slot duration.
    let now_stamp = QueueItem::created_at_from(Clock::get()?.unix_timestamp);

    // Borrow queue data and scan/remove expired items using QueueAccount view
    let mut acc_data = oracle_queue_info.try_borrow_mut_data()?;
    Queue::try_from_bytes(&acc_data)?;
    let mut queue_acc = QueueAccount::load(&mut acc_data)?;

    // Single O(n) pass so a full queue stays within the compute budget.
    let mut total_cost: u64 = 0;
    let mut removed: usize = 0;
    msg!("Items in the queue: {}", queue_acc.len());
    queue_acc.remove_items_matching(
        |item| item.age_secs(now_stamp) > QUEUE_TTL_SECONDS,
        |item| {
            let cost = if item.priority_request == 1 {
                VRF_HIGH_PRIORITY_LAMPORTS_COST
            } else {
                VRF_LAMPORTS_COST
            };
            total_cost = total_cost.saturating_add(cost);
            removed += 1;
        },
    );
    msg!("Removed {} expired items from the queue", removed);

    // Send the fees to the oracle.
    // The oracle also accrues fees on malformed/expired requests to
    // 1) incentivize queue cleaning and
    // 2) disincentivize creation of malformed requests
    if total_cost > 0 && !crate::fees::is_fee_exempt_ephemeral_queue(oracle_queue_info.key) {
        crate::fees::transfer_fee(oracle_queue_info, oracle_info, total_cost)?;
    }

    Ok(())
}
