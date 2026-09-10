use crate::prelude::{AccountDiscriminator, SolanaVrfError};
use crate::steel::{AccountMeta, Pod, ProgramError, Pubkey, Zeroable};
use borsh::{BorshDeserialize, BorshSerialize};
use core::mem::{size_of, size_of_val};

const MAX_CALLBACK_ACCOUNTS: usize = 25;

/// Header of the queue account (fixed size, lives at the start of the account
/// after the 8-byte discriminator).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Zeroable, Pod)]
pub struct Queue {
    /// Number of active (used) items.
    pub item_count: u32,
    /// Cursor in bytes from the start of the account data (after discriminator)
    /// pointing to the next free byte in the variable region.
    pub cursor: u32,
    /// Logical index or shard id of the queue.
    pub index: u8,
    /// 1 = paused: the oracle has stopped accepting new requests so the queue
    /// can be drained and closed. 0 = active. Reuses former padding, so
    /// existing (zeroed) accounts read as active.
    pub paused: u8,
    pub _padding: [u8; 2],
}

/// Single queue entry. This is written into the variable region and
/// references its own metas/args by byte offsets.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Zeroable, Pod, PartialEq)]
pub struct QueueItem {
    pub slot: u64,
    pub id: [u8; 32],
    pub callback_program_id: [u8; 32],
    pub callback_discriminator_offset: u32,
    pub metas_offset: u32,
    pub args_offset: u32,
    pub callback_discriminator_len: u16,
    pub metas_len: u16, // number of SerializableAccountMeta
    pub args_len: u16,  // number of bytes
    pub priority_request: u8,
    pub used: u8,          // Flag: 1 = used, 0 = free (logically removed)
    pub identity_mode: u8, // 0 = legacy global identity, 1 = scoped per-callback identity
    pub identity_bump: u8, // bump for the scoped identity PDA (valid when identity_mode == 1)
    pub _padding: [u8; 2],
}

impl QueueItem {
    pub fn callback_discriminator<'a>(&self, acc: &'a [u8]) -> &'a [u8] {
        let start = self.callback_discriminator_offset as usize;
        let end = start + self.callback_discriminator_len as usize;
        &acc[start..end]
    }

    pub fn account_metas<'a>(&self, acc: &'a [u8]) -> &'a [CompactAccountMeta] {
        let start = self.metas_offset as usize;
        let count = self.metas_len as usize;
        let byte_len = count * size_of::<CompactAccountMeta>();
        let end = start + byte_len;

        let bytes = &acc[start..end];

        bytemuck::cast_slice(bytes)
    }

    pub fn callback_args<'a>(&self, acc: &'a [u8]) -> &'a [u8] {
        let start = self.args_offset as usize;
        let end = start + self.args_len as usize;
        &acc[start..end]
    }
}

/// Serializable meta, Borsh compatible and Pod/Zeroable for zero copy.
#[repr(C)]
#[derive(Clone, Copy, Default, Zeroable, Pod, PartialEq)]
pub struct CompactAccountMeta {
    pub pubkey: [u8; 32],
    pub is_writable: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, BorshDeserialize, BorshSerialize)]
pub struct SerializableAccountMeta {
    pub pubkey: [u8; 32],
    pub is_signer: bool,
    pub is_writable: bool,
}

impl From<SerializableAccountMeta> for CompactAccountMeta {
    fn from(val: SerializableAccountMeta) -> Self {
        CompactAccountMeta {
            pubkey: val.pubkey,
            is_writable: val.is_writable as u8,
        }
    }
}

impl CompactAccountMeta {
    pub fn to_account_meta(&self) -> AccountMeta {
        let pubkey = Pubkey::new_from_array(self.pubkey);
        let is_signer = false;
        let is_writable = self.is_writable != 0;

        AccountMeta {
            pubkey,
            is_signer,
            is_writable,
        }
    }
}

/// View over a queue account: header + variable region in the same account data.
pub struct QueueAccount<'a> {
    /// Header, mapped on the first bytes after discriminator.
    pub header: &'a mut Queue,
    /// Full account data including header and variable data.
    pub acc: &'a mut [u8],
}

#[derive(Clone, Copy)]
struct ReusableSpan {
    item_pos: usize,
    logical_index: usize,
}

