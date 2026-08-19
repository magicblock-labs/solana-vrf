use crate::steel::*;
use solana_program::program::invoke;
use solana_program::rent::Rent;
use solana_system_interface::instruction as system_instruction;

/// Creates a new pda
#[inline(always)]
pub fn create_pda<'a, 'info>(
    target_account: &'a AccountInfo<'info>,
    space: usize,
    pda_seeds: &[&[u8]],
    pda_bump: u8,
    system_program: &'a AccountInfo<'info>,
    payer: &'a AccountInfo<'info>,
) -> ProgramResult {
    let owner = &crate::ID;
    // Generate the PDA's signer seeds
    let pda_bump_slice = &[pda_bump];
    let pda_signer_seeds = [pda_seeds, &[pda_bump_slice]].concat();
    // Create the account manually or using the create instruction
    let rent = Rent::get()?;
    if target_account.lamports().eq(&0) {
        // If balance is zero, create account
        solana_program::program::invoke_signed(
            &system_instruction::create_account(
                payer.key,
                target_account.key,
                rent.minimum_balance(space),
                space as u64,
                owner,
            ),
            &[
                payer.clone(),
                target_account.clone(),
                system_program.clone(),
            ],
            &[&pda_signer_seeds],
        )?;
    } else {
        // Otherwise, if balance is nonzero:
        // 1) transfer sufficient lamports for rent exemption
        let rent_exempt_balance = rent
            .minimum_balance(space)
            .saturating_sub(target_account.lamports());
        if rent_exempt_balance.gt(&0) {
            solana_program::program::invoke(
                &system_instruction::transfer(payer.key, target_account.key, rent_exempt_balance),
                &[
                    payer.as_ref().clone(),
                    target_account.as_ref().clone(),
                    system_program.as_ref().clone(),
                ],
            )?;
        }
        // 2) allocate space for the account
        solana_program::program::invoke_signed(
            &system_instruction::allocate(target_account.key, space as u64),
            &[
                target_account.as_ref().clone(),
                system_program.as_ref().clone(),
            ],
            &[&pda_signer_seeds],
        )?;
        // 3) assign our program as the owner
        solana_program::program::invoke_signed(
            &system_instruction::assign(target_account.key, owner),
            &[
                target_account.as_ref().clone(),
                system_program.as_ref().clone(),
            ],
            &[&pda_signer_seeds],
        )?;
    }

    Ok(())
}

/// Resize PDA
pub fn resize_pda<'a, 'info>(
    payer: &'a AccountInfo<'info>,
    pda: &'a AccountInfo<'info>,
    system_program: &'a AccountInfo<'info>,
    new_size: usize,
) -> Result<(), ProgramError> {
    let new_minimum_balance = Rent::default().minimum_balance(new_size);
    let lamports_diff = new_minimum_balance.saturating_sub(pda.lamports());
    if lamports_diff > 0 {
        invoke(
            &system_instruction::transfer(payer.key, pda.key, lamports_diff),
            &[payer.clone(), pda.clone(), system_program.clone()],
        )?;
    } else {
        let abs_diff = pda.lamports().saturating_sub(new_minimum_balance);
        **pda.try_borrow_mut_lamports()? -= abs_diff;
        **payer.try_borrow_mut_lamports()? += abs_diff;
    }

    pda.resize(new_size)?;
    Ok(())
}
