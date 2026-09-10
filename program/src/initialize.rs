use solana_vrf_api::prelude::*;

/// Process the initialization of the SolanaVrf program
///
/// Accounts:
///
/// 0; `[signer]` The authority that initializes the program
/// 1; `[]`       The oracles account (PDA to be created)
/// 2; `[]`       The system program
///
/// Requirements:
///
/// - The authority (account 0) must be a signer.
/// - The oracles account (account 1) must be empty and use the correct seeds ([ORACLES]).
///
/// 1. Parse the instruction data and extract arguments (Initialize).
/// 2. Create the oracles PDA.
/// 3. Write the default Oracles data to the new PDA.
pub fn process_initialize(accounts: &[AccountInfo<'_>], _data: &[u8]) -> ProgramResult {
    // Load accounts.
    let [signer_info, oracles_info, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer_info.is_signer()?;
    oracles_info
        .is_empty()?
        .is_writable()?
        .has_seeds(&[ORACLES], &solana_vrf_api::ID)?;

    let oracles = Oracles::default();
    let oracles_bytes = oracles.to_bytes_with_discriminator()?;

    create_pda(
        oracles_info,
        oracles_bytes.len(),
        &[ORACLES],
        oracles_pda().1,
        system_program,
        signer_info,
    )?;

    let mut oracles_data = oracles_info.try_borrow_mut_data()?;
    oracles_data.copy_from_slice(&oracles_bytes);

    Ok(())
}