#[derive(Clone, Copy)]
struct QueueScan {
    last_used_end_aligned: usize,
    reusable_span: Option<ReusableSpan>,
}

#[derive(Clone, Copy)]
struct QueueItemLayout {
    total_needed: usize,
    required_span: usize,
}

impl<'a> QueueAccount<'a> {
    #[inline]
    fn align_up(x: usize) -> usize {
        let align = core::mem::align_of::<QueueItem>();
        (x + align - 1) & !(align - 1)
    }

    #[inline]
    fn items_start() -> usize {
        Self::align_up(size_of::<Queue>())
    }

    /// Load from full account data, including the 8-byte discriminator.
    /// The discriminator is validated and skipped here so call sites pass the
    /// account data as-is and cannot mis-slice the offset. Validating it also
    /// makes a caller that mistakenly passes already-stripped data (starting at
    /// the header) fail loudly instead of silently reading the wrong offset.
    pub fn load(acc: &'a mut [u8]) -> Result<Self, ProgramError> {
        if acc.len() < 8 {
            return Err(ProgramError::InvalidAccountData);
        }
        if AccountDiscriminator::Queue.to_bytes() != acc[..8] {
            return Err(ProgramError::InvalidAccountData);
        }
        // Skip the 8-byte account discriminator.
        let (_discriminator, body) = acc.split_at_mut(8);

        let header_size = size_of::<Queue>();
        if body.len() < header_size {
            return Err(ProgramError::InvalidAccountData);
        }

        let (header_bytes, _rest) = body.split_at_mut(header_size);
        // Validate alignment and size using a safe checked conversion first
        if bytemuck::try_from_bytes_mut::<Queue>(header_bytes).is_err() {
            return Err(ProgramError::InvalidAccountData);
        }
        // Then form the header reference from the raw pointer to avoid lifetime conflicts
        let header: &mut Queue = unsafe { &mut *(header_bytes.as_mut_ptr() as *mut Queue) };

        // If this is a freshly created account, cursor 0 means "no data yet":
        if header.cursor == 0 {
            header.cursor = Self::items_start() as u32;
        }

        Ok(Self { header, acc: body })
    }

    #[inline]
    fn read_item_unaligned(bytes: &[u8]) -> QueueItem {
        bytemuck::pod_read_unaligned(bytes)
    }

    #[inline]
    fn write_item_unaligned(dst: &mut [u8], item: &QueueItem) {
        dst.copy_from_slice(bytemuck::bytes_of(item));
    }

    #[inline]
    fn item_next(cursor: usize, item: &QueueItem) -> usize {
        let metas_bytes = (item.metas_len as usize) * size_of::<CompactAccountMeta>();
        let item_end = cursor
            + size_of::<QueueItem>()
            + (item.callback_discriminator_len as usize)
            + metas_bytes
            + (item.args_len as usize);
        Self::align_up(item_end)
    }

    /// Walk the queue items once, calling `visit(logical_index, item_pos, next, item)`
    /// for each item, where `logical_index` is the count of used items seen so far,
    /// `item_pos` is the item's byte offset and `next` is the byte offset of the
    /// following item. Returns early with the first `Some(_)` a visit produces.
    ///
    /// Centralizes the raw byte-offset / unaligned-read / alignment traversal that
    /// every queue scan needs, so the delicate logic lives in exactly one place.
    fn scan_items<R>(
        &self,
        mut visit: impl FnMut(usize, usize, usize, &QueueItem) -> Option<R>,
    ) -> Option<R> {
        let mut used_index = 0usize;
        let mut cursor = Self::items_start();
        let end = core::cmp::min(self.acc.len(), self.header.cursor as usize);
        let item_size = size_of::<QueueItem>();

        while cursor + item_size <= end {
            let bytes = &self.acc[cursor..cursor + item_size];
            let item = Self::read_item_unaligned(bytes);
            let next = Self::item_next(cursor, &item);

            if let Some(result) = visit(used_index, cursor, next, &item) {
                return Some(result);
            }
            if item.used == 1 {
                used_index += 1;
            }

            cursor = next;
        }

        None
    }

