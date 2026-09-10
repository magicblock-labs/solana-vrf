pub mod accounts;

#[allow(unused_imports)]
pub(crate) use accounts::*;
use solana_program::pubkey;
use solana_vrf_api::steel::Pubkey;

pub(crate) const TEST_CALLBACK_PROGRAM: Pubkey =
    pubkey!("Cz9eXYRuhR7fxmhEvYRY5X19qhybJ29wpaervEHHz32s");
