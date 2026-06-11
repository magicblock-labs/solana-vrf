use ephemeral_rollups_sdk::cpi::undelegate_account;
use ephemeral_vrf_api::prelude::*;

/// Process the undelegation of an Oracle queue from the delegation program
///
/// This instruction is a vrf-macro from the delegation program to complete the undelegation process
/// for an Oracle queue that was previously delegated. It's called by the delegation program as part
/// of the undelegation flow.
///
/// Accounts:
///
/// 0. `[signer]` The payer for the transaction
/// 1. `[writable]` The Oracle queue account being undelegated
/// 2. `[]` The delegation buffer account
/// 3. `[]` The system program
///
/// Requirements:
///
/// - The payer (account 0) must be a signer.
/// - The Oracle queue (account 1) must have been previously delegated to the delegation program.
/// - The delegation buffer (account 2) must be the correct PDA for this delegation.
///
/// Process:
/// 1. Load and validate the required accounts.
/// 2. Call the delegation program's undelegate_account function to complete the undelegation process.
pub fn process_undelegation(accounts: &[AccountInfo<'_>], data: &[u8]) -> ProgramResult {
    let pda_seeds: Vec<Vec<u8>> = PdaSeeds::parse(data)?;

    // Load accounts.
    let [oracle_queue_info, delegation_buffer, payer, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if pda_seeds.len() != 3
        || pda_seeds[0].as_slice() != QUEUE
        || pda_seeds[1].len() != 32
        || pda_seeds[2].len() != 1
    {
        return Err(ProgramError::InvalidSeeds);
    }
    let authority = pda_seeds[1].as_slice();
    let index = [pda_seeds[2][0]];
    oracle_queue_info
        .is_writable()?
        .has_seeds(&[QUEUE, authority, &index], &ephemeral_vrf_api::ID)?;

    // Undelegate
    undelegate_account(
        oracle_queue_info,
        &ephemeral_vrf_api::ID,
        delegation_buffer,
        payer,
        system_program,
        pda_seeds,
    )?;

    let queue_data = oracle_queue_info.try_borrow_data()?;
    Queue::try_from_bytes(&queue_data)?;

    Ok(())
}