    fn scan_for_reusable_span(&self, required_span: usize) -> QueueScan {
        let mut last_used_end_aligned = Self::items_start();
        let mut reusable_span = None;

        self.scan_items(|logical_index, item_pos, next, item| {
            if item.used == 1 {
                last_used_end_aligned = next;
            } else if reusable_span.is_none() && next - item_pos == required_span {
                reusable_span = Some(ReusableSpan {
                    item_pos,
                    logical_index,
                });
            }
            None::<()>
        });

        QueueScan {
            last_used_end_aligned,
            reusable_span,
        }
    }

    fn write_item_at(
        &mut self,
        item_pos: usize,
        base_item: &QueueItem,
        discriminator: &[u8],
        metas: &[CompactAccountMeta],
        args: &[u8],
    ) -> Result<usize, ProgramError> {
        let item_size = size_of::<QueueItem>();
        let disc_off = item_pos + item_size;
        let metas_off = disc_off + discriminator.len();
        let metas_bytes_len = size_of_val(metas);
        let args_off = metas_off + metas_bytes_len;
        let args_end = args_off + args.len();

        if args_end > self.acc.len() {
            return Err(ProgramError::AccountDataTooSmall);
        }

        self.acc[disc_off..metas_off].copy_from_slice(discriminator);

        let metas_bytes: &[u8] = bytemuck::cast_slice(metas);
        self.acc[metas_off..args_off].copy_from_slice(metas_bytes);
        self.acc[args_off..args_end].copy_from_slice(args);

        let mut item = *base_item;
        item.callback_discriminator_offset = disc_off as u32;
        item.callback_discriminator_len = discriminator.len() as u16;
        item.metas_offset = metas_off as u32;
        item.metas_len = metas.len() as u16;
        item.args_offset = args_off as u32;
        item.args_len = args.len() as u16;
        item.used = 1;

        let dst = &mut self.acc[item_pos..item_pos + item_size];
        Self::write_item_unaligned(dst, &item);

        Ok(args_end)
    }

    fn item_layout(
        discriminator: &[u8],
        metas: &[CompactAccountMeta],
        args: &[u8],
    ) -> Result<QueueItemLayout, ProgramError> {
        if metas.len() > MAX_CALLBACK_ACCOUNTS || args.len() > 512 {
            return Err(ProgramError::from(SolanaVrfError::ArgumentSizeTooLarge));
        }

        let total_needed = size_of::<QueueItem>()
            .saturating_add(discriminator.len())
            .saturating_add(size_of_val(metas))
            .saturating_add(args.len());

        Ok(QueueItemLayout {
            total_needed,
            required_span: Self::align_up(total_needed),
        })
    }

    /// Recompute the end of the last used item and shrink the cursor to it,
    /// effectively removing all trailing holes. If no items are used, reset to items_start().
    fn trim_trailing_holes(&mut self) {
        // Default to empty queue start; if we see used items we’ll update this.
        let mut last_used_end_aligned = Self::items_start();

        self.scan_items(|_, _, next, item| {
            if item.used == 1 {
                last_used_end_aligned = next;
            }
            None::<()>
        });

        // If nothing was used, this stays items_start(); otherwise end of last used.
        if (last_used_end_aligned as u32) < self.header.cursor {
            self.header.cursor = last_used_end_aligned as u32;
        }
    }

    /// Append a new item to the queue with a fixed id.
    pub fn add_item(
        &mut self,
        base_item: &QueueItem,
        discriminator: &[u8],
        metas: &[CompactAccountMeta],
        args: &[u8],
    ) -> Result<usize, ProgramError> {
        let id = base_item.id;
        self.add_item_with_id(base_item, discriminator, metas, args, move |_pos| id)
    }

