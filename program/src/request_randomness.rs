use ephemeral_vrf_api::prelude::*;
use solana_program::hash::hashv;
use solana_program::msg;
use solana_program::program::invoke;
use solana_program::sysvar::slot_hashes;
use solana_system_interface::instruction as system_instruction;

/// Process a request for randomness
///
/// Accounts:
///
/// 0. `[signer]` signer - The account requesting randomness and paying for the transaction
/// 1. `[signer]` program_identity_info - The identity PDA of the calling program
/// 2. `[]` oracle_queue_info - The oracle queue account that will store the randomness request
/// 3. `[]` system_program_info - The system program
/// 4. `[]` slothashes_account_info - The SlotHashes sysvar account
///
/// Requirements:
///
/// - The signer must be a valid signer
/// - The program identity must be a valid signer and derived from the vrf-macro program ID
/// - The oracle queue must be properly initialized
/// - The request is stored in the oracle queue with a combined hash derived from:
///   - caller_seed
///   - current slot
///   - slot hash
///   - vrf-macro discriminator
///   - vrf-macro program ID
///
/// 1. Verify the signer
/// 2. Verify the program identity
/// 3. Get the current slot and slot hash
/// 4. Create a combined hash from inputs to uniquely identify this request
/// 5. Insert the request into the oracle queue
/// 6. Resize the oracle queue PDA if needed
/// 7. Update the oracle queue data
pub fn process_request_randomness(
    accounts: &[AccountInfo<'_>],
    data: &[u8],
    high_priority: bool,
    scoped: bool,
) -> ProgramResult {
    let args = RequestRandomness::try_from_bytes(data)?;

    // For scoped requests, precompute the scoped identity bump once here so that
    // `provide_randomness` can use the cheap `create_program_address` instead of
    // `find_program_address` on the (oracle-paid) fulfillment path.
    let identity_bump = if scoped {
        scoped_identity_pda(&args.callback_program_id).1
    } else {
        0
    };

    // Load accounts
    let [signer_info, program_identity_info, oracle_queue_info, system_program_info, slothashes_account_info] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Verify signer
    signer_info.is_signer()?;

    // Verify caller program
    program_identity_info
        .has_seeds(&[IDENTITY], &args.callback_program_id)?
        .is_signer()?;

    oracle_queue_info
        .is_writable()?
        .has_owner(&ephemeral_vrf_api::ID)?;

    // Load slot and slothash
    slothashes_account_info.is_sysvar(&slot_hashes::id())?;
    let slothash: [u8; 32] = slothashes_account_info.try_borrow_data()?[16..48]
        .try_into()
        .map_err(|_| ProgramError::UnsupportedSysvar)?;
    let slot = Clock::get()?.slot;
    let time = Clock::get()?.unix_timestamp;

    {
        // Borrow queue account data and load QueueAccount view
        let mut data = oracle_queue_info.try_borrow_mut_data()?;
        Queue::try_from_bytes(&data)?;
        // Skip 8-byte discriminator
        let queue_data = &mut data[8..];
        let mut queue_acc = QueueAccount::load(queue_data)?;

        // Optionally validate discriminator length to 8 bytes max (borsh Vec allows larger, but callbacks typically use 8)
        if args.callback_discriminator.len() > 8 {
            return Err(ProgramError::from(EphemeralVrfError::ArgumentSizeTooLarge));
        }

        let metas = args
            .callback_accounts_metas
            .iter()
            .map(|ca| (*ca).into())
            .collect::<Vec<CompactAccountMeta>>();

        // Compute a combined hash that includes the actual queue insertion position.
        let idx = queue_acc.insertion_position(
            &args.callback_discriminator,
            &metas,
            &args.callback_args,
        )?;
        let combined_hash = hashv(&[
            &args.caller_seed,
            &slot.to_le_bytes(),
            &slothash,
            &args.callback_discriminator,
            &args.callback_program_id.to_bytes(),
            &time.to_le_bytes(),
            &idx.to_le_bytes(),
        ]);

        // Log to simplify gathering all the information needed to recreate the combined_hash.
        msg!("Idx: {}", idx);

        // Build the base item; variable-length parts are appended by add_item()
        let base_item = QueueItem {
            slot,
            id: combined_hash.to_bytes(),
            callback_program_id: args.callback_program_id.to_bytes(),
            callback_discriminator_offset: 0,
            metas_offset: 0,
            args_offset: 0,
            callback_discriminator_len: 0,
            metas_len: 0,
            args_len: 0,
            priority_request: high_priority as u8,
            used: 0,
            identity_mode: scoped as u8,
            identity_bump,
            _padding: [0u8; 2],
        };

        // Append the item to the queue (writes discriminator, metas, args into the variable region)
        let _logical_index = queue_acc.add_item(
            &base_item,
            &args.callback_discriminator,
            &metas,
            &args.callback_args,
        )?;
    }

    // Transfer request cost to the queue PDA (unless this is a fee-exempt ephemeral queue)
    if !crate::fees::is_fee_exempt_ephemeral_queue(oracle_queue_info.key) {
        let cost = if high_priority {
            VRF_HIGH_PRIORITY_LAMPORTS_COST
        } else {
            VRF_LAMPORTS_COST
        };
        invoke(
            &system_instruction::transfer(signer_info.key, oracle_queue_info.key, cost),
            &[
                signer_info.clone(),
                oracle_queue_info.clone(),
                system_program_info.clone(),
            ],
        )?;
    }

    Ok(())
}
