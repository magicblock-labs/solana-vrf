use ephemeral_vrf_api::prelude::*;
use solana_program::msg;

/// Remove all requests in the queue whose age (current_slot - item.slot)
/// exceeds the TTL.
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

    let current_slot = Clock::get()?.slot;

    // Borrow queue data and scan/remove expired items using QueueAccount view
    let mut acc_data = oracle_queue_info.try_borrow_mut_data()?;
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
        let age = current_slot.saturating_sub(item.slot);
        if age > QUEUE_TTL_SLOTS {
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