    /// Append a new item, deriving its id from the byte position it is written to.
    ///
    /// `id_from_pos(item_pos)` is invoked exactly once, with the final insertion
    /// offset, and its result becomes the item's `id`. This lets a caller bind the
    /// id to the insertion position in a single scan, instead of computing the
    /// position up front (a first scan) and then writing the item (a second scan).
    pub fn add_item_with_id(
        &mut self,
        base_item: &QueueItem,
        discriminator: &[u8],
        metas: &[CompactAccountMeta],
        args: &[u8],
        mut id_from_pos: impl FnMut(u32) -> [u8; 32],
    ) -> Result<usize, ProgramError> {
        let layout = Self::item_layout(discriminator, metas, args)?;
        let scan = self.scan_for_reusable_span(layout.required_span);

        if (scan.last_used_end_aligned as u32) < self.header.cursor {
            self.header.cursor = scan.last_used_end_aligned as u32;
        }

        if let Some(span) = scan.reusable_span {
            if span.item_pos < self.header.cursor as usize {
                let mut item = *base_item;
                item.id = id_from_pos(span.item_pos as u32);
                self.write_item_at(span.item_pos, &item, discriminator, metas, args)?;
                self.header.item_count = self.header.item_count.saturating_add(1);
                return Ok(span.logical_index);
            }
        }

        // `aligned` is where the item will start (cursor may have been advanced
        // already). Ensure we have enough room before mutating any state.
        let aligned = Self::align_up(self.header.cursor as usize);
        if aligned.saturating_add(layout.total_needed) > self.acc.len() {
            return Err(ProgramError::AccountDataTooSmall);
        }

        // Ensure items area starts at the aligned offset.
        if aligned != self.header.cursor as usize {
            let start = self.header.cursor as usize;
            let end = aligned;
            // Safe due to preflight check above
            self.acc[start..end].fill(0);
            self.header.cursor = end as u32;
        }

        // Reserve space for the item so items are contiguous
        let item_pos = self.header.cursor as usize;
        let mut item = *base_item;
        item.id = id_from_pos(item_pos as u32);
        let end = self.write_item_at(item_pos, &item, discriminator, metas, args)?;
        self.header.cursor = end as u32;

        // Item index is logical position among used items.
        let logical_index = self.header.item_count as usize;
        self.header.item_count = self.header.item_count.saturating_add(1);
        Ok(logical_index)
    }

