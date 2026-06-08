use crate::consts::{
    VRF_PREFIX_CHALLENGE, VRF_PREFIX_HASH_TO_POINT, VRF_PREFIX_HASH_TO_SCALAR, VRF_PREFIX_NONCE,
};
use curve25519_dalek::constants::{RISTRETTO_BASEPOINT_POINT, RISTRETTO_BASEPOINT_TABLE};
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use hkdf::Hkdf;
use sha2::Sha512;
use solana_sdk::hash::{hash, hashv};
use solana_sdk::signature::Keypair;

// Key Generation (done once by the oracle)
pub fn generate_vrf_keypair(keypair: &Keypair) -> (Scalar, RistrettoPoint) {
    let hkdf = Hkdf::<Sha512>::new(Some(b"VRF-Solana-SecretKey"), &keypair.to_bytes());
    let mut okm = [0u8; 64];
    hkdf.expand(b"VRF-Key", &mut okm)
        .expect("HKDF expansion failed");
    let sk = Scalar::from_bytes_mod_order(okm[..32].try_into().unwrap());
    let pk = &sk * RISTRETTO_BASEPOINT_TABLE;
    (sk, pk)
}

const HASH_TO_POINT_MAX_ATTEMPTS: u16 = 512;

// Hash-to-Point with an unknown discrete log relative to the basepoint.
fn hash_to_point(input: &[u8]) -> RistrettoPoint {
    for counter in 0..HASH_TO_POINT_MAX_ATTEMPTS {
        let counter_bytes = counter.to_le_bytes();
        let candidate = hashv(&[VRF_PREFIX_HASH_TO_POINT, &counter_bytes, input]);
        if let Some(point) = CompressedRistretto(candidate.to_bytes()).decompress() {
            return point;
        }
    }
    panic!("Failed to hash input to a Ristretto point");
}

// Hash-to-Scalar using built-in hash_to_scalar function, plus domain separation
fn hash_to_scalar(input: &[u8; 32]) -> Scalar {
    let hashed_input = hash(
        [VRF_PREFIX_HASH_TO_SCALAR.to_vec(), input.to_vec()]
            .concat()
            .as_slice(),
    );
    Scalar::from_bytes_mod_order(hashed_input.to_bytes())
}

// VRF computation
pub fn compute_vrf(
    sk: Scalar,
    input: &[u8; 32],
) -> (
    CompressedRistretto,
    (CompressedRistretto, CompressedRistretto, Scalar),
) {
    // Hash the input
    let h = hash_to_point(input);
    // VRF output = sk·h
    let vrf_output = sk * h;
    // Public key = sk·G
    let pk = &sk * RISTRETTO_BASEPOINT_TABLE;

    // RFC 9381 Nonce generation with domain separation and secure key derivation
    // Use HKDF to derive the nonce from the secret key and input
    let salt = VRF_PREFIX_NONCE;
    let ikm = [&sk.to_bytes()[..], input].concat();
    let hkdf = Hkdf::<Sha512>::new(Some(salt), &ikm);
    let mut okm = [0u8; 64];
    hkdf.expand(b"VRF-Nonce", &mut okm)
        .expect("HKDF expansion failed");
    let k = Scalar::from_bytes_mod_order(
        okm[..32]
            .try_into()
            .expect("Failed to convert HKDF output to scalar - invalid 32-byte slice"),
    );

    // Commitments: one for basepoint G, one for hashed point h
    let commitment_base = k * RISTRETTO_BASEPOINT_POINT;
    let commitment_hash = k * h;

    // Compute Challenge (domain-tagged)
    let challenge_input = [
        VRF_PREFIX_CHALLENGE.to_vec(),
        vrf_output.compress().to_bytes().to_vec(),
        commitment_base.compress().to_bytes().to_vec(),
        commitment_hash.compress().to_bytes().to_vec(),
        pk.compress().to_bytes().to_vec(),
        input.to_vec(),
    ]
    .concat();

    let challenge_hash = hash(challenge_input.as_slice());
    let c = hash_to_scalar(&challenge_hash.to_bytes());

    // Response
    let s = k + c * sk;

    (
        vrf_output.compress(),
        (commitment_base.compress(), commitment_hash.compress(), s),
    )
}

// Verify VRF Proof
pub fn verify_vrf(
    pk: RistrettoPoint,
    input: &[u8; 32],
    output_compressed: CompressedRistretto,
    proof: (CompressedRistretto, CompressedRistretto, Scalar),
) -> bool {
    let (commitment_base_compressed, commitment_hash_compressed, s) = proof;

    let output = match output_compressed.decompress() {
        Some(p) => p,
        None => return false,
    };
    let commitment_base = match commitment_base_compressed.decompress() {
        Some(p) => p,
        None => return false,
    };
    let commitment_hash = match commitment_hash_compressed.decompress() {
        Some(p) => p,
        None => return false,
    };

    // Recompute h (with domain separation)
    let h = hash_to_point(input);

    // Recompute challenge
    let challenge_input = [
        VRF_PREFIX_CHALLENGE.to_vec(),
        output_compressed.to_bytes().to_vec(),
        commitment_base_compressed.to_bytes().to_vec(),
        commitment_hash_compressed.to_bytes().to_vec(),
        pk.compress().to_bytes().to_vec(),
        input.to_vec(),
    ]
    .concat();
    let challenge_hash = hash(challenge_input.as_slice());
    let c = hash_to_scalar(&challenge_hash.to_bytes());

    // ---------------------------
    // 1) Schnorr check for G:
    // s·G == commitment_base + c·pk
    // ---------------------------
    let lhs_base = &s * RISTRETTO_BASEPOINT_TABLE;
    let rhs_base = commitment_base + c * pk;

    // ---------------------------
    // 2) Schnorr-like check for h:
    // s·h == commitment_hash + c·(sk·h)
    // But sk·h = output
    // => s·h == commitment_hash + c·output
    // ---------------------------
    let lhs_hash = s * h;
    let rhs_hash = commitment_hash + c * output;

    lhs_base == rhs_base && lhs_hash == rhs_hash
}
