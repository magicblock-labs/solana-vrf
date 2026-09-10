use solana_vrf_api::prelude::*;

/// Pause or unpause an Oracle queue.
///
/// Pausing lets the owning oracle stop accepting new requests so the queue can
/// be drained and then closed; without it, anyone could keep adding requests
/// and prevent the queue from ever being closed.
///
/// Accounts:
///
/// 0. `[signer]` The Oracle account that owns the queue
/// 1. `[writable]` The Oracle queue account
///
/// Requirements:
///
/// - The Oracle (account 0) must be a signer.
/// - The queue (account 1) must be a valid PDA with seeds [QUEUE, oracle.key, index],
///   owned by the SolanaVrf program.
/// - `paused` must be 0 or 1; any other value is rejected.
pub fn process_set_queue_paused(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let args = SetQueuePaused::try_from_bytes(data)?;
    if args.paused > 1 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let [oracle_info, oracle_queue_info] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    oracle_info.is_signer()?;

    oracle_queue_info
        .is_writable()?
        .has_owner(&solana_vrf_api::ID)?
        .has_seeds(
            &[QUEUE, oracle_info.key.to_bytes().as_ref(), &[args.index]],
            &solana_vrf_api::ID,
        )?;

    let mut data = oracle_queue_info.try_borrow_mut_data()?;
    Queue::try_from_bytes(&data)?;
    let queue_acc = QueueAccount::load(&mut data)?;
    queue_acc.header.paused = args.paused;

    Ok(())
}
