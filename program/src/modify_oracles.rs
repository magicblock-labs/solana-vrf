use ephemeral_vrf_api::loaders::load_program_upgrade_authority;
use ephemeral_vrf_api::prelude::EphemeralVrfError::{
    InvalidOracleIdentity, QueueNotEmpty, Unauthorized,
};
use ephemeral_vrf_api::prelude::*;
use ephemeral_vrf_api::verify::is_on_curve;
use solana_curve25519::ristretto::validate_ristretto;
use solana_program::msg;

/// Process the modification of oracles (add or remove)
///
/// Accounts:
///
/// 0. `[signer]` signer - Must be the admin
/// 1. `[writable]` oracles_info - PDA that stores the list of oracle identities
/// 2. `[writable]` oracle_data_info - PDA that stores the oracle data
/// 2. `[]` program data account - Used to read the program's upgrade authority
/// 3. `[]` system_program - System program for account creation/closing
///
/// Requirements:
///
/// - Signer must be the admin (ADMIN_PUBKEY)
/// - For adding an oracle (operation = 0):
///   - Oracle data account is created
///   - Oracle identity is added to the oracles list
/// - For removing an oracle (operation = 1):
///   - Oracle data account is closed
///   - Oracle identity is removed from the oracles list
///
/// 1. Verify the signer is the admin
/// 2. Validate account PDAs
/// 3. Add or remove the oracle based on operation
/// 4. Resize the oracles PDA if needed
/// 5. Update the oracles list
pub fn process_modify_oracles(accounts: &[AccountInfo<'_>], data: &[u8]) -> ProgramResult {
    // Parse args.
    let args = ModifyOracle::try_from_bytes(data)?;

    // Validate identity
    if args.identity == Pubkey::default()
        || args.identity == system_program::ID
        || !is_on_curve(&args.identity)
    {
        return Err(InvalidOracleIdentity.into());
    }
    if args.operation == 0
        && (args.oracle_pubkey.0 == [0u8; 32] || !validate_ristretto(&args.oracle_pubkey))
    {
        return Err(InvalidOracleIdentity.into());
    }

    // Load accounts.
    let [signer_info, oracles_info, oracle_data_info, program_data_info, system_program] = accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer_info.is_signer()?;

    // Check that the signer is the admin.
    // The admin is the program upgrade authority, which should be a multi-sig.
    let admin_pubkey = load_program_upgrade_authority(&ephemeral_vrf_api::ID, program_data_info)?
        .ok_or(Unauthorized)?;

    if !signer_info.key.eq(&admin_pubkey) {
        log(format!(
            "Signer not authorized, expected: {}, got: {}",
            admin_pubkey, signer_info.key
        ));
        return Err(Unauthorized.into());
    }

    oracles_info
        .is_writable()?
        .has_seeds(&[ORACLES], &ephemeral_vrf_api::ID)?;

    oracle_data_info.is_writable()?.has_seeds(
        &[ORACLE_DATA, args.identity.to_bytes().as_ref()],
        &ephemeral_vrf_api::ID,
    )?;

    let oracles_data = oracles_info.try_borrow_data()?;
    let mut oracles = Oracles::try_from_bytes_with_discriminator(&oracles_data)?;
    drop(oracles_data);

    if args.operation == 0 {
        oracles.oracles.push(args.identity);
        create_program_account::<Oracle>(
            oracle_data_info,
            system_program,
            signer_info,
            &ephemeral_vrf_api::ID,
            &[ORACLE_DATA, args.identity.to_bytes().as_ref()],
        )?;
        let mut oracle_data = oracle_data_info.as_account_mut::<Oracle>(&ephemeral_vrf_api::ID)?;
        oracle_data.vrf_pubkey = args.oracle_pubkey;
        oracle_data.registration_slot = Clock::get()?.slot;
    } else if args.operation == 1 {
        // Ensure oracle has no open queues before removal
        let open_queue = {
            let oracle_data = oracle_data_info.as_account::<Oracle>(&ephemeral_vrf_api::ID)?;
            oracle_data.open_queue
        };
        if open_queue != 0 {
            msg!("Oracle has {} open queues", open_queue);
            return Err(QueueNotEmpty.into());
        }
        oracles.oracles.retain(|oracle| oracle.ne(&args.identity));
        close_account(oracle_data_info, signer_info)?;
    } else {
        return Err(ProgramError::InvalidArgument);
    }

    resize_pda(
        signer_info,
        oracles_info,
        system_program,
        oracles.size_with_discriminator(),
    )?;

    let oracles_bytes = oracles.to_bytes_with_discriminator()?;
    let mut oracles_data = oracles_info.try_borrow_mut_data()?;

    oracles_data.copy_from_slice(&oracles_bytes);

    Ok(())
}
