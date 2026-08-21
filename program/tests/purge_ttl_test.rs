//! Requests expire by wall-clock time, not a fixed slot count. Drives the real
//! purge instruction with a controlled Clock so the elapsed time is exact.

use ephemeral_vrf_api::prelude::*;
use solana_program::clock::Clock;
use solana_program_test::{processor, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

/// Program-owned queue at index 0 holding items at the given creation slots.
fn queue_with_items(size: usize, slots: &[u64]) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    {
        let mut queue = QueueAccount::load(&mut data).unwrap();
        queue.header.index = 0;
        for (i, slot) in slots.iter().enumerate() {
            let item = QueueItem {
                slot: *slot,
                id: [i as u8 + 1; 32],
                ..QueueItem::default()
            };
            queue.add_item(&item, &[], &[], &[]).unwrap();
        }
    }
    data
}

#[tokio::test]
async fn purge_expires_by_wall_clock_time() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle = Keypair::new();
    let queue_addr = oracle_queue_pda(&oracle.pubkey(), 0).0;
    program_test.add_account(
        oracle.pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: vec![],
            owner: solana_sdk_ids::system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    // Item 1 created at slot 0 (old), item 2 at slot 900 (fresh).
    program_test.add_account(
        queue_addr,
        Account {
            lamports: 10_000_000_000,
            data: queue_with_items(9_500, &[0, 900]),
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let ctx = program_test.start_with_context().await;
    let banks = ctx.banks_client.clone();

    // Controlled clock in epoch 0 (which starts at slot 0): 1000 slots and
    // 130 s have elapsed since epoch start => ~0.13 s/slot.
    //   item 1: age = 1000 * 130 / 1000 = 130 s  > 120 => purged
    //   item 2: age =  100 * 130 / 1000 =  13 s  < 120 => kept
    let mut clock = banks.get_sysvar::<Clock>().await.unwrap();
    clock.epoch = 0;
    clock.slot = 1000;
    clock.epoch_start_timestamp = 0;
    clock.unix_timestamp = 130;
    ctx.set_sysvar(&clock);

    let purge_ix = purge_expired_requests(oracle.pubkey(), 0);
    let bh = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[purge_ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        bh,
    );
    banks.process_transaction(tx).await.expect("purge failed");

    // Only the fresh request (id 2) survives.
    let acct = banks.get_account(queue_addr).await.unwrap().unwrap();
    let mut data = acct.data.clone();
    let queue = QueueAccount::load(&mut data).unwrap();
    let ids: Vec<u8> = queue.iter_items().map(|it| it.id[0]).collect();
    assert_eq!(
        ids,
        vec![2],
        "the old request must be purged, the fresh one kept"
    );
}
