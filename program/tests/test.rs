mod fixtures;

use crate::fixtures::{TEST_AUTHORITY, TEST_CALLBACK_PROGRAM, TEST_ORACLE};
use ephemeral_rollups_sdk::consts::DELEGATION_PROGRAM_ID;
use ephemeral_vrf::vrf::{compute_vrf, generate_vrf_keypair, verify_vrf};
use ephemeral_vrf_api::prelude::*;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_curve25519::ristretto::PodRistrettoPoint;
use solana_curve25519::scalar::PodScalar;
use solana_program::rent::Rent;
use solana_program::sysvar::slot_hashes;
use solana_program_test::{processor, read_file, ProgramTest, ProgramTestContext};
use solana_sdk::account::Account;
use solana_sdk::{pubkey, signature::Keypair, signer::Signer, transaction::Transaction};

async fn setup() -> ProgramTestContext {
    let mut program_test = ProgramTest::new(
        "ephemeral_vrf_program",
        ephemeral_vrf_api::ID,
        processor!(ephemeral_vrf_program::process_instruction),
    );

    // Setup the test authority
    program_test.add_account(
        Keypair::try_from(&TEST_AUTHORITY[..]).unwrap().pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    // Setup the oracle
    program_test.add_account(
        Keypair::try_from(&TEST_ORACLE[..]).unwrap().pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    // Setup program to test vrf-macro
    let data = read_file("tests/integration/use-randomness/target/deploy/use_randomness.so");
    program_test.add_account(
        TEST_CALLBACK_PROGRAM,
        Account {
            lamports: Rent::default().minimum_balance(data.len()).max(1),
            data,
            owner: solana_sdk_ids::bpf_loader::id(),
            executable: true,
            rent_epoch: 0,
        },
    );

    // Setup delegation program
    let data = read_file("tests/integration/use-randomness/tests/fixtures/dlp.so");
    program_test.add_account(
        DELEGATION_PROGRAM_ID,
        Account {
            lamports: Rent::default().minimum_balance(data.len()).max(1),
            data,
            owner: solana_sdk_ids::bpf_loader::id(),
            executable: true,
            rent_epoch: 0,
        },
    );

    program_test.prefer_bpf(true);
    program_test.start_with_context().await
}

#[tokio::test]
async fn run_test() {
    // Setup test
    let mut context = setup().await;
    let banks = context.banks_client.clone();

    let authority_keypair = Keypair::try_from(&TEST_AUTHORITY[..]).unwrap();
    let oracle_keypair = Keypair::try_from(&TEST_ORACLE[..]).unwrap();

    // Submit initialize transaction.
    let ix = initialize(context.payer.pubkey());
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify was initialized.
    let oracles_address = oracles_pda().0;
    let oracles_account = banks.get_account(oracles_address).await.unwrap().unwrap();
    let oracles = Oracles::try_from_bytes_with_discriminator(&oracles_account.data).unwrap();
    assert_eq!(oracles_account.owner, ephemeral_vrf_api::ID);
    assert_eq!(oracles.oracles.len(), 0);

    // Submit add oracle transaction.
    let (oracle_vrf_sk, oracle_vrf_pk) = generate_vrf_keypair(&oracle_keypair);
    let ix = add_oracle(
        authority_keypair.pubkey(),
        oracle_keypair.pubkey(),
        oracle_vrf_pk.compress().to_bytes(),
    );

    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&authority_keypair.pubkey()),
        &[&authority_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify oracle was added.
    let oracles_info = banks.get_account(oracles_address).await.unwrap().unwrap();
    let oracles_data = oracles_info.data;
    let oracles = Oracles::try_from_bytes_with_discriminator(&oracles_data).unwrap();
    assert!(oracles
        .oracles
        .iter()
        .any(|o| o.eq(&oracle_keypair.pubkey())));

    let oracle_data_info = banks
        .get_account(oracle_data_pda(&oracle_keypair.pubkey()).0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(oracle_data_info.owner, ephemeral_vrf_api::ID);
    let oracle_data = Oracle::try_from_bytes(&oracle_data_info.data).unwrap();
    assert!(oracle_data.registration_slot > 0);
    assert_eq!(
        oracle_data.vrf_pubkey.0,
        oracle_vrf_pk.compress().to_bytes()
    );

    // Advance to current slot + 200
    let current_slot = banks.get_sysvar::<Clock>().await.unwrap().slot;
    context.warp_to_slot(current_slot + 200).unwrap();

    // Submit init oracle queue transaction.
    let target_size = 50_000u32;
    let ixs = initialize_oracle_queue(
        context.payer.pubkey(),
        oracle_keypair.pubkey(),
        0,
        Some(target_size),
    );
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&context.payer.pubkey()),
        &[&context.payer, &oracle_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify queue was initialized.
    let oracle_queue_address = oracle_queue_pda(&oracle_keypair.pubkey(), 0).0;
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let oracle_queue = Queue::try_from_bytes(&oracle_queue_account.data).unwrap();
    assert_eq!(oracle_queue_account.owner, ephemeral_vrf_api::ID);
    assert_eq!(oracle_queue_account.data.len(), target_size as usize);
    assert_eq!(oracle_queue.index, 0);
    assert_eq!(oracle_queue.item_count, 0);

    // Submit request for randomness transaction.
    let ix = request_randomness(context.payer.pubkey(), 0);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify request was added to queue.
    let oracle_queue_address = oracle_queue_pda(&oracle_keypair.pubkey(), 0).0;
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let mut qdata = oracle_queue_account.data.clone();
    let queue_acc = QueueAccount::load(&mut qdata[8..]).unwrap();
    assert_eq!(oracle_queue_account.owner, ephemeral_vrf_api::ID);
    assert_eq!(queue_acc.len(), 1);

    // Verify cost of the vrf was collected in the oracle queue account.
    assert_eq!(
        oracle_queue_account.lamports,
        banks
            .get_rent()
            .await
            .unwrap()
            .minimum_balance(oracle_queue_account.data.len())
            + VRF_LAMPORTS_COST
    );

    // Advance to a later slot
    let current_slot = banks.get_sysvar::<Clock>().await.unwrap().slot;
    context.warp_to_slot(current_slot + 1).unwrap();

    // Compute off-chain VRF
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let mut qdata2 = oracle_queue_account.data.clone();
    let queue_acc2 = QueueAccount::load(&mut qdata2[8..]).unwrap();
    let vrf_input = queue_acc2.get_item_by_index(0).unwrap().id;
    let (output, (commitment_base_compressed, commitment_hash_compressed, s)) =
        compute_vrf(oracle_vrf_sk, &vrf_input);

    // Verify generated randomness is correct.
    let verified = verify_vrf(
        oracle_vrf_pk,
        &vrf_input,
        output,
        (commitment_base_compressed, commitment_hash_compressed, s),
    );
    assert!(verified);

    // Submit provide randomness transaction.
    let ix = provide_randomness(
        oracle_keypair.pubkey(),
        oracle_queue_address,
        TEST_CALLBACK_PROGRAM,
        vrf_input,
        PodRistrettoPoint(output.to_bytes()),
        PodRistrettoPoint(commitment_base_compressed.to_bytes()),
        PodRistrettoPoint(commitment_hash_compressed.to_bytes()),
        PodScalar(s.to_bytes()),
    );
    let compute_ix = ComputeBudgetInstruction::set_compute_unit_limit(2_000_000);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[compute_ix, ix],
        Some(&oracle_keypair.pubkey()),
        &[&oracle_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let mut qdata = oracle_queue_account.data.clone();
    let queue_acc = QueueAccount::load(&mut qdata[8..]).unwrap();
    assert_eq!(oracle_queue_account.owner, ephemeral_vrf_api::ID);
    assert_eq!(queue_acc.len(), 0);
    assert_eq!(
        oracle_queue_account.lamports,
        banks
            .get_rent()
            .await
            .unwrap()
            .minimum_balance(oracle_queue_account.data.len())
    );

    // Add another request, advance slots beyond TTL, then purge expired requests.
    let ix = request_randomness(context.payer.pubkey(), 1);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify request was added to queue (len == 1)
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let oracle_queue = Queue::try_from_bytes(&oracle_queue_account.data).unwrap();
    assert_eq!(oracle_queue.len(), 1);

    // Advance slots beyond TTL to make the request expired
    let current_slot = banks.get_sysvar::<Clock>().await.unwrap().slot;
    context
        .warp_to_slot(current_slot + QUEUE_TTL_SLOTS + 1)
        .unwrap();

    // Purge expired requests
    let purge_ix = purge_expired_requests(oracle_keypair.pubkey(), 0);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[purge_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify queue is empty after purge
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let oracle_queue = Queue::try_from_bytes(&oracle_queue_account.data).unwrap();
    assert_eq!(oracle_queue.len(), 0);

    // Initialize a new oracle queue
    let oracle_queue_address_1 = oracle_queue_pda(&oracle_keypair.pubkey(), 1).0;
    let ixs = initialize_oracle_queue(
        context.payer.pubkey(),
        oracle_keypair.pubkey(),
        1,
        Some(10_000),
    );
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&context.payer.pubkey()),
        &[&context.payer, &oracle_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Delegate oracle queue
    let ix = delegate_oracle_queue(oracle_keypair.pubkey(), oracle_queue_address_1, 1);
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&oracle_keypair.pubkey()),
        &[&oracle_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify delegation was successful by checking the queue account owner
    let oracle_queue_account = banks
        .get_account(oracle_queue_address_1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(oracle_queue_account.owner, DELEGATION_PROGRAM_ID);

    // Add num_requests to the new oracle queue (index 0)
    let num_requests = 10;
    for i in 0..num_requests {
        let ix = request_randomness_to_queue(context.payer.pubkey(), i, oracle_queue_address);
        let blockhash = banks.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&context.payer.pubkey()),
            &[&context.payer],
            blockhash,
        );
        let res = banks.process_transaction(tx).await;
        assert!(res.is_ok());
    }

    // Verify num_requests were added
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let mut qdata = oracle_queue_account.data.clone();
    let queue_acc = QueueAccount::load(&mut qdata[8..]).unwrap();
    assert_eq!(queue_acc.len(), num_requests as usize);

    // Increase the slot
    let current_slot = banks.get_sysvar::<Clock>().await.unwrap().slot;
    context.warp_to_slot(current_slot + 1).unwrap();

    // Consume 10 requests from the queue (index 0)
    for _ in 0..num_requests {
        // Load the current head item
        let oracle_queue_account = banks
            .get_account(oracle_queue_address)
            .await
            .unwrap()
            .unwrap();
        let mut qdata2 = oracle_queue_account.data.clone();
        let queue_acc2 = QueueAccount::load(&mut qdata2[8..]).unwrap();
        let vrf_input = queue_acc2.get_item_by_index(0).unwrap().id;

        // Compute off-chain VRF
        let (output, (commitment_base_compressed, commitment_hash_compressed, s)) =
            compute_vrf(oracle_vrf_sk, &vrf_input);

        // Provide randomness (consume the request)
        let ix = provide_randomness(
            oracle_keypair.pubkey(),
            oracle_queue_address,
            TEST_CALLBACK_PROGRAM,
            vrf_input,
            PodRistrettoPoint(output.to_bytes()),
            PodRistrettoPoint(commitment_base_compressed.to_bytes()),
            PodRistrettoPoint(commitment_hash_compressed.to_bytes()),
            PodScalar(s.to_bytes()),
        );
        let compute_ix = ComputeBudgetInstruction::set_compute_unit_limit(2_000_000);
        let blockhash = banks.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[compute_ix, ix],
            Some(&oracle_keypair.pubkey()),
            &[&oracle_keypair],
            blockhash,
        );
        let res = banks.process_transaction(tx).await;
        assert!(res.is_ok());
    }

    // Verify oracle queue is empty after consuming requests
    let oracle_queue_account = banks
        .get_account(oracle_queue_address)
        .await
        .unwrap()
        .unwrap();
    let mut qdata = oracle_queue_account.data.clone();
    let queue_acc = QueueAccount::load(&mut qdata[8..]).unwrap();
    assert_eq!(queue_acc.len(), 0);

    // Close oracle queue.
    let ix = close_oracle_queue(oracle_keypair.pubkey(), 0);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&oracle_keypair.pubkey()),
        &[&oracle_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify oracle queue was closed
    let oracle_queue_account = banks
        .get_account(oracle_queue_pda(&oracle_keypair.pubkey(), 0).0)
        .await
        .unwrap();
    assert!(oracle_queue_account.is_none());

    // Submit add a new oracle transaction.
    let new_test_oracle = Keypair::new();
    let (_, oracle_vrf_pk) = generate_vrf_keypair(&oracle_keypair);
    let ix = add_oracle(
        authority_keypair.pubkey(),
        new_test_oracle.pubkey(),
        oracle_vrf_pk.compress().to_bytes(),
    );

    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&authority_keypair.pubkey()),
        &[&authority_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify oracle was added.
    let oracles_info = banks.get_account(oracles_address).await.unwrap().unwrap();
    let oracles_data = oracles_info.data;
    let oracles = Oracles::try_from_bytes_with_discriminator(&oracles_data).unwrap();
    assert!(oracles
        .oracles
        .iter()
        .any(|o| o.eq(&new_test_oracle.pubkey())));

    // Submit remove oracle transaction.
    let ix = remove_oracle(authority_keypair.pubkey(), new_test_oracle.pubkey());
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&authority_keypair.pubkey()),
        &[&authority_keypair],
        blockhash,
    );
    let res = banks.process_transaction(tx).await;
    assert!(res.is_ok());

    // Verify oracle was removed.
    let oracles_info = banks.get_account(oracles_address).await.unwrap().unwrap();
    let oracles_data = oracles_info.data;
    let oracles = Oracles::try_from_bytes_with_discriminator(&oracles_data).unwrap();
    assert!(!oracles
        .oracles
        .iter()
        .any(|o| o.eq(&new_test_oracle.pubkey())));
    assert_eq!(
        oracles_info.lamports,
        banks
            .get_rent()
            .await
            .unwrap()
            .minimum_balance(oracles_data.len())
    );
}

pub fn request_randomness(signer: Pubkey, client_seed: u8) -> Instruction {
    // Forward to the generic helper, using the default oracle queue used previously
    let oracle_queue = pubkey!("GKE6d7iv8kCBrsxr78W3xVdjGLLLJnxsGiuzrsZCGEvb");
    request_randomness_to_queue(signer, client_seed, oracle_queue)
}

pub fn request_randomness_to_queue(
    signer: Pubkey,
    client_seed: u8,
    oracle_queue: Pubkey,
) -> Instruction {
    // Same discriminator used by the integration test callback program
    const DISCRIMINATOR: [u8; 8] = [213, 5, 173, 166, 37, 236, 31, 18];

    // Program identity PDA (seeded with "identity")
    let (program_identity, _) = Pubkey::find_program_address(&[IDENTITY], &TEST_CALLBACK_PROGRAM);

    // Construct account metas targeting the provided oracle_queue
    let accounts = vec![
        AccountMeta::new(signer, true),
        AccountMeta::new_readonly(program_identity, false),
        AccountMeta::new(oracle_queue, false),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new_readonly(slot_hashes::ID, false),
        AccountMeta::new_readonly(ephemeral_vrf_api::ID, false),
    ];

    // Instruction data: discriminator + client_seed
    let mut data = DISCRIMINATOR.to_vec();
    data.push(client_seed);

    Instruction {
        program_id: TEST_CALLBACK_PROGRAM,
        accounts,
        data,
    }
}
