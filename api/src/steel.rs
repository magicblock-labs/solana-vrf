pub use bytemuck::{Pod, Zeroable};
pub use num_enum::{IntoPrimitive, TryFromPrimitive};
pub use solana_program::sysvar::clock::Clock;
pub use solana_program::{
    account_info::AccountInfo,
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar,
    sysvar::Sysvar,
};
use solana_system_interface::instruction as system_instruction;
pub use solana_system_interface::program as system_program;
use std::cell::{Ref, RefMut};
pub use thiserror::Error;

pub trait Discriminator {
    fn discriminator() -> u8;
}

pub trait AccountDeserialize {
    fn try_from_bytes(data: &[u8]) -> Result<&Self, ProgramError>;
    fn try_from_bytes_mut(data: &mut [u8]) -> Result<&mut Self, ProgramError>;
}

impl<T> AccountDeserialize for T
where
    T: Discriminator + Pod,
{
    fn try_from_bytes(data: &[u8]) -> Result<&Self, ProgramError> {
        if data.len() < 8 || Self::discriminator() != data[0] {
            return Err(ProgramError::InvalidAccountData);
        }
        bytemuck::try_from_bytes::<Self>(&data[8..]).map_err(|_| ProgramError::InvalidAccountData)
    }

    fn try_from_bytes_mut(data: &mut [u8]) -> Result<&mut Self, ProgramError> {
        if data.len() < 8 || Self::discriminator() != data[0] {
            return Err(ProgramError::InvalidAccountData);
        }
        bytemuck::try_from_bytes_mut::<Self>(&mut data[8..])
            .map_err(|_| ProgramError::InvalidAccountData)
    }
}

pub trait AccountInfoValidation {
    fn is_signer(&self) -> Result<&Self, ProgramError>;
    fn is_writable(&self) -> Result<&Self, ProgramError>;
    fn is_empty(&self) -> Result<&Self, ProgramError>;
    fn is_sysvar(&self, sysvar_id: &Pubkey) -> Result<&Self, ProgramError>;
    fn has_address(&self, address: &Pubkey) -> Result<&Self, ProgramError>;
    fn has_owner(&self, owner: &Pubkey) -> Result<&Self, ProgramError>;
    fn has_seeds(&self, seeds: &[&[u8]], program_id: &Pubkey) -> Result<&Self, ProgramError>;
}

impl AccountInfoValidation for AccountInfo<'_> {
    fn is_signer(&self) -> Result<&Self, ProgramError> {
        if !self.is_signer {
            return Err(trace(
                "Account is not a signer",
                ProgramError::MissingRequiredSignature,
            ));
        }
        Ok(self)
    }

    fn is_writable(&self) -> Result<&Self, ProgramError> {
        if !self.is_writable {
            return Err(trace(
                "Account is not writable",
                ProgramError::InvalidAccountData,
            ));
        }
        Ok(self)
    }

    fn is_empty(&self) -> Result<&Self, ProgramError> {
        if !self.data_is_empty() {
            return Err(trace(
                "Account already initialized",
                ProgramError::AccountAlreadyInitialized,
            ));
        }
        Ok(self)
    }

    fn is_sysvar(&self, sysvar_id: &Pubkey) -> Result<&Self, ProgramError> {
        self.has_owner(&sysvar::ID)?.has_address(sysvar_id)
    }

    fn has_address(&self, address: &Pubkey) -> Result<&Self, ProgramError> {
        if self.key != address {
            return Err(trace(
                "Account has invalid address",
                ProgramError::InvalidAccountData,
            ));
        }
        Ok(self)
    }

    fn has_owner(&self, owner: &Pubkey) -> Result<&Self, ProgramError> {
        if self.owner != owner {
            return Err(trace(
                "Account has invalid owner",
                ProgramError::InvalidAccountOwner,
            ));
        }
        Ok(self)
    }

    fn has_seeds(&self, seeds: &[&[u8]], program_id: &Pubkey) -> Result<&Self, ProgramError> {
        let (pda, _) = Pubkey::find_program_address(seeds, program_id);
        if self.key != &pda {
            return Err(trace(
                "Account has invalid seeds",
                ProgramError::InvalidSeeds,
            ));
        }
        Ok(self)
    }
}

