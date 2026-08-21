//! Delegation is a one-time setup step: a queue that has already been used
//! (so it may back mainnet integrations) must not be delegatable.

use ephemeral_vrf_api::prelude::*;
use solana_program_test::{processor, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

/// A program-owned queue holding one request (variable region no longer zero).
fn used_queue(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    {
        let mut queue = QueueAccount::load(&mut data).unwrap();
        queue.header.index = 0;
        queue
            .add_item(&QueueItem::default(), &[], &[], &[])
            .unwrap();
    }
    data
}

#[tokio::test]
async fn delegation_rejected_for_used_queue() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let authority = Keypair::new();
    let queue_addr = oracle_queue_pda(&authority.pubkey(), 0).0;
    program_test.add_account(
        queue_addr,
        Account {
            lamports: 1_000_000_000,
            data: used_queue(9_500),
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let ctx = program_test.start_with_context().await;
    let banks = ctx.banks_client.clone();

    let ix = delegate_oracle_queue(authority.pubkey(), queue_addr, 0);
    let bh = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer, &authority],
        bh,
    );

    let err = banks
        .process_transaction(tx)
        .await
        .expect_err("delegating a used queue must be rejected");
    assert_eq!(
        err.unwrap(),
        solana_sdk::transaction::TransactionError::InstructionError(
            0,
            solana_program::instruction::InstructionError::Custom(
                EphemeralVrfError::QueueAlreadyInUse as u32
            )
        )
    );
}
