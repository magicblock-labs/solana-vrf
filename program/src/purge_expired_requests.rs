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
    let mut queue_acc = QueueAccount::load(&mut acc_data)?;

    // Age is measured in wall-clock seconds, derived from the epoch's average
    // slot duration. The epoch-relative rate (elapsed_secs / elapsed_slots) is
    // loop-invariant, so compute it once here rather than per item — that keeps
    // a single-pass purge of a full queue well within the compute budget.
    //
    // All values are bounded by a single epoch (elapsed_slots <= slots-per-epoch,
    // elapsed_secs <= the epoch's wall-clock length), so native u64 arithmetic
    // cannot overflow — much cheaper on SBF than emulated 128-bit math. The
    // saturating_mul below is purely defensive. Clamps keep values >= 0/1.
    let current_slot = clock.slot;
    let elapsed_slots = current_slot.saturating_sub(epoch_start_slot).max(1);
    let elapsed_secs = clock
        .unix_timestamp
        .saturating_sub(clock.epoch_start_timestamp)
        .max(0) as u64;

    // Scan and remove expired items in a single O(n) pass, so purging stays
    // within the compute budget even when the queue is completely full.
    let mut total_cost: u64 = 0;
    let mut removed: usize = 0;
    msg!("Items in the queue: {}", queue_acc.len());
    queue_acc.remove_items_matching(
        |item| {
            // Only age accrued within the current epoch is counted (slot_age is
            // capped at elapsed_slots), so the estimate can never exceed the real
            // time elapsed since the epoch start — a request is never expired
            // before its TTL, even for requests that span an epoch boundary.
            let slot_age = current_slot.saturating_sub(item.slot).min(elapsed_slots);
            let age_secs = slot_age.saturating_mul(elapsed_secs) / elapsed_slots;
            age_secs > QUEUE_TTL_SECONDS
        },
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
