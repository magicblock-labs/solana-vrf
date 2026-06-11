use ephemeral_vrf_api::prelude::*;

/// Process the closing of an Oracle queue account
///
/// This instruction allows an Oracle to close one of their queue accounts,
/// reclaiming the rent lamports back to their account.
///
/// Accounts:
///
/// 0. `[signer]` The Oracle account that owns the queue
/// 1. `[writable]` The Oracle data PDA account for this oracle
/// 2. `[writable]` The Oracle queue account to be closed
///
/// Requirements:
///
/// - The Oracle (account 0) must be a signer.
/// - The Oracle data account (account 1) must be a valid PDA with seeds [ORACLE_DATA, oracle.key].
/// - The Oracle queue (account 2) must be a valid PDA with seeds [QUEUE, oracle.key, index].
/// - The queue account must be owned by the ephemeral VRF program.
/// - The queue must be empty (no unprocessed requests).
///
/// Process:
///
/// 1. Parse the instruction data and extract arguments (CloseOracleQueue).
/// 2. Verify the Oracle account is a signer.
/// 3. Validate the Oracle data and queue account PDA seeds with the provided index.
/// 4. Ensure the queue is empty.
/// 5. Decrement Oracle.open_queue and close the queue account, transferring lamports to the Oracle.
pub fn process_close_oracle_queue(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    // Parse args.
    let args = CloseOracleQueue::try_from_bytes(data)?;

    // Load accounts.
    let [oracle_info, oracle_data_info, oracle_queue_info] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    oracle_info.is_signer()?;

    // Validate Oracle data PDA
    oracle_data_info
        .is_writable()?
        .has_owner(&ephemeral_vrf_api::ID)?
        .has_seeds(
            &[ORACLE_DATA, oracle_info.key.to_bytes().as_ref()],
            &ephemeral_vrf_api::ID,
        )?;

    // Validate queue PDA
    oracle_queue_info
        .is_writable()?
        .has_owner(&ephemeral_vrf_api::ID)?
        .has_seeds(
            &[QUEUE, oracle_info.key.to_bytes().as_ref(), &[args.index]],
            &ephemeral_vrf_api::ID,
        )?;

    // Ensure the queue has no pending items before closing.
    {
        // Borrow data and load QueueAccount view to check emptiness
        let mut data = oracle_queue_info.try_borrow_mut_data()?;
        Queue::try_from_bytes(&data)?;
        let queue_data = &mut data[8..];
        let queue_acc = QueueAccount::load(queue_data)?;
        if !queue_acc.is_empty() {
            return Err(EphemeralVrfError::QueueNotEmpty.into());
        }
    }

    // Decrement oracle's open queue count
    let mut oracle_data_mut = oracle_data_info.as_account_mut::<Oracle>(&ephemeral_vrf_api::ID)?;
    oracle_data_mut.open_queue = oracle_data_mut.open_queue.saturating_sub(1);

    close_account(oracle_queue_info, oracle_info)?;

    Ok(())
}
