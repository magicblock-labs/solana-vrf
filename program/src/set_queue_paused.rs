use ephemeral_vrf_api::prelude::*;

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
///   owned by the ephemeral VRF program.
pub fn process_set_queue_paused(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let args = SetQueuePaused::try_from_bytes(data)?;

    let [oracle_info, oracle_queue_info] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    oracle_info.is_signer()?;

    oracle_queue_info
        .is_writable()?
        .has_owner(&ephemeral_vrf_api::ID)?
        .has_seeds(
            &[QUEUE, oracle_info.key.to_bytes().as_ref(), &[args.index]],
            &ephemeral_vrf_api::ID,
        )?;

    let mut data = oracle_queue_info.try_borrow_mut_data()?;
    Queue::try_from_bytes(&data)?;
    let queue_data = &mut data[8..];
    let queue_acc = QueueAccount::load(queue_data)?;
    queue_acc.header.paused = u8::from(args.paused != 0);

    Ok(())
}