pub trait AsAccount {
    fn as_account<T>(&self, program_id: &Pubkey) -> Result<Ref<'_, T>, ProgramError>
    where
        T: AccountDeserialize + Discriminator + Pod;

    fn as_account_mut<T>(&self, program_id: &Pubkey) -> Result<RefMut<'_, T>, ProgramError>
    where
        T: AccountDeserialize + Discriminator + Pod;
}

impl AsAccount for AccountInfo<'_> {
    fn as_account<T>(&self, program_id: &Pubkey) -> Result<Ref<'_, T>, ProgramError>
    where
        T: AccountDeserialize + Discriminator + Pod,
    {
        self.has_owner(program_id)?;
        let data = self.try_borrow_data()?;
        let expected_len = 8 + core::mem::size_of::<T>();
        if data.len() != expected_len {
            return Err(ProgramError::InvalidAccountData);
        }
        T::try_from_bytes(&data)?;
        Ok(Ref::map(data, |data| bytemuck::from_bytes(&data[8..])))
    }

    fn as_account_mut<T>(&self, program_id: &Pubkey) -> Result<RefMut<'_, T>, ProgramError>
    where
        T: AccountDeserialize + Discriminator + Pod,
    {
        self.has_owner(program_id)?;
        self.is_writable()?;
        let mut data = self.try_borrow_mut_data()?;
        let expected_len = 8 + core::mem::size_of::<T>();
        if data.len() != expected_len {
            return Err(ProgramError::InvalidAccountData);
        }
        T::try_from_bytes_mut(&mut data)?;
        Ok(RefMut::map(data, |data| {
            bytemuck::from_bytes_mut(&mut data[8..])
        }))
    }
}

#[inline(always)]
pub fn close_account<'info>(
    account_info: &AccountInfo<'info>,
    recipient: &AccountInfo<'info>,
) -> ProgramResult {
    **recipient.try_borrow_mut_lamports()? += account_info.lamports();
    **account_info.try_borrow_mut_lamports()? = 0;
    account_info.assign(&system_program::ID);
    account_info.resize(0)
}

#[inline(always)]
pub fn create_program_account<'a, 'info, T: Discriminator + Pod>(
    target_account: &'a AccountInfo<'info>,
    system_program: &'a AccountInfo<'info>,
    payer: &'a AccountInfo<'info>,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> ProgramResult {
    let bump = Pubkey::find_program_address(seeds, owner).1;
    let bump_slice = &[bump];
    let signer_seeds = [seeds, &[bump_slice]].concat();
    let space = 8 + core::mem::size_of::<T>();
    let rent = solana_program::rent::Rent::get()?;

    if target_account.lamports() == 0 {
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
            &[&signer_seeds],
        )?;
    } else {
        let rent_exempt_balance = rent
            .minimum_balance(space)
            .saturating_sub(target_account.lamports());
        if rent_exempt_balance > 0 {
            solana_program::program::invoke(
                &system_instruction::transfer(payer.key, target_account.key, rent_exempt_balance),
                &[
                    payer.clone(),
                    target_account.clone(),
                    system_program.clone(),
                ],
            )?;
        }

        solana_program::program::invoke_signed(
            &system_instruction::allocate(target_account.key, space as u64),
            &[target_account.clone(), system_program.clone()],
            &[&signer_seeds],
        )?;

        solana_program::program::invoke_signed(
            &system_instruction::assign(target_account.key, owner),
            &[target_account.clone(), system_program.clone()],
            &[&signer_seeds],
        )?;
    }

    let mut data = target_account.try_borrow_mut_data()?;
    data[0] = T::discriminator();
    Ok(())
}

#[track_caller]
pub fn trace(msg: &str, error: ProgramError) -> ProgramError {
    let caller = core::panic::Location::caller();
    solana_program::msg!("{}: {}", msg, caller);
    error
}
