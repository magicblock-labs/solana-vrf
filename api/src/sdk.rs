use crate::prelude::*;
use crate::steel::*;
use crate::ID;
use ephemeral_rollups_sdk::consts::{DELEGATION_PROGRAM_ID, MAGIC_CONTEXT_ID, MAGIC_PROGRAM_ID};
use ephemeral_rollups_sdk::pda::{
    delegate_buffer_pda_from_delegated_account_and_owner_program,
    delegation_metadata_pda_from_delegated_account, delegation_record_pda_from_delegated_account,
};
use solana_curve25519::ristretto::PodRistrettoPoint;
use solana_curve25519::scalar::PodScalar;
use solana_sdk_ids::bpf_loader_upgradeable;

pub fn initialize(signer: Pubkey) -> Instruction {
    Instruction {
        program_id: ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(oracles_pda().0, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: Initialize {}.to_bytes(),
    }
}

pub fn add_oracle(signer: Pubkey, identity: Pubkey, oracle_pubkey: [u8; 32]) -> Instruction {
    let oracle_pubkey = PodRistrettoPoint(oracle_pubkey);
    let program_data_address =
        Pubkey::find_program_address(&[crate::ID.as_ref()], &bpf_loader_upgradeable::id()).0;
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(oracles_pda().0, false),
            AccountMeta::new(oracle_data_pda(&identity).0, false),
            AccountMeta::new_readonly(program_data_address, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ModifyOracle {
            identity,
            oracle_pubkey,
            operation: 0,
        }
        .to_bytes(),
    }
}

pub fn remove_oracle(signer: Pubkey, identity: Pubkey) -> Instruction {
    let program_data_address =
        Pubkey::find_program_address(&[crate::ID.as_ref()], &bpf_loader_upgradeable::id()).0;
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(oracles_pda().0, false),
            AccountMeta::new(oracle_data_pda(&identity).0, false),
            AccountMeta::new_readonly(program_data_address, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: ModifyOracle {
            identity,
            oracle_pubkey: PodRistrettoPoint::default(),
            operation: 1,
        }
        .to_bytes(),
    }
}

/// Returns a list of instructions to initialize an oracle queue. The initialize_oracle_queue is
/// repeated to alloc chunks of 10240 bytes, which is the maximum per instruction.
/// Should still be run in a single transaction.
pub fn initialize_oracle_queue(
    signer: Pubkey,
    identity: Pubkey,
    index: u8,
    bytes_to_allocate: Option<u32>,
) -> Vec<Instruction> {
    println!(
        "Queue: {:?}",
        oracle_queue_pda(&identity, index).0.to_string()
    );
    let target_size = bytes_to_allocate.unwrap_or(9500);
    let inits = target_size.div_ceil(10240);
    let mut ixs = Vec::with_capacity(inits as usize);
    for _ in 0..inits {
        ixs.push(Instruction {
            program_id: ID,
            accounts: vec![
                AccountMeta::new(signer, true),
                AccountMeta::new_readonly(identity, true),
                AccountMeta::new(oracle_data_pda(&identity).0, false),
                AccountMeta::new(oracle_queue_pda(&identity, index).0, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: InitializeOracleQueue::new(index, target_size).to_bytes(),
        })
    }
    ixs
}

#[allow(clippy::too_many_arguments)]
pub fn provide_randomness(
    oracle_identity: Pubkey,
    oracle_queue: Pubkey,
    callback_program_id: Pubkey,
    rnd_seed: [u8; 32],
    output: PodRistrettoPoint,
    commitment_base_compressed: PodRistrettoPoint,
    commitment_hash_compressed: PodRistrettoPoint,
    s: PodScalar,
) -> Instruction {
    provide_randomness_with_identity_mode(
        oracle_identity,
        oracle_queue,
        callback_program_id,
        1,
        rnd_seed,
        output,
        commitment_base_compressed,
        commitment_hash_compressed,
        s,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn provide_randomness_legacy(
    oracle_identity: Pubkey,
    oracle_queue: Pubkey,
    callback_program_id: Pubkey,
    rnd_seed: [u8; 32],
    output: PodRistrettoPoint,
    commitment_base_compressed: PodRistrettoPoint,
    commitment_hash_compressed: PodRistrettoPoint,
    s: PodScalar,
) -> Instruction {
    provide_randomness_with_identity_mode(
        oracle_identity,
        oracle_queue,
        callback_program_id,
        0,
        rnd_seed,
        output,
        commitment_base_compressed,
        commitment_hash_compressed,
        s,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn provide_randomness_with_identity_mode(
    oracle_identity: Pubkey,
    oracle_queue: Pubkey,
    callback_program_id: Pubkey,
    identity_mode: u8,
    rnd_seed: [u8; 32],
    output: PodRistrettoPoint,
    commitment_base_compressed: PodRistrettoPoint,
    commitment_hash_compressed: PodRistrettoPoint,
    s: PodScalar,
) -> Instruction {
    let program_identity = if identity_mode == 1 {
        scoped_identity_pda(&callback_program_id).0
    } else {
        program_identity_pda().0
    };

    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(oracle_identity, true),
            AccountMeta::new_readonly(program_identity, false),
            AccountMeta::new_readonly(oracle_data_pda(&oracle_identity).0, false),
            AccountMeta::new(oracle_queue, false),
            AccountMeta::new_readonly(callback_program_id, false),
        ],
        data: ProvideRandomness {
            input: rnd_seed,
            output,
            commitment_base_compressed,
            commitment_hash_compressed,
            scalar: s,
        }
        .to_bytes(),
    }
}

pub fn purge_expired_requests(identity: Pubkey, index: u8) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(identity, false),
            AccountMeta::new(oracle_queue_pda(&identity, index).0, false),
        ],
        data: PurgeExpiredRequests { index }.to_bytes(),
    }
}

pub fn delegate_oracle_queue(signer: Pubkey, queue: Pubkey, index: u8) -> Instruction {
    let buffer = delegate_buffer_pda_from_delegated_account_and_owner_program(&queue, &crate::ID);
    let delegation_record = delegation_record_pda_from_delegated_account(&queue);
    let delegation_metadata = delegation_metadata_pda_from_delegated_account(&queue);
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(queue, false),
            AccountMeta::new(buffer, false),
            AccountMeta::new(delegation_record, false),
            AccountMeta::new(delegation_metadata, false),
            AccountMeta::new_readonly(DELEGATION_PROGRAM_ID, false),
            AccountMeta::new_readonly(crate::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: DelegateOracleQueue { index }.to_bytes(),
    }
}

pub fn undelegate_oracle_queue(signer: Pubkey, queue: Pubkey, index: u8) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(queue, false),
            AccountMeta::new(MAGIC_CONTEXT_ID, false),
            AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
        ],
        data: UndelegateOracleQueue { index }.to_bytes(),
    }
}

