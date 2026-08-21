//! Live end-to-end regression test for the O(n) purge fix (GHSA-88jr-67m8-w8cq).
//!
//! Stages the worst-case queue from the advisory — a 30,000-byte account
//! filled with 312 minimal, expired requests — and executes the real
//! `purge_expired_requests` instruction through the Banks runtime with the
//! mainnet compute cap (1.4M CU). Before the fix this transaction could
//! never land; after the fix it must succeed and fully drain the queue.

use ephemeral_vrf_api::prelude::*;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_program_test::{processor, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

/// Mainnet queue account size (advisory: 29,976 usable bytes after
/// discriminator + header, i.e. 312 minimal 96-byte requests).
const QUEUE_ACCOUNT_SIZE: usize = 30_000;
/// Mainnet per-transaction compute unit cap.
const MAINNET_CU_LIMIT: u32 = 1_400_000;

/// Build the exact byte content of a full queue account whose items are all
/// expired (slot 0), using the real `QueueAccount` implementation so the
/// layout is identical to what on-chain requests produce.
fn full_expired_queue(index: u8, account_size: usize) -> (Vec<u8>, usize) {
    let mut data = vec![0u8; account_size];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    let mut queue = QueueAccount::load(&mut data).unwrap();
    queue.header.index = index;

    let expired_item = QueueItem {
        slot: 0,
        ..QueueItem::default()
    };
    let mut added = 0usize;
    while queue.add_item(&expired_item, &[], &[], &[]).is_ok() {
        added += 1;
    }
    (data, added)
}

fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

#[tokio::test]
async fn purge_full_queue_within_mainnet_compute_budget() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle_keypair = Keypair::new();
    let (queue_address, _) = oracle_queue_pda(&oracle_keypair.pubkey(), 0);

    // Oracle account (receives the reclaimed fees).
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

    // Worst-case queue: completely full of expired minimal requests.
    let (queue_data, item_count) = full_expired_queue(0, QUEUE_ACCOUNT_SIZE);
    assert_eq!(item_count, 312, "advisory worst case is 312 requests");
    let queue_lamports = 10_000_000_000; // rent + reclaimable fees
    program_test.add_account(
        queue_address,
        Account {
            lamports: queue_lamports,
            data: queue_data,
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut context = program_test.start_with_context().await;
    let banks = context.banks_client.clone();

    // Sanity: queue is full.
    let queue_account = banks.get_account(queue_address).await.unwrap().unwrap();
    assert_eq!(
        Queue::try_from_bytes(&queue_account.data).unwrap().len(),
        312
    );

    // Advance past the TTL so every staged request is expired.
    let slot = banks
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap()
        .slot;
    context.warp_to_slot(slot + 1_000).unwrap();
    let mut clock = context
        .banks_client
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap();
    // Expiry is wall-clock based; push the Clock timestamp well past the TTL.
    clock.unix_timestamp = clock.epoch_start_timestamp + 1_000_000;
    context.set_sysvar(&clock);

    // Execute the real purge instruction under the mainnet CU cap.
    let budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(MAINNET_CU_LIMIT);
    let purge_ix = purge_expired_requests(oracle_keypair.pubkey(), 0);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[budget_ix, purge_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let outcome = banks
        .process_transaction_with_metadata(tx)
        .await
        .expect("purge transaction failed to execute");
    let metadata = outcome.metadata.expect("missing transaction metadata");
    assert!(
        outcome.result.is_ok(),
        "purge of a full queue must succeed: {:?}\nlogs: {:?}",
        outcome.result,
        metadata.log_messages
    );
    let cu = metadata.compute_units_consumed;
    println!("purge of 312 expired requests consumed {} CU", cu);
    assert!(
        cu < 200_000,
        "purge must keep wide headroom under the 1.4M CU cap, got {cu} CU"
    );

    // Queue fully drained and cursor reset to the start of the items region.
    let queue_account = banks.get_account(queue_address).await.unwrap().unwrap();
    let header = Queue::try_from_bytes(&queue_account.data).unwrap();
    assert_eq!(header.len(), 0);
    let items_start = align_up(
        core::mem::size_of::<Queue>(),
        core::mem::align_of::<QueueItem>(),
    );
    assert_eq!(header.cursor as usize, items_start);

    // Reclaimed fees paid to the oracle: 312 * VRF_LAMPORTS_COST.
    let expected_fees = 312 * VRF_LAMPORTS_COST;
    let oracle_account = banks
        .get_account(oracle_keypair.pubkey())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(oracle_account.lamports, 1_000_000_000 + expected_fees);
    assert_eq!(queue_account.lamports, queue_lamports - expected_fees);
}

/// Live requests must survive a purge untouched: expired items become
/// reusable holes, and trailing expired items shrink the cursor.
#[tokio::test]
async fn purge_mixed_queue_keeps_live_requests() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle_keypair = Keypair::new();
    let (queue_address, _) = oracle_queue_pda(&oracle_keypair.pubkey(), 0);
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

    let mut context = program_test.start_with_context().await;
    let banks = context.banks_client.clone();
    let live_slot = banks
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap()
        .slot
        + 1_000_000; // far in the future: never expires during the test

    // 312 items: alternate expired/live for the first 200 (middle holes),
    // then 112 expired at the tail (trailing holes).
    let mut data = vec![0u8; QUEUE_ACCOUNT_SIZE];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    let (expired_count, live_count) = {
        let mut queue = QueueAccount::load(&mut data).unwrap();
        let mut expired = 0u64;
        let mut live = 0u64;
        for i in 0..312usize {
            let is_live = i < 200 && i % 2 == 0;
            let item = QueueItem {
                slot: if is_live { live_slot } else { 0 },
                id: [i as u8; 32],
                ..QueueItem::default()
            };
            queue.add_item(&item, &[], &[], &[]).unwrap();
            if is_live {
                live += 1;
            } else {
                expired += 1;
            }
        }
        (expired as usize, live as usize)
    };
    assert_eq!((expired_count, live_count), (212, 100));
    context.set_account(
        &queue_address,
        &Account {
            lamports: 10_000_000_000,
            data,
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    // Advance past the TTL and purge.
    let slot = banks
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap()
        .slot;
    context.warp_to_slot(slot + 1_000).unwrap();
    let mut clock = context
        .banks_client
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap();
    // Expiry is wall-clock based; push the Clock timestamp well past the TTL.
    clock.unix_timestamp = clock.epoch_start_timestamp + 1_000_000;
    context.set_sysvar(&clock);
    let purge_ix = purge_expired_requests(oracle_keypair.pubkey(), 0);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[purge_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    banks.process_transaction(tx).await.expect("purge failed");

    // All and only the live requests survive, in order and intact.
    let queue_account = banks.get_account(queue_address).await.unwrap().unwrap();
    let mut data = queue_account.data.clone();
    let mut queue = QueueAccount::load(&mut data).unwrap();
    let survivors: Vec<QueueItem> = queue.iter_items().collect();
    assert_eq!(survivors.len(), live_count);
    for (n, item) in survivors.iter().enumerate() {
        assert_eq!(item.slot, live_slot);
        assert_eq!(item.id, [(2 * n) as u8; 32]); // original even-indexed items
    }

    // Cursor trimmed to the end of the last live item (original index 198).
    let items_start = align_up(
        core::mem::size_of::<Queue>(),
        core::mem::align_of::<QueueItem>(),
    );
    let item_span = align_up(
        core::mem::size_of::<QueueItem>(),
        core::mem::align_of::<QueueItem>(),
    );
    assert_eq!(queue.header.cursor as usize, items_start + 199 * item_span);

    // Middle holes are reusable: adding a live item fills the first hole.
    let new_item = QueueItem {
        slot: live_slot,
        id: [255; 32],
        ..QueueItem::default()
    };
    let logical_index = queue.add_item(&new_item, &[], &[], &[]).unwrap();
    assert_eq!(logical_index, 1); // reused the hole left by expired item 1
    assert_eq!(queue.get_item_by_index(1).unwrap().id, [255; 32]);
    assert_eq!(queue.len(), live_count + 1);
}

/// Worst case under the size cap: a max-size (512 KiB) queue completely full
/// of expired minimal requests must still be purgeable in one transaction.
#[tokio::test]
async fn purge_max_size_queue_within_mainnet_compute_budget() {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    let oracle_keypair = Keypair::new();
    let (queue_address, _) = oracle_queue_pda(&oracle_keypair.pubkey(), 0);
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

    let (queue_data, item_count) = full_expired_queue(0, MAX_QUEUE_ACCOUNT_SIZE as usize);
    println!("max-size queue holds {item_count} minimal requests");
    let queue_lamports = 20_000_000_000_000; // rent + reclaimable fees at this size
    program_test.add_account(
        queue_address,
        Account {
            lamports: queue_lamports,
            data: queue_data,
            owner: ephemeral_vrf_api::ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut context = program_test.start_with_context().await;
    let banks = context.banks_client.clone();
    let slot = banks
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap()
        .slot;
    context.warp_to_slot(slot + 1_000).unwrap();
    let mut clock = context
        .banks_client
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .unwrap();
    // Expiry is wall-clock based; push the Clock timestamp well past the TTL.
    clock.unix_timestamp = clock.epoch_start_timestamp + 1_000_000;
    context.set_sysvar(&clock);

    let budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(MAINNET_CU_LIMIT);
    let purge_ix = purge_expired_requests(oracle_keypair.pubkey(), 0);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[budget_ix, purge_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let outcome = banks
        .process_transaction_with_metadata(tx)
        .await
        .expect("purge transaction failed to execute");
    let metadata = outcome.metadata.expect("missing transaction metadata");
    assert!(
        outcome.result.is_ok(),
        "purge of a max-size queue must succeed: {:?}",
        outcome.result
    );
    let cu = metadata.compute_units_consumed;
    println!("purge of {item_count} expired requests consumed {cu} CU");
    assert!(
        cu < MAINNET_CU_LIMIT as u64,
        "purge of a max-size queue must fit the mainnet CU cap, got {cu} CU"
    );

    let queue_account = banks.get_account(queue_address).await.unwrap().unwrap();
    let header = Queue::try_from_bytes(&queue_account.data).unwrap();
    assert_eq!(header.len(), 0);
    let items_start = align_up(
        core::mem::size_of::<Queue>(),
        core::mem::align_of::<QueueItem>(),
    );
    assert_eq!(header.cursor as usize, items_start);

    let expected_fees = item_count as u64 * VRF_LAMPORTS_COST;
    let oracle_account = banks
        .get_account(oracle_keypair.pubkey())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(oracle_account.lamports, 1_000_000_000 + expected_fees);
}
