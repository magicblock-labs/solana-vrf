use std::{pin::Pin, str::FromStr, sync::Arc};

use async_trait::async_trait;
use crossbeam_channel::{select, Receiver};
use futures_util::StreamExt;
use solana_client::{
    pubsub_client::{PubsubProgramClientSubscription, PubsubSlotClientSubscription},
    rpc_response::{RpcKeyedAccount, SlotInfo},
};
use solana_sdk::pubkey::Pubkey;

use crate::blockhash_cache::BlockhashCache;
use crate::oracle::client::{QueueUpdateSource, SlotTracker};
use ephemeral_vrf_api::prelude::Queue;
use ephemeral_vrf_api::ID as PROGRAM_ID;
use helius_laserstream::{
    grpc::{subscribe_update::UpdateOneof, SlotStatus, SubscribeUpdate},
    LaserstreamError,
};

pub struct WebSocketSource {
    pub subscription: Receiver<solana_client::rpc_response::Response<RpcKeyedAccount>>,
    pub client: PubsubProgramClientSubscription,
    pub slot_subscription: Receiver<SlotInfo>,
    pub slot_client: PubsubSlotClientSubscription,
    pub slot_tracker: SlotTracker,
}

impl Drop for WebSocketSource {
    fn drop(&mut self) {
        let _ = self.client.shutdown();
        let _ = self.slot_client.shutdown();
    }
}

#[async_trait]
impl QueueUpdateSource for WebSocketSource {
    async fn next(&mut self) -> Option<(Pubkey, Queue, Vec<u8>, u64)> {
        loop {
            select! {
                recv(self.subscription) -> update => {
                    let update = update.ok()?;
                    self.slot_tracker.update(update.context.slot);
                    let data = update.value.account.data.decode()?;
                    if update.value.account.owner != PROGRAM_ID.to_string() {
                        return None;
                    }
                    let queue = Queue::try_from_bytes(&data).ok()?;
                    let pubkey = Pubkey::from_str(&update.value.pubkey).ok()?;
                    return Some((pubkey, *queue, data, update.context.slot));
                }
                recv(self.slot_subscription) -> update => {
                    self.slot_tracker.update(update.ok()?.slot);
                }
            }
        }
    }
}

pub struct LaserstreamSource {
    pub stream:
        Pin<Box<dyn futures_core::Stream<Item = Result<SubscribeUpdate, LaserstreamError>> + Send>>,
    pub blockhash_cache: Arc<BlockhashCache>,
    pub slot_tracker: SlotTracker,
}

#[async_trait]
impl QueueUpdateSource for LaserstreamSource {
    async fn next(&mut self) -> Option<(Pubkey, Queue, Vec<u8>, u64)> {
        while let Some(result) = self.stream.next().await {
            let update = result.ok()?;
            match update.update_oneof {
                Some(UpdateOneof::Account(acc)) => {
                    let slot = acc.slot;
                    self.slot_tracker.update(slot);
                    let acc = acc.account?;
                    let queue = Queue::try_from_bytes(&acc.data).ok()?;
                    let pubkey = Pubkey::new_from_array(acc.pubkey.try_into().ok()?);
                    return Some((pubkey, *queue, acc.data, slot));
                }
                Some(UpdateOneof::BlockMeta(meta)) => {
                    if let Ok(hash) = meta.blockhash.parse() {
                        self.blockhash_cache.set_blockhash(hash, meta.slot).await;
                    }
                }
                Some(UpdateOneof::Slot(slot)) => {
                    if matches!(
                        SlotStatus::try_from(slot.status),
                        Ok(SlotStatus::SlotProcessed
                            | SlotStatus::SlotConfirmed
                            | SlotStatus::SlotFinalized
                            | SlotStatus::SlotCompleted
                            | SlotStatus::SlotCreatedBank)
                    ) {
                        self.slot_tracker.update(slot.slot);
                    }
                }
                _ => {}
            }
        }
        None
    }
}
