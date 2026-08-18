//! Live test for the 1 MiB queue size cap (MAX_QUEUE_ACCOUNT_SIZE).
//!
//! Runs the real `initialize_oracle_queue` instruction: an over-limit
//! `target_size` must be rejected with `QueueSizeTooLarge`, while a valid
//! size must still initialize the queue correctly.

use ephemeral_vrf_api::prelude::*;
use solana_curve25519::ristretto::PodRistrettoPoint;
use solana_program_test::{processor, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

fn oracle_data_account(
    identity: &solana_program::pubkey::Pubkey,
) -> (solana_program::pubkey::Pubkey, Account) {
    let oracle = Oracle {
        vrf_pubkey: PodRistrettoPoint([0; 32]),
        registration_slot: 0,
        open_queue: 0,
    };
    let mut data = AccountDiscriminator::Oracle.to_bytes().to_vec();
    data.extend_from_slice(oracle.to_bytes());
    (
        oracle_data_pda(identity).0,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

#[tokio::test]
async fn queue_size_is_capped_at_max() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle_keypair = Keypair::new();
    program_test.add_account(
        oracle_keypair.pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: vec![],
            owner: solana_sdk_ids::system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let (oracle_data_address, oracle_data) = oracle_data_account(&oracle_keypair.pubkey());
    program_test.add_account(oracle_data_address, oracle_data);

    let mut context = program_test.start_with_context().await;
    let banks = context.banks_client.clone();

    // Oracle must have been registered for at least 200 slots.
    context.warp_to_slot(250).unwrap();

    // Over-limit target size must be rejected with QueueSizeTooLarge.
    // (The guard runs before any realloc, so a single ix is enough even
    // though the SDK would emit one ix per 10 KiB step.)
    let ixs = initialize_oracle_queue(
        context.payer.pubkey(),
        oracle_keypair.pubkey(),
        0,
        Some(MAX_QUEUE_ACCOUNT_SIZE + 1),
    );
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ixs[0].clone()],
        Some(&context.payer.pubkey()),
        &[&context.payer, &oracle_keypair],
        blockhash,
    );
    let err = banks
        .process_transaction(tx)
        .await
        .expect_err("over-limit queue size must be rejected");
    assert_eq!(
        err.unwrap(),
        solana_sdk::transaction::TransactionError::InstructionError(
            0,
            solana_program::instruction::InstructionError::Custom(
                EphemeralVrfError::QueueSizeTooLarge as u32
            )
        )
    );

    // A valid size still initializes the queue correctly.
    let target_size = 9_500u32;
    let ixs = initialize_oracle_queue(
        context.payer.pubkey(),
        oracle_keypair.pubkey(),
        1,
        Some(target_size),
    );
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ixs[0].clone()],
        Some(&context.payer.pubkey()),
        &[&context.payer, &oracle_keypair],
        blockhash,
    );
    banks
        .process_transaction(tx)
        .await
        .expect("valid queue init failed");

    let (queue_address, _) = oracle_queue_pda(&oracle_keypair.pubkey(), 1);
    let queue_account = banks.get_account(queue_address).await.unwrap().unwrap();
    assert_eq!(queue_account.data.len(), target_size as usize);
    let header = Queue::try_from_bytes(&queue_account.data).unwrap();
    assert_eq!(header.index, 1);
    assert_eq!(header.len(), 0);

    // The oracle's open queue count was incremented.
    let oracle_account = banks
        .get_account(oracle_data_address)
        .await
        .unwrap()
        .unwrap();
    let expected = Oracle {
        vrf_pubkey: PodRistrettoPoint([0; 32]),
        registration_slot: 0,
        open_queue: 1,
    };
    assert_eq!(&oracle_account.data[8..], expected.to_bytes());
}
