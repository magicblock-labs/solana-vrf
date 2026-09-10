use crate::state::AccountDiscriminator;
use crate::steel::{Discriminator, Pod, Zeroable};
use solana_curve25519::ristretto::PodRistrettoPoint;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Oracle {
    pub vrf_pubkey: PodRistrettoPoint,
    pub registration_slot: u64,
    pub open_queues: u64,
}

impl Oracle {
    pub fn to_bytes(&self) -> &[u8] {
        bytemuck::bytes_of(self)
    }
}

impl Discriminator for Oracle {
    fn discriminator() -> u8 {
        AccountDiscriminator::Oracle.into()
    }
}
