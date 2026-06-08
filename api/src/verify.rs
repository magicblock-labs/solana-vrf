use crate::consts::VRF_PREFIX_HASH_TO_SCALAR;
use crate::prelude::*;
use curve25519_dalek::Scalar;
use solana_curve25519::edwards::{validate_edwards, PodEdwardsPoint};
use solana_curve25519::ristretto::{
    add_ristretto, multiply_ristretto, validate_ristretto, PodRistrettoPoint,
};
use solana_curve25519::scalar::PodScalar;
use solana_program::hash::{hash, hashv};
use solana_program::pubkey::Pubkey;

const HASH_TO_POINT_MAX_ATTEMPTS: u16 = 512;

/// Verify a VRF proof
///
/// Accounts: None
///
/// Requirements:
///
/// - Proof must be valid for the given public key, input, and output
///
/// 1. Recompute the hash point from input
/// 2. Recompute the challenge scalar
/// 3. Verify the Schnorr proof for the base point
/// 4. Verify the Schnorr-like proof for the hash point
pub fn verify_vrf(
    pk: &PodRistrettoPoint,
    input: &[u8; 32],
    output_compressed: &PodRistrettoPoint,
    proof: (&PodRistrettoPoint, &PodRistrettoPoint, &PodScalar),
) -> bool {
    let (commitment_base_compressed, commitment_hash_compressed, s) = proof;

    // Recompute h
    let h = match hash_to_point(input) {
        Some(h) => h,
        None => return false,
    };

    // Recompute challenge
    let challenge_input = [
        VRF_PREFIX_CHALLENGE.to_vec(),
        output_compressed.0.to_vec(),
        commitment_base_compressed.0.to_vec(),
        commitment_hash_compressed.0.to_vec(),
        pk.0.to_vec(),
        input.to_vec(),
    ]
    .concat();

    let challenge_hash = hash(challenge_input.as_slice());
    let c = hash_to_scalar(&challenge_hash.to_bytes());

    // ---------------------------
    // 1) Schnorr check for G:
    // s·G == commitment_base + c·pk
    // ---------------------------
    let lhs_base = match multiply_ristretto(s, &RISTRETTO_BASEPOINT_POINT) {
        Some(result) => result,
        None => return false,
    };

    let rhs_base_r = match multiply_ristretto(&c, pk) {
        Some(result) => result,
        None => return false,
    };

    let rhs_base = match add_ristretto(commitment_base_compressed, &rhs_base_r) {
        Some(result) => result,
        None => return false,
    };

    // 2) Schnorr-like check for h:
    // s·h == commitment_hash + c·(sk·h)
    // But sk·h = output
    // => s·h == commitment_hash + c·output
    let lhs_hash = match multiply_ristretto(s, &h) {
        Some(result) => result,
        None => return false,
    };

    let rhs_hash_r = match multiply_ristretto(&c, output_compressed) {
        Some(result) => result,
        None => return false,
    };

    let rhs_hash = match add_ristretto(commitment_hash_compressed, &rhs_hash_r) {
        Some(result) => result,
        None => return false,
    };

    lhs_base == rhs_base && lhs_hash == rhs_hash
}

/// Hash the input to a Ristretto point with an unknown discrete log relative to the basepoint.
///
/// Accounts: None
///
/// Requirements: None
///
/// 1. Hash the input with the VRF prefix
fn hash_to_point(input: &[u8]) -> Option<PodRistrettoPoint> {
    for counter in 0..HASH_TO_POINT_MAX_ATTEMPTS {
        let counter_bytes = counter.to_le_bytes();
        let candidate =
            PodRistrettoPoint(hashv(&[VRF_PREFIX_HASH_TO_POINT, &counter_bytes, input]).to_bytes());
        if validate_ristretto(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Convert the input to a scalar using the modulus order of the curve
///
/// Accounts: None
///
/// Requirements: None
///
/// 1. Hash the input with the VRF prefix
/// 2. Convert the hash to a scalar using the curve's modulus
fn hash_to_scalar(input: &[u8; 32]) -> PodScalar {
    let hashed_input = hash(
        [VRF_PREFIX_HASH_TO_SCALAR.to_vec(), input.to_vec()]
            .concat()
            .as_slice(),
    );
    PodScalar(Scalar::from_bytes_mod_order(hashed_input.to_bytes()).to_bytes())
}

pub fn is_on_curve(key: &Pubkey) -> bool {
    validate_edwards(&PodEdwardsPoint(key.to_bytes()))
}
