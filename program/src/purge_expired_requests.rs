use ephemeral_vrf_api::prelude::*;
use solana_program::epoch_schedule::EpochSchedule;
use solana_program::msg;

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
        .has_owner(&ephemeral_vrf_api::ID)?
        .has_seeds(
            &[QUEUE, oracle_info.key.to_bytes().as_ref(), &[args.index]],
            &ephemeral_vrf_api::ID,
        )?;

    // Measure request age in wall-clock seconds rather than a fixed slot count,
    // using the current epoch's average slot duration, so expiry stays correct
    // if the slot duration changes.
    let clock = Clock::get()?;
    let epoch_start_slot = EpochSchedule::get()?.get_first_slot_in_epoch(clock.epoch);

    // Borrow queue data and scan/remove expired items using QueueAccount view
    let mut acc_data = oracle_queue_info.try_borrow_mut_data()?;
    Queue::try_from_bytes(&acc_data)?;
    let queue_data = &mut acc_data[8..];
    let mut queue_acc = QueueAccount::load(queue_data)?;

    // Scan and remove expired items by logical index
    let mut total_cost: u64 = 0;
    let mut i: usize = 0;
    msg!("Items in the queue: {}", queue_acc.len());
    while i < queue_acc.len() {
        // Safe to unwrap: index < len()
        let item = queue_acc
            .get_item_by_index(i)
            .ok_or(ProgramError::InvalidAccountData)?;
        // Age in wall-clock seconds, derived from the epoch's average slot
        // duration. Computed in i128 to avoid overflow; clamps keep it >= 0.
        // Only age accrued within the current epoch is counted (slot_age is
        // capped at elapsed_slots), so the estimate can never exceed the real
        // time elapsed since the epoch start — a request is never expired
        // before its TTL, even for requests that span an epoch boundary.
        let elapsed_slots = clock.slot.saturating_sub(epoch_start_slot).max(1) as i128;
        let elapsed_secs = clock
            .unix_timestamp
            .saturating_sub(clock.epoch_start_timestamp)
            .max(0) as i128;
        let slot_age = (clock.slot.saturating_sub(item.slot) as i128).min(elapsed_slots);
        let age_secs = ((slot_age * elapsed_secs) / elapsed_slots) as i64;
        if age_secs > QUEUE_TTL_SECONDS {
            let cost = if item.priority_request == 1 {
                VRF_HIGH_PRIORITY_LAMPORTS_COST
            } else {
                VRF_LAMPORTS_COST
            };
            total_cost = total_cost.saturating_add(cost);
            let _ = queue_acc.remove_item(i)?;
            msg!(
                "Removing item {} from queue, new size {}",
                i,
                queue_acc.len()
            );
            // do not increment i; next item shifts into this index
        } else {
            i += 1;
        }
    }

    // Send the fees to the oracle.
    // The oracle also accrue fees on malformed/expired requests to
    // 1) incentivize queue cleaning and
    // 2) disincentivize creation of malformed requests
    if total_cost > 0 && !crate::fees::is_fee_exempt_ephemeral_queue(oracle_queue_info.key) {
        crate::fees::transfer_fee(oracle_queue_info, oracle_info, total_cost)?;
    }

    Ok(())
}
