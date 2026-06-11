use ephemeral_rollups_sdk::cpi::{
    delegate_account_with_any_validator, DelegateAccounts, DelegateConfig,
};
use ephemeral_vrf_api::prelude::*;

/// Process the delegation of an Oracle queue to the delegation program
///
/// This instruction allows an authority to vrf-macro an Oracle queue to the delegation program,
/// enabling other programs to interact with the queue through the delegation mechanism.
///
/// Accounts:
///
/// 0. `[signer]` The authority that controls the Oracle queue
/// 1. `[writable]` The Oracle queue account to be delegated
/// 2. `[writable]` The delegation buffer account
/// 3. `[writable]` The delegation record account
/// 4. `[writable]` The delegation metadata account
/// 5. `[]` The delegation program
/// 6. `[]` The owner program (must be the ephemeral VRF program)
/// 7. `[]` The system program
///
/// Requirements:
///
/// - The authority (account 0) must be a signer.
/// - The Oracle queue (account 1) must be a valid PDA with seeds [QUEUE, authority.key, index].
/// - The owner program (account 2) must be the ephemeral VRF program.
/// - The delegation accounts (3-5) must have the correct PDAs for the delegation program.
///
/// 1. Parse the instruction data and extract arguments (DelegateOracleQueue).
/// 2. Verify the owner program is the ephemeral VRF program.
/// 3. Set up the delegation accounts structure.
/// 4. Create the PDA seeds for the Oracle queue.
/// 5. Configure the delegation parameters.
/// 6. Call the delegation program to vrf-macro the Oracle queue account.
pub fn process_delegate_oracle_queue(accounts: &[AccountInfo<'_>], data: &[u8]) -> ProgramResult {
    let args = DelegateOracleQueue::try_from_bytes(data)?;

    // Load accounts.
    let [authority_info, oracle_queue_info, buffer, delegation_record, delegation_metadata, delegation_program, owner_program, system_program] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Checks
    authority_info.is_signer()?;
    owner_program.has_address(&ephemeral_vrf_api::ID)?;
    let pda_seeds: &[&[u8]] = &[QUEUE, &authority_info.key.to_bytes(), &[args.index]];
    oracle_queue_info
        .is_writable()?
        .has_owner(&ephemeral_vrf_api::ID)?
        .has_seeds(pda_seeds, &ephemeral_vrf_api::ID)?;
    Queue::try_from_bytes(&oracle_queue_info.try_borrow_data()?)?;

    // Delegate
    let delegate_accounts = DelegateAccounts {
        payer: authority_info,
        pda: oracle_queue_info,
        owner_program,
        buffer,
        delegation_record,
        delegation_metadata,
        delegation_program,
        system_program,
    };
    let delegate_config = DelegateConfig {
        commit_frequency_ms: 0,
        validator: Some(system_program::id()),
    };
    delegate_account_with_any_validator(delegate_accounts, pda_seeds, delegate_config)?;

    Ok(())
}
