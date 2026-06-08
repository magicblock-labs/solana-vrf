use solana_curve25519::ristretto::PodRistrettoPoint;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

/// seed of the oracles account PDA.
pub const ORACLES: &[u8] = b"oracles";

/// seed of the oracle data account PDA.
pub const ORACLE_DATA: &[u8] = b"oracle";

/// Seed of the identity account PDA.
pub const IDENTITY: &[u8] = b"identity";

/// Seed of the queue account PDA.
pub const QUEUE: &[u8] = b"queue";
pub const VRF_PREFIX_CHALLENGE: &[u8] = b"VRF-Ephem-Challenge";
pub const VRF_PREFIX_HASH_TO_POINT: &[u8] = b"VRF-Ephem-v2-HashToRistretto";
pub const VRF_PREFIX_HASH_TO_SCALAR: &[u8] = b"VRF-Ephem-HashToScalar";

pub const VRF_HIGH_PRIORITY_LAMPORTS_COST: u64 = 800000;
pub const VRF_LAMPORTS_COST: u64 = 500000;

// ~2 minutes on Solana (~500ms/slot) ≈ 240 slots. Round to 240.
pub const QUEUE_TTL_SLOTS: u64 = 240;

pub const RISTRETTO_BASEPOINT_POINT: PodRistrettoPoint = PodRistrettoPoint([
    226, 242, 174, 10, 106, 188, 78, 113, 168, 132, 169, 97, 197, 0, 81, 95, 88, 227, 11, 106, 165,
    130, 221, 141, 182, 166, 89, 69, 224, 141, 45, 118,
]);

pub const MAGIC_PROGRAM_ID: Pubkey = pubkey!("Magic11111111111111111111111111111111111111");
pub const MAGIC_CONTEXT_ID: Pubkey = pubkey!("MagicContext1111111111111111111111111111111");

pub const DEFAULT_EPHEMERAL_QUEUE: Pubkey = pubkey!("5hBR571xnXppuCPveTrctfTU7tJLSN94nq7kv7FRK5Tc");
#[cfg(feature = "ephemeral-test-queue")]
pub const DEFAULT_EPHEMERAL_TEST_QUEUE: Pubkey =
    pubkey!("Sc9MJUngNbQXSXGP3F67KvKwVnhaYn6kcioxXNVowYT");
