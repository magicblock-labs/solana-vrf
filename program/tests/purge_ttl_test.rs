//! Requests expire by wall-clock age from their own creation timestamp.
//! Drives the real purge instruction with a controlled Clock.

use solana_program::clock::Clock;
use solana_program::epoch_schedule::EpochSchedule;
use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::account::Account;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};
use solana_vrf_api::prelude::*;

/// Program-owned queue at index 0 holding items with the given `created_at`
/// stamps. Item ids are 1-based positions, so survivors can be asserted by id.
fn queue_with_items(size: usize, created_at: &[u16]) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
    {
        let mut queue = QueueAccount::load(&mut data).unwrap();
        queue.header.index = 0;
        for (i, stamp) in created_at.iter().enumerate() {
            let item = QueueItem {
                created_at: *stamp,
                id: [i as u8 + 1; 32],
                ..QueueItem::default()
            };
            queue.add_item(&item, &[], &[], &[]).unwrap();
        }
    }
    data
}

struct Harness {
    context: ProgramTestContext,
    oracle: Keypair,
    queue_addr: Pubkey,
}

async fn start() -> Harness {
    let mut program_test = ProgramTest::new(
        "solana_vrf_program",
        solana_vrf_api::ID,
        processor!(solana_vrf_program::process_instruction),
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
    let context = program_test.start_with_context().await;
    Harness {
        context,
        oracle,
        queue_addr,
    }
}

impl Harness {
    fn stage_queue(&mut self, created_at: &[u16]) {
        self.context.set_account(
            &self.queue_addr,
            &Account {
                lamports: 10_000_000_000,
                data: queue_with_items(9_500, created_at),
                owner: solana_vrf_api::ID,
                executable: false,
                rent_epoch: 0,
            }
            .into(),
        );
    }

    async fn set_time(&mut self, unix_timestamp: i64) {
        let mut clock = self
            .context
            .banks_client
            .get_sysvar::<Clock>()
            .await
            .unwrap();
        clock.unix_timestamp = unix_timestamp;
        self.context.set_sysvar(&clock);
    }

    /// Run the purge and return the surviving item ids.
    async fn purge(&mut self) -> Vec<u8> {
        let banks = self.context.banks_client.clone();
        let purge_ix = purge_expired_requests(self.oracle.pubkey(), 0);
        let bh = banks.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[purge_ix],
            Some(&self.context.payer.pubkey()),
            &[&self.context.payer],
            bh,
        );
        banks.process_transaction(tx).await.expect("purge failed");
        let acct = banks.get_account(self.queue_addr).await.unwrap().unwrap();
        let mut data = acct.data.clone();
        let queue = QueueAccount::load(&mut data).unwrap();
        queue.iter_items().map(|it| it.id[0]).collect()
    }
}

const NOW: i64 = 1_789_086_305;

#[tokio::test]
async fn purge_expires_by_wall_clock_time() {
    let mut h = start().await;
    // item 1: 124 s old => purged; item 2: 116 s old => kept.
    h.stage_queue(&[
        QueueItem::created_at_from(NOW - 124),
        QueueItem::created_at_from(NOW - 116),
    ]);
    h.set_time(NOW).await;

    assert_eq!(h.purge().await, vec![2]);
}

/// Age must not depend on slots or epochs: devnet's stale EpochSchedule
/// (8,192 slots per epoch against a 432,000-slot clock) changes nothing.
#[tokio::test]
async fn purge_ignores_epoch_sysvars() {
    let mut h = start().await;
    h.context
        .set_sysvar(&EpochSchedule::custom(8_192, 8_192, false));
    let mut clock = h.context.banks_client.get_sysvar::<Clock>().await.unwrap();
    clock.slot = 496_404_516;
    clock.epoch = 1_149;
    clock.epoch_start_timestamp = 0;
    h.context.set_sysvar(&clock);

    h.stage_queue(&[
        QueueItem::created_at_from(NOW - 40 * 3_600),
        QueueItem::created_at_from(NOW - 30),
    ]);
    h.set_time(NOW).await;

    assert_eq!(h.purge().await, vec![2]);
}

/// The 16-bit stamp wraps; ages are taken modulo the wrap.
#[tokio::test]
async fn purge_handles_created_at_wrap() {
    let mut h = start().await;
    // `now` is 8 s past a wrap boundary of the stamp.
    let now = ((NOW / CREATED_AT_UNIT_SECS) / 65_536 + 1) * 65_536 * CREATED_AT_UNIT_SECS + 8;
    assert_eq!(QueueItem::created_at_from(now), 2);
    // item 1: created 200 s before the wrap => purged
    // item 2: created  60 s before the wrap => kept
    // item 3: created in this unit => kept
    h.stage_queue(&[
        QueueItem::created_at_from(now - 208),
        QueueItem::created_at_from(now - 68),
        QueueItem::created_at_from(now),
    ]);
    h.set_time(now).await;

    assert_eq!(h.purge().await, vec![2, 3]);
}
