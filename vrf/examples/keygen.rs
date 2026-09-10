use solana_sdk::hash::hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_vrf::vrf::{compute_vrf, generate_vrf_keypair, verify_vrf};

fn main() {
    let keypair = Keypair::new();
    let (sk, pk) = generate_vrf_keypair(&keypair);
    let bs58_pk = Pubkey::new_from_array(pk.compress().to_bytes());
    print!("Generated PK: {bs58_pk:?}");

    let blockhash = b"blockhash";
    let seed = b"test-seed";
    let input: Vec<u8> = blockhash.iter().chain(seed.iter()).cloned().collect();
    let input_hash = hash(&input).to_bytes();
    let (output, (commitment_base_compressed, commitment_hash_compressed, s)) =
        compute_vrf(sk, &input_hash);

    let is_valid = verify_vrf(
        pk,
        &input_hash,
        output,
        (commitment_base_compressed, commitment_hash_compressed, s),
    );
    print!("\nVRF proof is valid: {is_valid:?}");
}
