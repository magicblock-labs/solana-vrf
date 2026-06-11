#![allow(unexpected_cfgs)]
mod close_oracle_queue;
mod delegate_oracle_queue;
mod fees;
mod initialize;
mod initialize_oracle_queue;
mod modify_oracles;
mod process_undelegation;
mod provide_randomness;
mod purge_expired_requests;
mod request_randomness;
mod undelegate_oracle_queue;

use close_oracle_queue::*;
use delegate_oracle_queue::*;
use initialize::*;
use initialize_oracle_queue::*;
use modify_oracles::*;
use process_undelegation::*;
use provide_randomness::*;
use purge_expired_requests::*;
use request_randomness::*;
use undelegate_oracle_queue::*;

use ephemeral_vrf_api::prelude::*;

fn parse_instruction8<'a, T: std::convert::TryFrom<u8>>(
    api_id: &'a Pubkey,
    program_id: &'a Pubkey,
    data: &'a [u8],
) -> Result<(T, &'a [u8]), ProgramError> {
    if program_id.ne(api_id) {
        return Err(ProgramError::IncorrectProgramId);
    }
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let tag = data[0];
    let ix = T::try_from(tag).or(Err(ProgramError::InvalidInstructionData))?;
    Ok((ix, &data[8..]))
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (ix, data) = parse_instruction8(&ephemeral_vrf_api::ID, program_id, data)?;
    match ix {
        EphemeralVrfInstruction::Initialize => process_initialize(accounts, data)?,
        EphemeralVrfInstruction::ModifyOracle => process_modify_oracles(accounts, data)?,
        EphemeralVrfInstruction::InitializeOracleQueue => {
            process_initialize_oracle_queue(accounts, data)?
        }
        EphemeralVrfInstruction::RequestHighPriorityRandomness => {
            process_request_randomness(accounts, data, true, false)?
        }
        EphemeralVrfInstruction::RequestRandomness => {
            process_request_randomness(accounts, data, false, false)?
        }
        EphemeralVrfInstruction::RequestHighPriorityRandomnessScoped => {
            process_request_randomness(accounts, data, true, true)?
        }
        EphemeralVrfInstruction::RequestRandomnessScoped => {
            process_request_randomness(accounts, data, false, true)?
        }
        EphemeralVrfInstruction::ProvideRandomness => process_provide_randomness(accounts, data)?,
        EphemeralVrfInstruction::DelegateOracleQueue => {
            process_delegate_oracle_queue(accounts, data)?
        }
        EphemeralVrfInstruction::UndelegateOracleQueue => {
            process_undelegate_oracle_queue(accounts, data)?
        }
        EphemeralVrfInstruction::ProcessUndelegation => process_undelegation(accounts, data)?,
        EphemeralVrfInstruction::CloseOracleQueue => process_close_oracle_queue(accounts, data)?,
        EphemeralVrfInstruction::PurgeExpiredRequests => {
            process_purge_expired_requests(accounts, data)?
        }
    }

    Ok(())
}
solana_program::entrypoint!(process_instruction);
