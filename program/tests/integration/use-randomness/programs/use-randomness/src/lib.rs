#![allow(unexpected_cfgs)]
use crate::instruction::{ConsumeRandomness, ConsumeRandomnessLegacy};
use anchor_lang::prelude::borsh::BorshDeserialize;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::sysvar::slot_hashes;
use ephemeral_rollups_sdk::anchor::{vrf, vrf_callback, VrfProgram};
use ephemeral_rollups_sdk::vrf::consts::IDENTITY;
use ephemeral_rollups_sdk::vrf::instructions::create_request_scoped_randomness_ix;
use ephemeral_rollups_sdk::vrf::instructions::RequestRandomnessParams;
use ephemeral_rollups_sdk::vrf::rnd::{random_bool, random_u32, random_u8_with_range};
use ephemeral_rollups_sdk::vrf::types::SerializableAccountMeta;

declare_id!("Cz9eXYRuhR7fxmhEvYRY5X19qhybJ29wpaervEHHz32s");

#[program]
pub mod use_randomness {
    use super::*;

    // ---------------------------------------------------------------------------------------------
    // Default pattern: scoped per-program identity.
    // Requests use the scoped builder / the `#[vrf]` macro; the callback validates the scoped
    // identity via `#[vrf_callback]`.
    // ---------------------------------------------------------------------------------------------

    pub fn request_randomness(ctx: Context<RequestRandomnessCtx>, client_seed: u8) -> Result<()> {
        msg!(
            "Generating a random number: (from program: {:?})",
            ctx.program_id
        );
        let ix = create_request_scoped_randomness_ix(RequestRandomnessParams {
            payer: ctx.accounts.payer.key(),
            oracle_queue: ctx.accounts.oracle_queue.key(),
            callback_program_id: ID,
            callback_discriminator: ConsumeRandomness::DISCRIMINATOR.to_vec(),
            caller_seed: hash(&[client_seed]).to_bytes(),
            ..Default::default()
        });
        invoke_signed(
            &ix,
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.program_identity.to_account_info(),
                ctx.accounts.oracle_queue.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.slot_hashes.to_account_info(),
            ],
            &[&[IDENTITY, &[ctx.bumps.program_identity]]],
        )?;
        Ok(())
    }

    pub fn simpler_request_randomness(
        ctx: Context<RequestRandomnessSimplerCtx>,
        client_seed: u8,
    ) -> Result<()> {
        msg!("Generating a random number with simpler API");
        // The #[vrf] macro forces a scoped request regardless of the builder used.
        let ix = create_request_scoped_randomness_ix(RequestRandomnessParams {
            payer: ctx.accounts.payer.key(),
            oracle_queue: ctx.accounts.oracle_queue.key(),
            callback_program_id: crate::ID,
            callback_discriminator: ConsumeRandomness::DISCRIMINATOR.to_vec(),
            caller_seed: hash(&[client_seed]).to_bytes(),
            accounts_metas: Some(vec![SerializableAccountMeta {
                pubkey: pubkey!("Fx9JhvMsnJSuDZDKg3dM9wLpznyK7hfY2BP5jwiFbj7q"),
                is_signer: false,
                is_writable: true,
            }]),
            ..Default::default()
        });
        ctx.accounts
            .invoke_signed_vrf(&ctx.accounts.payer.to_account_info(), &ix)?;
        Ok(())
    }

    pub fn cheaper_request_randomness(
        ctx: Context<RequestRandomnessSimplerCtx>,
        client_seed: u8,
    ) -> Result<()> {
        msg!("Generating a random number");
        let ix = create_request_scoped_randomness_ix(RequestRandomnessParams {
            payer: ctx.accounts.payer.key(),
            oracle_queue: ctx.accounts.oracle_queue.key(),
            callback_program_id: crate::ID,
            callback_discriminator: ConsumeRandomness::DISCRIMINATOR.to_vec(),
            caller_seed: hash(&[client_seed]).to_bytes(),
            ..Default::default()
        });
        ctx.accounts
            .invoke_signed_vrf(&ctx.accounts.payer.to_account_info(), &ix)?;
        Ok(())
    }

    pub fn consume_randomness(
        ctx: Context<ConsumeRandomnessCtx>,
        randomness: [u8; 32],
    ) -> Result<()> {
        // Reaching here proves the scoped VRF identity signed the callback (enforced by the
        // `#[vrf_callback]` account constraint), so the randomness is authentic.
        msg!(
            "Scoped VRF identity (signer): {:?}",
            ctx.accounts.vrf_program_identity.key()
        );
        msg!("Consuming random u32: {:?}", random_u32(&randomness));
        msg!(
            "Consuming random u8 (range 1-6): {:?}",
            random_u8_with_range(&randomness, 1, 6)
        );
        msg!("Consuming random bool: {:?}", random_bool(&randomness));
        if !ctx.remaining_accounts.is_empty() {
            msg!(
                "First remaining account: {:?}",
                ctx.remaining_accounts.first()
            );
            assert!(ctx.remaining_accounts.first().unwrap().is_writable);
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------------
    // Legacy (global identity) pattern — DEPRECATED. Kept only to test backward compatibility.
    // ---------------------------------------------------------------------------------------------

    pub fn request_randomness_legacy(
        ctx: Context<RequestRandomnessCtx>,
        client_seed: u8,
    ) -> Result<()> {
        msg!("Generating a random number (LEGACY global-identity path)");
        #[allow(deprecated)]
        let ix = ephemeral_rollups_sdk::vrf::instructions::create_request_randomness_ix(
            RequestRandomnessParams {
                payer: ctx.accounts.payer.key(),
                oracle_queue: ctx.accounts.oracle_queue.key(),
                callback_program_id: ID,
                callback_discriminator: ConsumeRandomnessLegacy::DISCRIMINATOR.to_vec(),
                caller_seed: hash(&[client_seed]).to_bytes(),
                ..Default::default()
            },
        );
        invoke_signed(
            &ix,
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.program_identity.to_account_info(),
                ctx.accounts.oracle_queue.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.slot_hashes.to_account_info(),
            ],
            &[&[IDENTITY, &[ctx.bumps.program_identity]]],
        )?;
        Ok(())
    }

    pub fn consume_randomness_legacy(
        ctx: Context<ConsumeRandomnessLegacyCtx>,
        randomness: [u8; 32],
    ) -> Result<()> {
        msg!(
            "LEGACY global VRF identity (signer): {:?}",
            ctx.accounts.vrf_program_identity.key()
        );
        msg!("Consuming random u32: {:?}", random_u32(&randomness));
        Ok(())
    }
}

