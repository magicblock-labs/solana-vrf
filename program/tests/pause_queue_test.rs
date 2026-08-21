//! An oracle can pause its queue to stop accepting new requests (so it can be
//! drained and closed), and unpause it again. Exercises the real instruction
//! against a program-owned queue account.

use ephemeral_vrf_api::prelude::*;
use solana_program_test::{processor, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

/// Build a valid, empty program-owned queue account for `oracle` at index 0.
fn empty_queue(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    {
        let queue = QueueAccount::load(&mut data).unwrap();
        queue.header.index = 0;
    }
    data
}

async fn paused_flag(
    banks: &solana_program_test::BanksClient,
    queue: solana_program::pubkey::Pubkey,
) -> u8 {
    let acct = banks.get_account(queue).await.unwrap().unwrap();
    Queue::try_from_bytes(&acct.data).unwrap().paused
}

#[tokio::test]
async fn oracle_can_pause_and_unpause_queue() {
    let program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle = Keypair::new();
    let queue_addr = oracle_queue_pda(&oracle.pubkey(), 0).0;

    let mut program_test = program_test;
    program_test.add_account(
        queue_addr,
        Account {
            lamports: 1_000_000_000,
            data: empty_queue(9_500),
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let ctx = program_test.start_with_context().await;
    let banks = ctx.banks_client.clone();

    // A freshly created queue is active (backward compatible: zeroed = active).
    assert_eq!(paused_flag(&banks, queue_addr).await, 0);

    // Oracle pauses its queue.
    let bh = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[set_queue_paused(oracle.pubkey(), 0, true)],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer, &oracle],
        bh,
    );
    banks.process_transaction(tx).await.expect("pause failed");
    assert_eq!(paused_flag(&banks, queue_addr).await, 1);

    // Oracle unpauses its queue.
    let bh = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[set_queue_paused(oracle.pubkey(), 0, false)],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer, &oracle],
        bh,
    );
    banks.process_transaction(tx).await.expect("unpause failed");
    assert_eq!(paused_flag(&banks, queue_addr).await, 0);

    // A non-oracle signer cannot pause the queue.
    let attacker = Keypair::new();
    let mut ix = set_queue_paused(oracle.pubkey(), 0, true);
    ix.accounts[0].pubkey = attacker.pubkey(); // real queue, wrong signer
    let bh = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer, &attacker],
        bh,
    );
    assert!(banks.process_transaction(tx).await.is_err());
    assert_eq!(paused_flag(&banks, queue_addr).await, 0);
}