pub fn close_oracle_queue(identity: Pubkey, index: u8) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(identity, true),
            AccountMeta::new(oracle_data_pda(&identity).0, false),
            AccountMeta::new(oracle_queue_pda(&identity, index).0, false),
        ],
        data: CloseOracleQueue { index }.to_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provide_randomness_test_ix(identity_mode: u8) -> Instruction {
        provide_randomness_with_identity_mode(
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
            Pubkey::new_from_array([3; 32]),
            identity_mode,
            [4; 32],
            PodRistrettoPoint([5; 32]),
            PodRistrettoPoint([6; 32]),
            PodRistrettoPoint([7; 32]),
            PodScalar([8; 32]),
        )
    }

    #[test]
    fn provide_randomness_uses_scoped_identity_by_default() {
        let callback_program_id = Pubkey::new_from_array([3; 32]);
        let ix = provide_randomness(
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
            callback_program_id,
            [4; 32],
            PodRistrettoPoint([5; 32]),
            PodRistrettoPoint([6; 32]),
            PodRistrettoPoint([7; 32]),
            PodScalar([8; 32]),
        );

        assert_eq!(
            ix.accounts[1].pubkey,
            scoped_identity_pda(&callback_program_id).0
        );
    }

    #[test]
    fn provide_randomness_identity_mode_selects_legacy_identity() {
        let ix = provide_randomness_test_ix(0);
        assert_eq!(ix.accounts[1].pubkey, program_identity_pda().0);
    }

    #[test]
    fn provide_randomness_identity_mode_selects_scoped_identity() {
        let callback_program_id = Pubkey::new_from_array([3; 32]);
        let ix = provide_randomness_test_ix(1);
        assert_eq!(
            ix.accounts[1].pubkey,
            scoped_identity_pda(&callback_program_id).0
        );
    }
}