#[derive(Accounts)]
pub struct RequestRandomnessCtx<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: Used to verify the identity of the program
    #[account(seeds = [b"identity"], bump)]
    pub program_identity: AccountInfo<'info>,
    /// CHECK: Oracle queue
    #[account(mut, address = DEFAULT_TEST_QUEUE)]
    pub oracle_queue: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: Slot hashes sysvar
    #[account(address = slot_hashes::ID)]
    pub slot_hashes: AccountInfo<'info>,
    pub vrf_program: Program<'info, VrfProgram>,
}

#[vrf]
#[derive(Accounts)]
pub struct RequestRandomnessSimplerCtx<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: The oracle queue
    #[account(mut, address = DEFAULT_TEST_QUEUE)]
    pub oracle_queue: AccountInfo<'info>,
}

/// Default callback context: `#[vrf_callback]` injects `vrf_program_identity` bound to the
/// scoped per-program identity PDA.
#[vrf_callback]
#[derive(Accounts)]
pub struct ConsumeRandomnessCtx<'info> {}

/// Legacy callback context (DEPRECATED): validates the global identity. Kept only for
/// backward-compatibility tests.
#[derive(Accounts)]
pub struct ConsumeRandomnessLegacyCtx<'info> {
    #[account(address = ephemeral_rollups_sdk::vrf::consts::VRF_PROGRAM_IDENTITY)]
    pub vrf_program_identity: Signer<'info>,
}

pub const DEFAULT_TEST_QUEUE: Pubkey = pubkey!("GKE6d7iv8kCBrsxr78W3xVdjGLLLJnxsGiuzrsZCGEvb");
pub const DEFAULT_EPHEMERAL_TEST_QUEUE: Pubkey =
    pubkey!("Sc9MJUngNbQXSXGP3F67KvKwVnhaYn6kcioxXNVowYT");