    /// Iterate over all used items.
    pub fn iter_items(&self) -> impl Iterator<Item = QueueItem> + '_ {
        let mut out = Vec::new();
        self.scan_items(|_, _, _, item| {
            if item.used == 1 {
                out.push(*item);
            }
            None::<()>
        });
        out.into_iter()
    }

    /// Find the nth used item (logical index) and return its value.
    pub fn get_item_by_index(&self, index: usize) -> Option<QueueItem> {
        self.scan_items(|logical_index, _, _, item| {
            (item.used == 1 && logical_index == index).then_some(*item)
        })
    }

    /// Remove the nth used item (logical index).
    pub fn remove_item(&mut self, index: usize) -> Result<QueueItem, ProgramError> {
        let (item_pos, mut item) = self
            .scan_items(|logical_index, item_pos, _, item| {
                (item.used == 1 && logical_index == index).then_some((item_pos, *item))
            })
            .ok_or::<ProgramError>(SolanaVrfError::InvalidQueueIndex.into())?;

        // Logically remove: clear the used flag in place and trim trailing holes.
        item.used = 0;
        self.header.item_count = self.header.item_count.saturating_sub(1);
        let bytes = &mut self.acc[item_pos..item_pos + size_of::<QueueItem>()];
        Self::write_item_unaligned(bytes, &item);
        self.trim_trailing_holes();

        Ok(item)
    }

    /// Remove in a single pass all used items matching `pred`, invoking
    /// `on_removed` for each removed item. Trailing holes are trimmed once at
    /// the end. Runs in O(n) regardless of how many items are removed, so the
    /// purge can never be priced out of the compute budget.
    pub fn remove_items_matching<P, R>(&mut self, mut pred: P, mut on_removed: R)
    where
        P: FnMut(&QueueItem) -> bool,
        R: FnMut(&QueueItem),
    {
        let mut cursor = Self::items_start();
        let end = core::cmp::min(self.acc.len(), self.header.cursor as usize);
        let mut last_used_end_aligned = Self::items_start();

        while cursor + size_of::<QueueItem>() <= end {
            let bytes = &mut self.acc[cursor..cursor + size_of::<QueueItem>()];
            let mut item = Self::read_item_unaligned(bytes);
            let next = Self::item_next(cursor, &item);

            if item.used == 1 {
                if pred(&item) {
                    // Logically remove in place; offsets stay valid.
                    item.used = 0;
                    Self::write_item_unaligned(bytes, &item);
                    self.header.item_count = self.header.item_count.saturating_sub(1);
                    on_removed(&item);
                } else {
                    last_used_end_aligned = next;
                }
            }

            cursor = next;
        }

        // Trim trailing holes once.
        if (last_used_end_aligned as u32) < self.header.cursor {
            self.header.cursor = last_used_end_aligned as u32;
        }
    }

    /// Find first used item by id, returning its logical index and value.
    pub fn find_item_by_id(&self, id: &[u8; 32]) -> Option<(usize, QueueItem)> {
        self.scan_items(|logical_index, _, _, item| {
            (item.used == 1 && &item.id == id).then_some((logical_index, *item))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.header.item_count == 0
    }

    pub fn len(&self) -> usize {
        self.header.item_count as usize
    }
}

impl Queue {
    /// Returns the number of active (used) items in the queue.
    pub fn len(&self) -> usize {
        self.item_count as usize
    }

    /// Returns true if the queue has no active (used) items.
    pub fn is_empty(&self) -> bool {
        self.item_count == 0
    }
}

impl crate::state::AccountWithDiscriminator for Queue {
    fn discriminator() -> AccountDiscriminator {
        AccountDiscriminator::Queue
    }
}

impl Queue {
    /// Reads the fixed-size header from a full account data slice that includes
    /// an 8-byte discriminator followed by the `Queue` header and a variable region.
    /// Accepts buffers larger than the header (unlike the default macro impl).
    pub fn try_from_bytes(data: &[u8]) -> Result<&Self, ProgramError> {
        let header_size = size_of::<Queue>();
        if data.len() < 8 + header_size {
            return Err(ProgramError::InvalidAccountData);
        }
        // Validate discriminator
        if AccountDiscriminator::Queue.to_bytes() != data[..8] {
            return Err(ProgramError::InvalidAccountData);
        }
        // SAFETY: types are Pod; slice length checked above
        bytemuck::try_from_bytes::<Queue>(&data[8..8 + header_size])
            .map_err(|_| ProgramError::InvalidAccountData)
    }

    /// Mutable variant of `try_from_bytes`.
    pub fn try_from_bytes_mut(data: &mut [u8]) -> Result<&mut Self, ProgramError> {
        let header_size = size_of::<Queue>();
        if data.len() < 8 + header_size {
            return Err(ProgramError::InvalidAccountData);
        }
        if AccountDiscriminator::Queue.to_bytes() != data[..8] {
            return Err(ProgramError::InvalidAccountData);
        }
        bytemuck::try_from_bytes_mut::<Queue>(&mut data[8..8 + header_size])
            .map_err(|_| ProgramError::InvalidAccountData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_item(id: u8) -> QueueItem {
        QueueItem {
            id: [id; 32],
            callback_program_id: [7; 32],
            ..QueueItem::default()
        }
    }

    #[test]
    fn add_item_reuses_exact_size_free_span() {
        let discriminator = [1u8; 8];
        let metas = [CompactAccountMeta {
            pubkey: [2; 32],
            is_writable: 1,
        }];
        let args = [3u8; 48];
        let span = QueueAccount::align_up(
            size_of::<QueueItem>() + discriminator.len() + size_of_val(&metas) + args.len(),
        );
        // 8 leading bytes for the account discriminator, which load() validates and skips.
        let mut data = vec![0u8; 8 + QueueAccount::items_start() + (span * 4)];
        data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
        let mut queue = QueueAccount::load(&mut data).unwrap();

        queue
            .add_item(&test_item(0), &discriminator, &metas, &args)
            .unwrap();
        queue
            .add_item(&test_item(1), &discriminator, &metas, &args)
            .unwrap();
        queue
            .add_item(&test_item(2), &discriminator, &metas, &args)
            .unwrap();

        let cursor_after_fill = queue.header.cursor;
        let removed = queue.remove_item(1).unwrap();
        let removed_pos = removed.callback_discriminator_offset as usize - size_of::<QueueItem>();
        assert_eq!(removed.id, [1; 32]);
        assert_eq!(queue.header.cursor, cursor_after_fill);

        // The next insertion reuses the freed span: the id is derived from the
        // exact byte position it is written to, which is the removed item's slot.
        let mut reused_pos = None;
        let reused_index = queue
            .add_item_with_id(&test_item(9), &discriminator, &metas, &args, |pos| {
                reused_pos = Some(pos as usize);
                [9; 32]
            })
            .unwrap();

        assert_eq!(reused_pos, Some(removed_pos));
        assert_eq!(reused_index, 1);
        assert_eq!(queue.header.cursor, cursor_after_fill);
        assert_eq!(queue.len(), 3);

        let reused = queue.get_item_by_index(1).unwrap();
        assert_eq!(reused.id, [9; 32]);
        assert_eq!(reused.callback_discriminator(queue.acc), discriminator);
        assert!(reused.account_metas(queue.acc) == metas);
        assert_eq!(reused.callback_args(queue.acc), args);

        let tail = queue.get_item_by_index(2).unwrap();
        assert_eq!(tail.id, [2; 32]);

        // With no holes left, the next item appends past the last used item.
        let mut appended_pos = None;
        let appended_index = queue
            .add_item_with_id(&test_item(11), &discriminator, &metas, &args, |pos| {
                appended_pos = Some(pos as usize);
                [11; 32]
            })
            .unwrap();
        assert_eq!(appended_index, 3);
        assert_eq!(
            appended_pos,
            Some(QueueAccount::align_up(cursor_after_fill as usize))
        );
    }

    #[test]
    fn bytemuck_casts_match_previous_unsafe_forms() {
        let item = QueueItem {
            slot: 42,
            callback_discriminator_offset: 1,
            metas_offset: 2,
            args_offset: 3,
            callback_discriminator_len: 4,
            metas_len: 5,
            args_len: 6,
            priority_request: 1,
            used: 1,
            ..test_item(9)
        };
        let metas = [
            CompactAccountMeta {
                pubkey: [2; 32],
                is_writable: 1,
            },
            CompactAccountMeta {
                pubkey: [3; 32],
                is_writable: 0,
            },
        ];

        // Writing an item: bytes_of vs raw parts over the struct
        let safe_item_bytes = bytemuck::bytes_of(&item);
        let unsafe_item_bytes = unsafe {
            core::slice::from_raw_parts(
                &item as *const QueueItem as *const u8,
                size_of::<QueueItem>(),
            )
        };
        assert_eq!(safe_item_bytes, unsafe_item_bytes);

        // Reading an item back: pod_read_unaligned vs ptr::read_unaligned (offset by 1 to
        // exercise the unaligned path)
        let mut buf = vec![0u8; 1 + size_of::<QueueItem>()];
        buf[1..].copy_from_slice(safe_item_bytes);
        let safe_read: QueueItem = bytemuck::pod_read_unaligned(&buf[1..]);
        let unsafe_read =
            unsafe { core::ptr::read_unaligned(buf[1..].as_ptr() as *const QueueItem) };
        assert_eq!(safe_read, unsafe_read);
        assert_eq!(safe_read, item);

        // Metas as bytes: cast_slice vs raw parts over the slice
        let safe_metas_bytes: &[u8] = bytemuck::cast_slice(&metas);
        let unsafe_metas_bytes = unsafe {
            core::slice::from_raw_parts(metas.as_ptr() as *const u8, size_of_val(&metas))
        };
        assert_eq!(safe_metas_bytes, unsafe_metas_bytes);

        // Bytes as metas: cast_slice vs raw parts back to the typed slice
        let safe_typed: &[CompactAccountMeta] = bytemuck::cast_slice(safe_metas_bytes);
        let unsafe_typed = unsafe {
            core::slice::from_raw_parts(
                safe_metas_bytes.as_ptr() as *const CompactAccountMeta,
                metas.len(),
            )
        };
        assert!(safe_typed == unsafe_typed);
        assert!(safe_typed == metas);
    }

    #[test]
    fn add_item_allows_twenty_five_callback_accounts() {
        let discriminator = [1u8; 8];
        let args = [3u8; 48];
        let metas = [CompactAccountMeta {
            pubkey: [2; 32],
            is_writable: 1,
        }; MAX_CALLBACK_ACCOUNTS];
        // 8 leading bytes for the account discriminator, which load() validates and skips.
        let mut data = vec![0u8; 8 + 2048];
        data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
        let mut queue = QueueAccount::load(&mut data).unwrap();

        queue
            .add_item(&test_item(0), &discriminator, &metas, &args)
            .unwrap();

        let too_many_metas = [CompactAccountMeta {
            pubkey: [2; 32],
            is_writable: 1,
        }; MAX_CALLBACK_ACCOUNTS + 1];

        assert!(queue
            .add_item(&test_item(1), &discriminator, &too_many_metas, &args)
            .is_err());
    }

    #[test]
    fn remove_items_matching_purges_full_mainnet_size_queue() {
        // Worst case on mainnet: 30,000-byte account (minus discriminator)
        // filled with minimal 96-byte items = 312 requests.
        let mut data = vec![0u8; 30_000];
        data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
        let mut queue = QueueAccount::load(&mut data).unwrap();

        let mut added = 0usize;
        while queue.add_item(&test_item(1), &[], &[], &[]).is_ok() {
            added += 1;
        }
        assert_eq!(added, 312);
        assert_eq!(queue.len(), 312);

        let mut removed = 0usize;
        queue.remove_items_matching(|_| true, |_| removed += 1);

        assert_eq!(removed, 312);
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
        assert_eq!(queue.header.cursor, QueueAccount::items_start() as u32);
    }

    #[test]
    fn remove_items_matching_keeps_survivors_and_trims_trailing_holes() {
        let discriminator = [1u8; 8];
        let metas = [CompactAccountMeta {
            pubkey: [2; 32],
            is_writable: 1,
        }];
        let args = [3u8; 48];
        let span = QueueAccount::align_up(
            size_of::<QueueItem>() + discriminator.len() + size_of_val(&metas) + args.len(),
        );
        let mut data = vec![0u8; 8 + QueueAccount::items_start() + span * 6];
        data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
        let mut queue = QueueAccount::load(&mut data).unwrap();

        for i in 0..6u8 {
            queue
                .add_item(&test_item(i), &discriminator, &metas, &args)
                .unwrap();
        }
        let cursor_after_fill = queue.header.cursor;

        // Remove one middle item (id 1) and the trailing items (ids 4, 5).
        let mut removed_ids = Vec::new();
        queue.remove_items_matching(
            |item| item.id[0] == 1 || item.id[0] >= 4,
            |item| removed_ids.push(item.id[0]),
        );

        assert_eq!(removed_ids, vec![1, 4, 5]);
        assert_eq!(queue.len(), 3);
        // Cursor trimmed to the end of the last surviving item (id 3).
        assert_eq!(
            queue.header.cursor as usize,
            QueueAccount::items_start() + span * 4
        );

        // Survivors are intact at their logical indices.
        for (logical, id) in [0u8, 2, 3].iter().enumerate() {
            let item = queue.get_item_by_index(logical).unwrap();
            assert_eq!(item.id, [*id; 32]);
            assert_eq!(item.callback_discriminator(queue.acc), discriminator);
            assert!(item.account_metas(queue.acc) == metas);
            assert_eq!(item.callback_args(queue.acc), args);
        }

        // The middle hole is reusable.
        let reused_index = queue
            .add_item(&test_item(9), &discriminator, &metas, &args)
            .unwrap();
        assert_eq!(reused_index, 1);
        assert_eq!(queue.len(), 4);
        assert_eq!(
            queue.header.cursor as usize,
            QueueAccount::items_start() + span * 4
        );

        // Appending past the survivors extends from the trimmed cursor.
        let tail_index = queue
            .add_item(&test_item(10), &discriminator, &metas, &args)
            .unwrap();
        assert_eq!(tail_index, 4);
        assert!(queue.header.cursor <= cursor_after_fill);
    }

    #[test]
    fn remove_items_matching_noop_preserves_queue() {
        let discriminator = [1u8; 8];
        let metas = [CompactAccountMeta {
            pubkey: [2; 32],
            is_writable: 1,
        }];
        let args = [3u8; 48];
        let mut data = vec![0u8; 8 + 2048];
        data[..8].copy_from_slice(&AccountDiscriminator::Queue.to_bytes());
        let mut queue = QueueAccount::load(&mut data).unwrap();

        for i in 0..3u8 {
            queue
                .add_item(&test_item(i), &discriminator, &metas, &args)
                .unwrap();
        }
        let cursor_before = queue.header.cursor;

        let mut removed = 0usize;
        queue.remove_items_matching(|_| false, |_| removed += 1);

        assert_eq!(removed, 0);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.header.cursor, cursor_before);
    }
}
