use ephemeral_vrf_api::consts::DEFAULT_EPHEMERAL_QUEUE;
#[cfg(feature = "ephemeral-test-queue")]
use ephemeral_vrf_api::consts::DEFAULT_EPHEMERAL_TEST_QUEUE;
use solana_program::account_info::AccountInfo;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;

/// Whether `queue` is exempt from the per-request fee (and the matching oracle payout).
/// `DEFAULT_EPHEMERAL_QUEUE` is always exempt; the local test queue only with the
/// `ephemeral-test-queue` feature, so production builds never exempt it.
pub fn is_fee_exempt_ephemeral_queue(queue: &Pubkey) -> bool {
    if queue == &DEFAULT_EPHEMERAL_QUEUE {
        return true;
    }
    #[cfg(feature = "ephemeral-test-queue")]
    if queue == &DEFAULT_EPHEMERAL_TEST_QUEUE {
        return true;
    }
    false
}

// Transfer a specific amount of lamports from the oracle queue account to the oracle account.
// Assumes caller already validated seeds/ownership/writability and any signer requirements.
pub fn transfer_fee(
    oracle_queue_info: &AccountInfo<'_>,
    oracle_info: &AccountInfo<'_>,
    amount: u64,
) -> Result<(), ProgramError> {
    let (mut queue_lamports, mut oracle_lamports) = (
        oracle_queue_info.try_borrow_mut_lamports()?,
        oracle_info.try_borrow_mut_lamports()?,
    );

    **queue_lamports = (**queue_lamports)
        .checked_sub(amount)
        .ok_or(ProgramError::InsufficientFunds)?;
    **oracle_lamports = (**oracle_lamports)
        .checked_add(amount)
        .ok_or(ProgramError::InvalidArgument)?;

    Ok(())
}
