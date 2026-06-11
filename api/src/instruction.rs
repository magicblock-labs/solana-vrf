use crate::prelude::SerializableAccountMeta;
use crate::steel::*;
use borsh::{BorshDeserialize, BorshSerialize};
use solana_curve25519::ristretto::PodRistrettoPoint;
use solana_curve25519::scalar::PodScalar;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromPrimitive)]
pub enum EphemeralVrfInstruction {
    Initialize = 0,
    ModifyOracle = 1,
    InitializeOracleQueue = 2,
    RequestHighPriorityRandomness = 3,
    ProvideRandomness = 4,
    DelegateOracleQueue = 5,
    UndelegateOracleQueue = 6,
    ProcessUndelegation = 196,
    CloseOracleQueue = 7,
    RequestRandomness = 8,
    PurgeExpiredRequests = 9,
    RequestRandomnessScoped = 10,
    RequestHighPriorityRandomnessScoped = 11,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Initialize {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ModifyOracle {
    pub identity: Pubkey,
    pub oracle_pubkey: PodRistrettoPoint,
    pub operation: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct InitializeOracleQueue {
    pub target_size: u32,
    pub index: u8,
    pub _padding: [u8; 3],
}

impl InitializeOracleQueue {
    pub(crate) fn new(index: u8, target_size: u32) -> Self {
        Self {
            target_size,
            index,
            _padding: [0; 3],
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq, Default)]
pub struct RequestRandomness {
    pub caller_seed: [u8; 32],
    pub callback_program_id: Pubkey,
    pub callback_discriminator: Vec<u8>,
    pub callback_accounts_metas: Vec<SerializableAccountMeta>,
    pub callback_args: Vec<u8>,
}

pub struct PdaSeeds;
impl PdaSeeds {
    pub fn parse(data: &[u8]) -> Result<Vec<Vec<u8>>, ProgramError> {
        Vec::<Vec<u8>>::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ProvideRandomness {
    pub input: [u8; 32],
    pub output: PodRistrettoPoint,
    pub commitment_base_compressed: PodRistrettoPoint,
    pub commitment_hash_compressed: PodRistrettoPoint,
    pub scalar: PodScalar,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct DelegateOracleQueue {
    pub index: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct UndelegateOracleQueue {
    pub index: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct CloseOracleQueue {
    pub index: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct PurgeExpiredRequests {
    pub index: u8,
}

instruction8!(EphemeralVrfInstruction, Initialize);
instruction8!(EphemeralVrfInstruction, ModifyOracle);
instruction8!(EphemeralVrfInstruction, InitializeOracleQueue);
instruction8!(EphemeralVrfInstruction, ProvideRandomness);
instruction8!(EphemeralVrfInstruction, DelegateOracleQueue);
instruction8!(EphemeralVrfInstruction, UndelegateOracleQueue);
instruction8!(EphemeralVrfInstruction, CloseOracleQueue);
instruction8!(EphemeralVrfInstruction, PurgeExpiredRequests);

impl RequestRandomness {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![
            EphemeralVrfInstruction::RequestHighPriorityRandomness as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        self.serialize(&mut bytes).unwrap();
        bytes
    }

    pub fn try_from_bytes(mut bytes: &[u8]) -> Result<Self, std::io::Error> {
        Self::deserialize(&mut bytes)
    }
}
