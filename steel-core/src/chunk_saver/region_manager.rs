//! Region file manager with seek-based chunk access.
//!
//! Uses a sector-based format where only the header (8KB) is kept in memory.
//! Chunk data is read on-demand from disk and converted directly to runtime
//! format, avoiding memory duplication.

use std::{
    fmt,
    io::{self},
    path::PathBuf,
    sync::Weak,
};

use rustc_hash::FxHashMap;
use steel_utils::{ChunkPos, locks::AsyncRwLock};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::oneshot,
};

use crate::chunk::status::ChunkStatus;
use crate::world::World;

use super::{
    ChunkStorage, LoadedChunk, PersistentChunk,
    format::{
        CHUNK_TABLE_SIZE, ChunkEntry, FILE_HEADER_SIZE, FIRST_DATA_SECTOR, FORMAT_VERSION,
        MAX_CHUNK_SIZE, REGION_MAGIC, RegionHeader, RegionPos, SECTOR_SIZE,
    },
};

#[derive(Debug)]
struct CorruptChunkData(String);

impl fmt::Display for CorruptChunkData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Manages region files with seek-based chunk access.
///
/// Only keeps region headers (8KB each) in memory, not chunk data.
/// Chunks are loaded on-demand and converted directly to runtime format.
pub struct RegionManager {
    /// Base directory for region files (e.g., "world/region").
    base_path: PathBuf,
    /// Open region file handles with their headers.
    regions: AsyncRwLock<FxHashMap<RegionPos, RegionHandle>>,
}

/// Prepared chunk data ready to be saved asynchronously.
/// Created by `prepare_chunk_save` during the holder's snapshot-preparation phase.
pub struct PreparedChunkSave {
    /// The chunk position.
    pub pos: ChunkPos,
    /// The highest persisted status captured with the chunk data.
    pub status: ChunkStatus,
    /// The serialized chunk data.
    pub persistent: PersistentChunk<'static>,
    /// Runtime manager entity IDs that were either serialized or explicitly skipped.
    pub handled_runtime_entity_ids: Vec<i32>,
}

/// An open region file with its header.
struct RegionHandle {
    /// File handle for reading/writing.
    file: File,
    /// Chunk location header (8KB).
    header: RegionHeader,
    /// Number of chunks currently loaded from this region.
    loaded_chunk_count: usize,
    /// Whether the chunk table on disk may differ from `header`. Saves write their entry as
    /// they go, so this stays set only when that write fails or is abandoned, and the table is
    /// then rewritten in full on the next flush or close.
    header_dirty: bool,
    /// Current file size in sectors.
    file_sectors: u32,
    /// Sector ranges (`(start, end)`, end exclusive) that `header` does not use but the table
    /// on disk may point at, kept from allocation until the table is rewritten in full.
    reserved_sectors: Vec<(u32, u32)>,
    /// Makes every full chunk table rewrite fail.
    #[cfg(test)]
    fail_full_table_writes: bool,
    /// Makes every table entry write stop partway and fail, to test what a failed save
    /// leaves behind.
    #[cfg(test)]
    fail_table_entry_writes: bool,
}

impl RegionManager {
    /// Creates a new region manager.
    ///
    /// # Arguments
    /// * `base_path` - Directory where region files are stored.
    /// * `registry` - The registry for block state and biome conversions.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
            regions: AsyncRwLock::new(FxHashMap::default()),
        }
    }

    /// Gets the file path for a region.
    fn region_path(&self, pos: RegionPos) -> PathBuf {
        self.base_path.join(pos.filename())
    }

    /// Opens or creates a region file, loading only the header.
    async fn open_region(&self, pos: RegionPos) -> io::Result<RegionHandle> {
        let path = self.region_path(pos);

        if !path.exists() {
            // Create new region file with empty header
            return self.create_region(pos).await;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await?;

        // A file too short for the chunk table holds no chunks: creating the region was cut
        // short, as by a crash. Like vanilla, which reads a truncated header as empty, start
        // the region over.
        let file_size = file.metadata().await?.len();
        if file_size < (FILE_HEADER_SIZE + CHUNK_TABLE_SIZE) as u64 {
            drop(file);
            tracing::warn!(
                "Region file {} is {file_size} bytes, too short for its chunk table; recreating it",
                path.display()
            );
            return self.create_region(pos).await;
        }

        // Read and verify magic + version
        let mut header_bytes = [0u8; FILE_HEADER_SIZE];
        file.read_exact(&mut header_bytes).await?;

        let magic = &header_bytes[0..4];
        if magic != REGION_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid region file magic",
            ));
        }

        let version = u16::from_le_bytes([header_bytes[4], header_bytes[5]]);
        if version != FORMAT_VERSION {
            // Version mismatch — backup the old file and create a fresh region.
            drop(file);
            let backup_path = path.with_extension(format!("srg.v{version}.bak"));
            tracing::warn!(
                "Region file {} has version {version} (expected {FORMAT_VERSION}), backing up to {} and recreating",
                path.display(),
                backup_path.display()
            );
            fs::rename(&path, &backup_path).await?;
            return self.create_region(pos).await;
        }

        // Read chunk table
        let mut table_bytes = vec![0u8; CHUNK_TABLE_SIZE];
        file.read_exact(&mut table_bytes).await?;
        let header = RegionHeader::from_bytes(&table_bytes).map_err(|index| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("region chunk table entry {index} has an invalid status byte"),
            )
        })?;

        // Calculate file size in sectors
        let file_size = file.seek(io::SeekFrom::End(0)).await?;
        let file_sectors = file_size.div_ceil(SECTOR_SIZE as u64) as u32;
        Self::validate_region_entries(&header, file_sectors)?;

        Ok(RegionHandle {
            file,
            header,
            loaded_chunk_count: 0,
            header_dirty: false,
            file_sectors,
            reserved_sectors: Vec::new(),
            #[cfg(test)]
            fail_full_table_writes: false,
            #[cfg(test)]
            fail_table_entry_writes: false,
        })
    }

    /// Creates a new empty region file.
    async fn create_region(&self, pos: RegionPos) -> io::Result<RegionHandle> {
        fs::create_dir_all(&self.base_path).await?;

        let path = self.region_path(pos);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await?;

        // The file header and the empty chunk table, in one write.
        let header = RegionHeader::new();
        let mut header_bytes = Vec::with_capacity(FILE_HEADER_SIZE + CHUNK_TABLE_SIZE);
        header_bytes.extend_from_slice(&REGION_MAGIC);
        header_bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        header_bytes.resize(FILE_HEADER_SIZE, 0);
        header_bytes.extend_from_slice(&header.to_bytes());
        file.write_all(&header_bytes).await?;
        file.flush().await?;

        Ok(RegionHandle {
            file,
            header,
            loaded_chunk_count: 0,
            header_dirty: false,
            file_sectors: FIRST_DATA_SECTOR,
            reserved_sectors: Vec::new(),
            #[cfg(test)]
            fail_full_table_writes: false,
            #[cfg(test)]
            fail_table_entry_writes: false,
        })
    }

    /// Writes the header to disk.
    async fn write_header(file: &mut File, header: &RegionHeader) -> io::Result<()> {
        file.seek(io::SeekFrom::Start(FILE_HEADER_SIZE as u64))
            .await?;
        file.write_all(&header.to_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Rewrites the region's whole chunk table from memory, after which the table on disk
    /// matches it and no space needs to stay reserved.
    async fn rewrite_table(handle: &mut RegionHandle) -> io::Result<()> {
        #[cfg(test)]
        if handle.fail_full_table_writes {
            return Err(io::Error::other("injected chunk table write failure"));
        }
        Self::write_header(&mut handle.file, &handle.header).await?;
        handle.header_dirty = false;
        handle.reserved_sectors.clear();
        Ok(())
    }

    /// Writes one chunk's table entry to disk.
    async fn write_table_entry(
        handle: &mut RegionHandle,
        index: usize,
        entry: ChunkEntry,
    ) -> io::Result<()> {
        let entry_offset = FILE_HEADER_SIZE + index * size_of::<[u8; 8]>();
        handle
            .file
            .seek(io::SeekFrom::Start(entry_offset as u64))
            .await?;
        #[cfg(test)]
        if handle.fail_table_entry_writes {
            // Only the sector offset gets written.
            handle.file.write_all(&entry.to_bytes()[..4]).await?;
            handle.file.flush().await?;
            return Err(io::Error::other("injected table entry write failure"));
        }
        handle.file.write_all(&entry.to_bytes()).await?;
        handle.file.flush().await
    }

    /// Reads a chunk's compressed data from disk.
    async fn read_chunk_data(
        file: &mut File,
        sector_offset: u32,
        size: u32,
    ) -> io::Result<Vec<u8>> {
        let byte_offset = u64::from(sector_offset) * SECTOR_SIZE as u64;
        file.seek(io::SeekFrom::Start(byte_offset)).await?;

        let mut compressed = vec![0u8; size as usize];
        file.read_exact(&mut compressed).await?;
        Ok(compressed)
    }

    fn validate_chunk_entry(entry: ChunkEntry, file_sectors: u32) -> io::Result<()> {
        if entry.sector_offset < FIRST_DATA_SECTOR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk table entry points into the region header at sector {}",
                    entry.sector_offset
                ),
            ));
        }
        if entry.size_bytes == 0 || entry.size_bytes as usize > MAX_CHUNK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid chunk table entry size {}", entry.size_bytes),
            ));
        }
        let Some(end_sector) = entry.sector_offset.checked_add(entry.sector_count()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk table entry sector range overflowed",
            ));
        };
        if end_sector > file_sectors {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk table entry ends at sector {end_sector}, past region end {file_sectors}"
                ),
            ));
        }
        Ok(())
    }

    fn validate_region_entries(header: &RegionHeader, file_sectors: u32) -> io::Result<()> {
        let mut occupied = vec![false; file_sectors as usize];
        for sector in occupied.iter_mut().take(FIRST_DATA_SECTOR as usize) {
            *sector = true;
        }
        for (index, &entry) in header.entries.iter().enumerate() {
            if !entry.exists() {
                continue;
            }
            Self::validate_chunk_entry(entry, file_sectors)?;
            let start = entry.sector_offset as usize;
            let end = start + entry.sector_count() as usize;
            if occupied[start..end].iter().any(|is_occupied| *is_occupied) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("chunk table entry {index} overlaps another region allocation"),
                ));
            }
            occupied[start..end].fill(true);
        }
        Ok(())
    }

    async fn clear_corrupt_chunk_if_unchanged(
        &self,
        region_pos: RegionPos,
        index: usize,
        expected_entry: ChunkEntry,
    ) -> io::Result<bool> {
        let mut regions = self.regions.write().await;
        let Some(handle) = regions.get_mut(&region_pos) else {
            return Err(io::Error::other(
                "region was released while clearing corrupt chunk data",
            ));
        };
        if handle.header.entries[index] != expected_entry {
            return Ok(false);
        }

        // Until the table is rewritten, the table on disk still points at the corrupt copy, so
        // its space must not be reused even if this is abandoned partway; a full rewrite
        // releases it.
        handle.header_dirty = true;
        handle.reserved_sectors.push((
            expected_entry.sector_offset,
            expected_entry.sector_offset + expected_entry.sector_count(),
        ));
        handle.header.entries[index] = ChunkEntry::empty();
        if let Err(error) = Self::rewrite_table(handle).await {
            handle.header.entries[index] = expected_entry;
            return Err(error);
        }
        Ok(true)
    }

    /// Writes chunk data to disk at the specified sector offset.
    async fn write_chunk_data(
        file: &mut File,
        sector_offset: u32,
        data: &[u8],
        file_sectors: &mut u32,
    ) -> io::Result<()> {
        let byte_offset = u64::from(sector_offset) * SECTOR_SIZE as u64;
        file.seek(io::SeekFrom::Start(byte_offset)).await?;
        file.write_all(data).await?;

        // Pad to sector boundary
        let padding_needed = (SECTOR_SIZE - (data.len() % SECTOR_SIZE)) % SECTOR_SIZE;
        if padding_needed > 0 {
            file.write_all(&vec![0u8; padding_needed]).await?;
        }

        // Update file sectors if we wrote past the end
        let sectors_used = data.len().div_ceil(SECTOR_SIZE) as u32;
        let end_sector = sector_offset + sectors_used;
        if end_sector > *file_sectors {
            *file_sectors = end_sector;
        }

        file.flush().await?;
        Ok(())
    }

    /// Saves prepared chunk data to disk after the snapshot-preparation phase has ended.
    #[expect(
        clippy::missing_panics_doc,
        reason = "panic on `just inserted` is unreachable"
    )]
    pub async fn save_chunk_data(
        &self,
        prepared: PreparedChunkSave,
        thread_pool: &rayon::ThreadPool,
    ) -> io::Result<bool> {
        let pos = prepared.pos;
        let status = prepared.status;
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);

        let (sender, receiver) = oneshot::channel();
        thread_pool.spawn(move || {
            let result = Self::encode_chunk(prepared);
            if sender.send(result).is_err() {
                tracing::trace!(
                    chunk = ?pos,
                    "Discarding encoded chunk after its save task was canceled"
                );
            }
        });
        let compressed = receiver.await.map_err(|_| {
            io::Error::other("chunk encode task ended without returning a result")
        })??;

        let mut regions = self.regions.write().await;

        // Get or open the region
        let handle = if let Some(handle) = regions.get_mut(&region_pos) {
            handle
        } else {
            let handle = self.open_region(region_pos).await?;
            regions.insert(region_pos, handle);
            regions.get_mut(&region_pos).expect("just inserted")
        };

        // Like vanilla's `RegionFile.write`, the new copy goes to free sectors and the old copy
        // stays allocated until the chunk's table entry on disk points at the new one, so a
        // process crash at any point leaves the table pointing at a complete copy. The old
        // entry is still in the in-memory table here, so the allocation cannot overlap it.
        let sectors_needed = compressed.len().div_ceil(SECTOR_SIZE) as u32;
        let sector_offset = handle
            .header
            .find_free_sectors(sectors_needed, &handle.reserved_sectors);
        Self::write_chunk_data(
            &mut handle.file,
            sector_offset,
            &compressed,
            &mut handle.file_sectors,
        )
        .await?;

        // Until the entry write finishes, the entry on disk may point at the new copy while the
        // in-memory table still has the old one. Reserve the new copy and mark the table dirty
        // first, so neither a failed nor an abandoned write lets another save reuse it.
        // A partly written entry could pair the new offset with the old size, so reserve the
        // larger of the two.
        let was_dirty = handle.header_dirty;
        let reservations = handle.reserved_sectors.len();
        let reserved_sectors = sectors_needed.max(handle.header.entries[index].sector_count());
        handle.header_dirty = true;
        handle
            .reserved_sectors
            .push((sector_offset, sector_offset + reserved_sectors));
        let entry = ChunkEntry::new(sector_offset, compressed.len() as u32, status);
        Self::write_table_entry(handle, index, entry).await?;
        handle.header_dirty = was_dirty;
        handle.reserved_sectors.truncate(reservations);
        // Only now is the old copy's space free for reuse.
        handle.header.entries[index] = entry;

        // With no chunks loaded from it, the region is only open for this save (or because an
        // earlier table write failed), so close it now. The save is already on disk, so a
        // failed close does not fail it: the region stays open and dirty, and a later release,
        // save, flush or close retries.
        if handle.loaded_chunk_count == 0
            && let Err(error) = Self::close_region(&mut regions, region_pos).await
        {
            tracing::warn!(
                "Couldn't write the chunk table of region {region_pos:?} after saving {pos:?}; \
                 it will be retried: {error}"
            );
        }

        Ok(true)
    }

    fn encode_chunk(prepared: PreparedChunkSave) -> io::Result<Vec<u8>> {
        let data = wincode::serialize(&prepared.persistent)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let compressed = zstd::encode_all(&data[..], 3)?;

        if compressed.len() > MAX_CHUNK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Chunk too large: {} bytes (max {})",
                    compressed.len(),
                    MAX_CHUNK_SIZE
                ),
            ));
        }

        Ok(compressed)
    }

    /// Loads a chunk from the appropriate region.
    ///
    /// Automatically opens the region if not already open. The region's reference
    /// count is incremented, so you must call `release_chunk` when done with the chunk.
    ///
    /// Returns `Ok(None)` if the chunk doesn't exist on disk.
    ///
    /// # Arguments
    /// * `pos` - The chunk position
    /// * `min_y` - The minimum Y coordinate of the world
    /// * `height` - The total height of the world
    /// * `level` - Weak reference to the world for Full chunk runtime access
    ///
    /// The region must already be acquired via `acquire_chunk` before calling this.
    pub async fn load_chunk(
        &self,
        pos: ChunkPos,
        min_y: i32,
        height: i32,
        level: Weak<World>,
        thread_pool: &rayon::ThreadPool,
    ) -> io::Result<Option<LoadedChunk>> {
        if height <= 0 || height % 16 != 0 || min_y % 16 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk world range must be section-aligned, got min_y={min_y}, height={height}"
                ),
            ));
        }
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);

        let (compressed, entry) = {
            let mut regions = self.regions.write().await;

            // Get the region (should already be open via acquire_chunk)
            let Some(handle) = regions.get_mut(&region_pos) else {
                log::warn!("load_chunk called without acquire_chunk for region {region_pos:?}");
                return Ok(None);
            };

            // Check if chunk exists
            let entry = handle.header.entries[index];
            if !entry.exists() {
                return Ok(None);
            }

            // Invalid offsets and sizes indicate damage to the region's location
            // table, not a self-contained chunk payload. Do not discard the slot.
            Self::validate_chunk_entry(entry, handle.file_sectors)?;

            // Read chunk data from disk
            let compressed =
                Self::read_chunk_data(&mut handle.file, entry.sector_offset, entry.size_bytes)
                    .await?;
            (compressed, entry)
        };

        // Keep CPU-heavy decoding off the async runtime. Awaiting the Rayon
        // handoff also lets the region-lock waiter woken above make progress.
        let (sender, receiver) = oneshot::channel();
        thread_pool.spawn(move || {
            let result = Self::decode_chunk(compressed, pos, entry.status, min_y, height, level);
            if sender.send(result).is_err() {
                tracing::trace!(
                    chunk = ?pos,
                    "Discarding decoded chunk after its load task was canceled"
                );
            }
        });

        let decoded = receiver
            .await
            .map_err(|_| io::Error::other("chunk decode task ended without returning a result"))?;
        match decoded {
            Ok(loaded) => Ok(Some(loaded)),
            Err(error) => {
                if !self
                    .clear_corrupt_chunk_if_unchanged(region_pos, index, entry)
                    .await?
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "corrupt chunk payload was superseded before it could be removed: {error}"
                        ),
                    ));
                }
                tracing::error!(
                    chunk = ?pos,
                    "Discarded corrupt chunk payload and will regenerate it: {error}",
                );
                Ok(None)
            }
        }
    }

    fn decode_chunk(
        compressed: Vec<u8>,
        pos: ChunkPos,
        status: ChunkStatus,
        min_y: i32,
        height: i32,
        level: Weak<World>,
    ) -> Result<LoadedChunk, CorruptChunkData> {
        let data = zstd::decode_all(&compressed[..])
            .map_err(|error| CorruptChunkData(format!("zstd decode failed: {error}")))?;
        let persistent: PersistentChunk<'_> = wincode::deserialize(&data)
            .map_err(|error| CorruptChunkData(format!("chunk decode failed: {error}")))?;

        ChunkStorage::try_persistent_to_chunk(&persistent, pos, status, min_y, height, level)
            .map_err(|error| CorruptChunkData(format!("chunk materialization failed: {error}")))
    }

    /// Acquires a chunk, incrementing the region's reference count.
    ///
    /// This opens or creates the region file. Call this before loading or
    /// generating a chunk, and call `release_chunk` when done with the chunk.
    ///
    /// Returns `Ok(true)` if the chunk exists on disk, `Ok(false)` if it doesn't.
    #[expect(
        clippy::missing_panics_doc,
        reason = "panic on `just inserted` is unreachable"
    )]
    pub async fn acquire_chunk(&self, pos: ChunkPos) -> io::Result<bool> {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);

        let mut regions = self.regions.write().await;

        // Get or open/create the region
        let handle = if let Some(handle) = regions.get_mut(&region_pos) {
            handle
        } else {
            // open_region creates the file if it doesn't exist
            let handle = self.open_region(region_pos).await?;
            regions.insert(region_pos, handle);
            regions.get_mut(&region_pos).expect("just inserted")
        };

        // Check if chunk exists
        let exists = handle.header.entries[index].exists();

        // Increment ref count
        handle.loaded_chunk_count += 1;

        Ok(exists)
    }

    /// Releases a loaded chunk, decrementing the region's reference count.
    ///
    /// When all chunks from a region are released, the header is saved (if dirty)
    /// and the file handle is closed.
    ///
    /// This must be called for each chunk returned by `load_chunk`.
    pub async fn release_chunk(&self, pos: ChunkPos) -> io::Result<()> {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);

        let mut regions = self.regions.write().await;

        let should_close = if let Some(handle) = regions.get_mut(&region_pos) {
            handle.loaded_chunk_count = handle.loaded_chunk_count.saturating_sub(1);
            handle.loaded_chunk_count == 0
        } else {
            return Ok(());
        };

        if should_close {
            Self::close_region(&mut regions, region_pos).await?;
        }

        Ok(())
    }

    /// Writes a region's chunk table if it is dirty, then closes the region.
    ///
    /// If the write fails the region stays open and dirty: its table on disk may still hold a
    /// partly written entry, and a later release, flush or close retries.
    async fn close_region(
        regions: &mut FxHashMap<RegionPos, RegionHandle>,
        region_pos: RegionPos,
    ) -> io::Result<()> {
        let Some(handle) = regions.get_mut(&region_pos) else {
            return Ok(());
        };
        if handle.header_dirty {
            Self::rewrite_table(handle).await?;
        }
        regions.remove(&region_pos);
        Ok(())
    }

    /// Checks if a chunk exists on disk without loading it.
    pub async fn chunk_exists(&self, pos: ChunkPos) -> io::Result<bool> {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);

        let regions = self.regions.write().await;

        // Check cached header first
        if let Some(handle) = regions.get(&region_pos) {
            return Ok(handle.header.entries[index].exists());
        }

        drop(regions);

        // Need to read header from disk
        let path = self.region_path(region_pos);
        if !path.exists() {
            return Ok(false);
        }

        let mut file = File::open(&path).await?;

        // Skip magic + version
        file.seek(io::SeekFrom::Start(FILE_HEADER_SIZE as u64))
            .await?;

        // Read just the one entry we need (8 bytes at index * 8)
        file.seek(io::SeekFrom::Current((index * 8) as i64)).await?;
        let mut entry_bytes = [0u8; 8];
        file.read_exact(&mut entry_bytes).await?;

        let Some(entry) = super::format::ChunkEntry::from_bytes(entry_bytes) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk table entry has an invalid status byte",
            ));
        };
        Ok(entry.exists())
    }

    /// Flushes all dirty headers to disk.
    ///
    /// Every region is attempted; one that fails stays dirty and the first error is returned.
    pub async fn flush_all(&self) -> io::Result<()> {
        let mut regions = self.regions.write().await;

        let mut result = Ok(());
        for handle in regions.values_mut() {
            if !handle.header_dirty {
                continue;
            }
            if let Err(error) = Self::rewrite_table(handle).await
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }

    /// Flushes all dirty headers and closes all region file handles.
    ///
    /// This should be called during graceful shutdown after all chunks have been saved.
    /// It ensures all data is persisted and file handles are properly closed.
    ///
    /// Every region is attempted; one whose header cannot be written stays open and dirty, and
    /// the first error is returned.
    pub async fn close_all(&self) -> io::Result<()> {
        let mut regions = self.regions.write().await;

        let positions: Vec<RegionPos> = regions.keys().copied().collect();
        let mut result = Ok(());
        for region_pos in positions {
            if let Err(error) = Self::close_region(&mut regions, region_pos).await
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, mem,
        path::Path,
        process,
        sync::{
            Weak,
            atomic::{AtomicU64, Ordering},
        },
    };

    use super::*;
    use steel_utils::Identifier;

    use crate::chunk_saver::{PersistentBlockState, PersistentChunk, PersistentLightData};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "steel-region-manager-{name}-{}-{sequence}",
            process::id()
        ))
    }

    async fn write_test_region(
        directory: &Path,
        pos: ChunkPos,
        payload: &[u8],
        declared_size: u32,
    ) -> io::Result<()> {
        fs::create_dir_all(directory).await?;
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let path = directory.join(region_pos.filename());
        let mut file = File::create(path).await?;
        let mut file_header = [0u8; FILE_HEADER_SIZE];
        file_header[0..4].copy_from_slice(&REGION_MAGIC);
        file_header[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        file.write_all(&file_header).await?;

        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);
        let mut header = RegionHeader::new();
        header.entries[index] =
            ChunkEntry::new(FIRST_DATA_SECTOR, declared_size, ChunkStatus::Empty);
        file.write_all(&header.to_bytes()).await?;
        file.seek(io::SeekFrom::Start(
            u64::from(FIRST_DATA_SECTOR) * SECTOR_SIZE as u64,
        ))
        .await?;
        file.write_all(payload).await?;
        file.flush().await
    }

    fn test_thread_pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("test thread pool should build")
    }

    async fn assert_slot_exists_on_disk(directory: &Path, pos: ChunkPos) {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let path = directory.join(region_pos.filename());
        let mut file = File::open(path)
            .await
            .expect("test region should remain readable");
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);
        file.seek(io::SeekFrom::Start(
            FILE_HEADER_SIZE as u64 + (index * 8) as u64,
        ))
        .await
        .expect("chunk table entry should be seekable");
        let mut bytes = [0; 8];
        file.read_exact(&mut bytes)
            .await
            .expect("chunk table entry should be readable");
        assert!(ChunkEntry::from_bytes(bytes).is_some_and(|entry| entry.exists()));
    }

    #[tokio::test]
    async fn invalid_zstd_payload_is_removed_for_regeneration() {
        let directory = test_directory("zstd");
        let pos = ChunkPos::new(0, 0);
        let payload = b"this is not a zstd frame";
        write_test_region(&directory, pos, payload, payload.len() as u32)
            .await
            .expect("test region should be written");

        let manager = RegionManager::new(&directory);
        assert!(
            manager
                .acquire_chunk(pos)
                .await
                .expect("region should open")
        );
        let loaded = manager
            .load_chunk(pos, 0, 16, Weak::new(), &test_thread_pool())
            .await
            .expect("corrupt payload should be handled");
        assert!(loaded.is_none());
        assert!(
            !manager
                .chunk_exists(pos)
                .await
                .expect("header should be readable")
        );
        manager
            .release_chunk(pos)
            .await
            .expect("region should release");

        let reopened = RegionManager::new(&directory);
        assert!(
            !reopened
                .chunk_exists(pos)
                .await
                .expect("header should be flushed")
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn semantically_invalid_complete_payload_is_removed_for_regeneration() {
        let directory = test_directory("semantic");
        let pos = ChunkPos::new(0, 0);
        let persistent = PersistentChunk {
            last_modified: 0,
            block_states: Vec::new(),
            biomes: Vec::new(),
            sections: Vec::new(),
            block_entities: Vec::new(),
            entities: Vec::new(),
            block_ticks: Vec::new(),
            fluid_ticks: Vec::new(),
            heightmaps: Vec::new(),
            light: PersistentLightData::default(),
            postprocessing: Vec::new(),
            structure_starts: Vec::new(),
            structure_references: Vec::new(),
            pois: Vec::new(),
        };
        let encoded = wincode::serialize(&persistent).expect("test chunk should encode");
        let payload = zstd::encode_all(encoded.as_slice(), 1).expect("test chunk should compress");
        write_test_region(&directory, pos, &payload, payload.len() as u32)
            .await
            .expect("test region should be written");

        let manager = RegionManager::new(&directory);
        assert!(
            manager
                .acquire_chunk(pos)
                .await
                .expect("region should open")
        );
        let loaded = manager
            .load_chunk(pos, 0, 16, Weak::new(), &test_thread_pool())
            .await
            .expect("semantic corruption should be handled");
        assert!(loaded.is_none());
        assert!(
            !manager
                .chunk_exists(pos)
                .await
                .expect("slot should be cleared")
        );
        manager
            .release_chunk(pos)
            .await
            .expect("region should release");
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn incomplete_payload_read_is_an_error_and_keeps_slot() {
        let directory = test_directory("short-read");
        let pos = ChunkPos::new(0, 0);
        write_test_region(&directory, pos, &[1, 2, 3], 128)
            .await
            .expect("test region should be written");

        let manager = RegionManager::new(&directory);
        assert!(
            manager
                .acquire_chunk(pos)
                .await
                .expect("region should open")
        );
        let Err(error) = manager
            .load_chunk(pos, 0, 16, Weak::new(), &test_thread_pool())
            .await
        else {
            panic!("short filesystem read must not be treated as payload corruption");
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(manager.chunk_exists(pos).await.expect("slot should remain"));
        manager
            .release_chunk(pos)
            .await
            .expect("region should release");
        assert_slot_exists_on_disk(&directory, pos).await;
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn invalid_world_geometry_is_not_classified_as_chunk_corruption() {
        let directory = test_directory("invalid-world-range");
        let pos = ChunkPos::new(0, 0);
        let payload = b"payload must not be decoded";
        write_test_region(&directory, pos, payload, payload.len() as u32)
            .await
            .expect("test region should be written");

        let manager = RegionManager::new(&directory);
        assert!(
            manager
                .acquire_chunk(pos)
                .await
                .expect("region should open")
        );
        let Err(error) = manager
            .load_chunk(pos, 1, 16, Weak::new(), &test_thread_pool())
            .await
        else {
            panic!("invalid world geometry must fail before decoding the chunk");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(manager.chunk_exists(pos).await.expect("slot should remain"));
        manager
            .release_chunk(pos)
            .await
            .expect("region should release");
        assert_slot_exists_on_disk(&directory, pos).await;
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn invalid_chunk_status_byte_is_a_structural_error_and_is_preserved() {
        let directory = test_directory("invalid-status");
        let pos = ChunkPos::new(0, 0);
        let payload = b"payload must not be decoded";
        write_test_region(&directory, pos, payload, payload.len() as u32)
            .await
            .expect("test region should be written");

        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let path = directory.join(region_pos.filename());
        let mut file = OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .expect("test region should reopen for corruption");
        file.seek(io::SeekFrom::Start(FILE_HEADER_SIZE as u64 + 7))
            .await
            .expect("status byte should be seekable");
        file.write_all(&[u8::MAX])
            .await
            .expect("status byte should be writable");
        file.flush().await.expect("status byte should be flushed");
        drop(file);

        let manager = RegionManager::new(&directory);
        let Err(error) = manager.acquire_chunk(pos).await else {
            panic!("invalid status byte must reject the region header");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut file = File::open(path)
            .await
            .expect("rejected region should remain readable");
        file.seek(io::SeekFrom::Start(FILE_HEADER_SIZE as u64 + 7))
            .await
            .expect("status byte should remain seekable");
        let mut status = [0];
        file.read_exact(&mut status)
            .await
            .expect("status byte should remain readable");
        assert_eq!(status[0], u8::MAX);

        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    fn empty_saved_chunk(pos: ChunkPos) -> PreparedChunkSave {
        PreparedChunkSave {
            pos,
            status: ChunkStatus::Full,
            persistent: PersistentChunk {
                last_modified: 0,
                block_states: Vec::new(),
                biomes: Vec::new(),
                sections: Vec::new(),
                block_entities: Vec::new(),
                entities: Vec::new(),
                block_ticks: Vec::new(),
                fluid_ticks: Vec::new(),
                heightmaps: Vec::new(),
                light: PersistentLightData::default(),
                postprocessing: Vec::new(),
                structure_starts: Vec::new(),
                structure_references: Vec::new(),
                pois: Vec::new(),
            },
            handled_runtime_entity_ids: Vec::new(),
        }
    }

    fn saved_chunk(pos: ChunkPos, last_modified: u32) -> PreparedChunkSave {
        let mut prepared = empty_saved_chunk(pos);
        prepared.persistent.last_modified = last_modified;
        prepared
    }

    /// Opens `pos`'s region and saves an empty chunk into it, then fails a second save's table
    /// entry write partway, leaving the region open with a dirty chunk table.
    async fn save_into_open_region(manager: &RegionManager, pos: ChunkPos) -> ChunkEntry {
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");
        let saved = manager
            .save_chunk_data(empty_saved_chunk(pos), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        set_table_entry_writes_failing(manager, pos, true).await;
        assert!(
            manager
                .save_chunk_data(empty_saved_chunk(pos), &test_thread_pool())
                .await
                .is_err()
        );
        set_table_entry_writes_failing(manager, pos, false).await;
        open_region_entry(manager, pos).await
    }

    async fn set_table_entry_writes_failing(manager: &RegionManager, pos: ChunkPos, fail: bool) {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let mut regions = manager.regions.write().await;
        let handle = regions.get_mut(&region_pos).expect("region should be open");
        handle.fail_table_entry_writes = fail;
    }

    async fn open_region_entry(manager: &RegionManager, pos: ChunkPos) -> ChunkEntry {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let regions = manager.regions.read().await;
        let handle = regions.get(&region_pos).expect("region should be open");
        handle.header.entries[RegionHeader::chunk_index(local_x, local_z)]
    }

    /// Reads `pos`'s table entry straight from the file, as a restart after a crash would.
    async fn entry_on_disk(directory: &Path, pos: ChunkPos) -> [u8; 8] {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let mut file = File::open(directory.join(region_pos.filename()))
            .await
            .expect("region file should open");
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);
        file.seek(io::SeekFrom::Start((FILE_HEADER_SIZE + index * 8) as u64))
            .await
            .expect("chunk table entry should be seekable");
        let mut bytes = [0; 8];
        file.read_exact(&mut bytes)
            .await
            .expect("chunk table entry should be readable");
        bytes
    }

    /// The `last_modified` of the copy `entry` points at, read straight from the file.
    async fn last_modified_on_disk(directory: &Path, pos: ChunkPos, entry: ChunkEntry) -> u32 {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let mut file = File::open(directory.join(region_pos.filename()))
            .await
            .expect("region file should open");
        let compressed =
            RegionManager::read_chunk_data(&mut file, entry.sector_offset, entry.size_bytes)
                .await
                .expect("chunk data should be readable");
        let data = zstd::decode_all(&compressed[..]).expect("chunk data should decompress");
        let persistent: PersistentChunk<'_> =
            wincode::deserialize(&data).expect("chunk data should decode");
        persistent.last_modified
    }

    /// Swaps the open region's file for a read-only handle so its chunk table write fails,
    /// returning the writable handle.
    async fn fail_table_writes(manager: &RegionManager, directory: &Path, pos: ChunkPos) -> File {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let read_only = File::open(directory.join(region_pos.filename()))
            .await
            .expect("region file should open read-only");
        let mut regions = manager.regions.write().await;
        let handle = regions.get_mut(&region_pos).expect("region should be open");
        assert!(handle.header_dirty);
        mem::replace(&mut handle.file, read_only)
    }

    async fn restore_table_writes(manager: &RegionManager, pos: ChunkPos, writable: File) {
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let mut regions = manager.regions.write().await;
        let handle = regions
            .get_mut(&region_pos)
            .expect("region should still be open");
        assert!(
            handle.header_dirty,
            "an unwritten chunk table must stay dirty"
        );
        handle.file = writable;
    }

    #[tokio::test]
    async fn failed_table_write_on_release_keeps_the_region_open_for_a_retry() {
        let directory = test_directory("release_table_failure");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        let saved = save_into_open_region(&manager, pos).await;
        let writable = fail_table_writes(&manager, &directory, pos).await;

        assert!(manager.release_chunk(pos).await.is_err());
        restore_table_writes(&manager, pos, writable).await;
        manager
            .close_all()
            .await
            .expect("the retried write should succeed");

        assert_eq!(
            entry_on_disk(&directory, pos).await,
            saved.to_bytes(),
            "the retried close must repair the entry"
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn failed_table_write_on_close_all_still_closes_the_other_regions() {
        let directory = test_directory("close_all_table_failure");
        let failing = ChunkPos::new(2, 3);
        let other = ChunkPos::new(40, 3);
        let manager = RegionManager::new(&directory);
        let failing_saved = save_into_open_region(&manager, failing).await;
        let other_saved = save_into_open_region(&manager, other).await;
        let writable = fail_table_writes(&manager, &directory, failing).await;

        assert!(manager.close_all().await.is_err());
        assert_eq!(
            entry_on_disk(&directory, other).await,
            other_saved.to_bytes(),
            "the other region's table must still be written"
        );
        restore_table_writes(&manager, failing, writable).await;
        manager
            .close_all()
            .await
            .expect("the retried write should succeed");
        assert_eq!(
            entry_on_disk(&directory, failing).await,
            failing_saved.to_bytes()
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn saving_into_an_unopened_region_writes_its_table() {
        let directory = test_directory("save_closes_region");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        let saved = manager
            .save_chunk_data(empty_saved_chunk(pos), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        assert!(manager.regions.read().await.is_empty());
        assert!(
            RegionManager::new(&directory)
                .chunk_exists(pos)
                .await
                .expect("header should be readable")
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn next_save_into_a_region_left_open_by_a_failed_write_retries_it() {
        let directory = test_directory("save_retries_table");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        let entry = save_into_open_region(&manager, pos).await;
        let writable = fail_table_writes(&manager, &directory, pos).await;
        assert!(manager.release_chunk(pos).await.is_err());
        restore_table_writes(&manager, pos, writable).await;

        // Nothing is loaded from the region any more, so the next save closes it.
        let other = ChunkPos::new(4, 3);
        let saved = manager
            .save_chunk_data(empty_saved_chunk(other), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        assert!(manager.regions.read().await.is_empty());
        assert_eq!(entry_on_disk(&directory, pos).await, entry.to_bytes());
        assert!(
            RegionManager::new(&directory)
                .chunk_exists(other)
                .await
                .expect("header should be readable")
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn each_save_reaches_the_table_on_disk_without_overwriting_the_previous_copy() {
        let directory = test_directory("save_writes_entry");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        // Loading a chunk keeps its region open, so nothing here closes the region.
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");

        let mut previous: Option<ChunkEntry> = None;
        for last_modified in 1..=3 {
            let saved = manager
                .save_chunk_data(saved_chunk(pos, last_modified), &test_thread_pool())
                .await;
            assert!(matches!(saved, Ok(true)));
            let entry = open_region_entry(&manager, pos).await;
            assert_eq!(entry_on_disk(&directory, pos).await, entry.to_bytes());
            assert_eq!(
                last_modified_on_disk(&directory, pos, entry).await,
                last_modified
            );
            if let Some(previous) = previous {
                let previous_end = previous.sector_offset + previous.sector_count();
                let end = entry.sector_offset + entry.sector_count();
                assert!(
                    end <= previous.sector_offset || entry.sector_offset >= previous_end,
                    "save {last_modified} overlapped the copy the table pointed at before it"
                );
            }
            previous = Some(entry);
        }
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn failed_table_entry_write_keeps_the_previous_copy() {
        let directory = test_directory("save_entry_failure");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");
        let saved = manager
            .save_chunk_data(saved_chunk(pos, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let first = open_region_entry(&manager, pos).await;

        set_table_entry_writes_failing(&manager, pos, true).await;
        assert!(
            manager
                .save_chunk_data(saved_chunk(pos, 2), &test_thread_pool())
                .await
                .is_err()
        );
        set_table_entry_writes_failing(&manager, pos, false).await;
        assert!(
            open_region_entry(&manager, pos).await == first,
            "the old copy must stay allocated"
        );

        // The next save must not reuse the old copy's sectors either.
        let saved = manager
            .save_chunk_data(saved_chunk(pos, 3), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let third = open_region_entry(&manager, pos).await;
        assert!(third.sector_offset >= first.sector_offset + first.sector_count());
        manager
            .release_chunk(pos)
            .await
            .expect("region should close");
        assert_eq!(entry_on_disk(&directory, pos).await, third.to_bytes());
        assert_eq!(last_modified_on_disk(&directory, pos, third).await, 3);
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn closing_after_a_failed_table_entry_write_repairs_the_entry() {
        let directory = test_directory("save_entry_repair");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");
        let saved = manager
            .save_chunk_data(saved_chunk(pos, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let first = open_region_entry(&manager, pos).await;

        set_table_entry_writes_failing(&manager, pos, true).await;
        assert!(
            manager
                .save_chunk_data(saved_chunk(pos, 2), &test_thread_pool())
                .await
                .is_err()
        );
        assert_ne!(
            entry_on_disk(&directory, pos).await,
            first.to_bytes(),
            "the injected failure should leave a partly written entry"
        );
        set_table_entry_writes_failing(&manager, pos, false).await;
        manager
            .release_chunk(pos)
            .await
            .expect("region should close");

        assert_eq!(entry_on_disk(&directory, pos).await, first.to_bytes());
        assert_eq!(last_modified_on_disk(&directory, pos, first).await, 1);
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn repeated_saves_reuse_freed_space_instead_of_growing_the_file() {
        let directory = test_directory("save_reuses_space");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");
        for last_modified in 0..20 {
            let saved = manager
                .save_chunk_data(saved_chunk(pos, last_modified), &test_thread_pool())
                .await;
            assert!(matches!(saved, Ok(true)));
        }
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let file_sectors = manager.regions.read().await[&region_pos].file_sectors;
        // One sector for the current copy and one for the copy being replaced.
        assert_eq!(file_sectors, FIRST_DATA_SECTOR + 2);
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn space_written_by_a_failed_save_stays_reserved_until_the_table_is_rewritten() {
        let directory = test_directory("save_failure_reserves");
        let chunk = ChunkPos::new(2, 3);
        let neighbour = ChunkPos::new(3, 3);
        let other = ChunkPos::new(4, 3);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(chunk)
            .await
            .expect("region should open");
        for (pos, last_modified) in [(chunk, 1), (neighbour, 1), (chunk, 2)] {
            let saved = manager
                .save_chunk_data(saved_chunk(pos, last_modified), &test_thread_pool())
                .await;
            assert!(matches!(saved, Ok(true)));
        }
        // `chunk` moved past `neighbour`, leaving a gap at the first data sector.
        assert!(open_region_entry(&manager, chunk).await.sector_offset > FIRST_DATA_SECTOR);

        set_table_entry_writes_failing(&manager, chunk, true).await;
        assert!(
            manager
                .save_chunk_data(saved_chunk(chunk, 3), &test_thread_pool())
                .await
                .is_err()
        );
        set_table_entry_writes_failing(&manager, chunk, false).await;
        let failed_copy = u32::from_le_bytes(
            entry_on_disk(&directory, chunk).await[..4]
                .try_into()
                .expect("offset is four bytes"),
        );
        assert_eq!(failed_copy, FIRST_DATA_SECTOR);

        let saved = manager
            .save_chunk_data(saved_chunk(other, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        assert_ne!(
            open_region_entry(&manager, other).await.sector_offset,
            failed_copy,
            "the table on disk may still point at the failed save's copy"
        );
        // A restart without a close must still find a consistent table.
        assert!(
            RegionManager::new(&directory)
                .acquire_chunk(chunk)
                .await
                .is_ok()
        );

        // Once the table is rewritten from memory, the space is free again.
        manager.flush_all().await.expect("table should be written");
        let saved = manager
            .save_chunk_data(saved_chunk(other, 2), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        assert_eq!(
            open_region_entry(&manager, other).await.sector_offset,
            failed_copy
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn region_left_short_by_a_crash_during_creation_is_recreated() {
        let directory = test_directory("short_region");
        let pos = ChunkPos::new(2, 3);
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        fs::create_dir_all(&directory)
            .await
            .expect("test directory should be creatable");
        let mut file_header = [0u8; FILE_HEADER_SIZE];
        file_header[0..4].copy_from_slice(&REGION_MAGIC);
        file_header[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        // A crash between writing the file header and the chunk table leaves this behind.
        fs::write(directory.join(region_pos.filename()), file_header)
            .await
            .expect("short region should be writable");

        let manager = RegionManager::new(&directory);
        assert!(
            !manager
                .acquire_chunk(pos)
                .await
                .expect("a short region holds no chunks and should be recreated")
        );
        let saved = manager
            .save_chunk_data(saved_chunk(pos, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        manager
            .release_chunk(pos)
            .await
            .expect("region should close");
        assert!(
            RegionManager::new(&directory)
                .chunk_exists(pos)
                .await
                .expect("header should be readable")
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    /// A chunk whose compressed save spans several sectors: block state names that barely
    /// compress.
    fn large_saved_chunk(pos: ChunkPos) -> PreparedChunkSave {
        let mut prepared = empty_saved_chunk(pos);
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        prepared.persistent.block_states = (0..2000)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                PersistentBlockState {
                    name: Identifier::new("test".to_owned(), format!("{state:016x}")),
                    properties: Vec::new(),
                }
            })
            .collect();
        prepared
    }

    #[tokio::test]
    async fn a_failed_entry_write_reserves_as_much_as_the_old_copy_it_may_pair_with() {
        let directory = test_directory("torn_entry_reservation");
        let chunk = ChunkPos::new(2, 3);
        let other = ChunkPos::new(4, 3);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(chunk)
            .await
            .expect("region should open");
        let saved = manager
            .save_chunk_data(large_saved_chunk(chunk), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let large = open_region_entry(&manager, chunk).await;
        assert!(
            large.sector_count() > 1,
            "the test needs a multi-sector copy"
        );

        // The new copy lands after the old one, and its entry is left as the new offset with
        // the old copy's size, reaching past the new copy.
        set_table_entry_writes_failing(&manager, chunk, true).await;
        assert!(
            manager
                .save_chunk_data(saved_chunk(chunk, 2), &test_thread_pool())
                .await
                .is_err()
        );
        set_table_entry_writes_failing(&manager, chunk, false).await;
        let torn = ChunkEntry::from_bytes(entry_on_disk(&directory, chunk).await)
            .expect("the torn entry keeps a valid status byte");
        assert_eq!(torn.size_bytes, large.size_bytes);
        assert_ne!(torn.sector_offset, large.sector_offset);

        let saved = manager
            .save_chunk_data(saved_chunk(other, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let other_entry = open_region_entry(&manager, other).await;
        assert!(
            other_entry.sector_offset >= torn.sector_offset + torn.sector_count(),
            "sector {} reused space the torn entry covers",
            other_entry.sector_offset
        );
        // A restart without a close must still find a consistent table.
        assert!(
            RegionManager::new(&directory)
                .acquire_chunk(chunk)
                .await
                .is_ok()
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn a_save_that_reached_disk_succeeds_even_if_closing_its_region_fails() {
        let directory = test_directory("save_close_failure");
        let pos = ChunkPos::new(2, 3);
        let manager = RegionManager::new(&directory);
        save_into_open_region(&manager, pos).await;
        let writable = fail_table_writes(&manager, &directory, pos).await;
        assert!(manager.release_chunk(pos).await.is_err());
        restore_table_writes(&manager, pos, writable).await;
        // Nothing is loaded from the region now, so the next save closes it, and that close's
        // table rewrite fails.
        {
            let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
            let mut regions = manager.regions.write().await;
            let handle = regions.get_mut(&region_pos).expect("region should be open");
            handle.fail_full_table_writes = true;
        }
        let other = ChunkPos::new(4, 3);
        let saved = manager
            .save_chunk_data(saved_chunk(other, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)), "{saved:?}");
        assert!(
            !manager.regions.read().await.is_empty(),
            "the region stays open for a retry"
        );
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }

    #[tokio::test]
    async fn clearing_a_corrupt_chunk_keeps_its_space_until_the_table_is_rewritten() {
        let directory = test_directory("clear_corrupt_reserves");
        let pos = ChunkPos::new(2, 3);
        let region_pos = RegionPos::from_chunk(pos.0.x, pos.0.y);
        let (local_x, local_z) = RegionPos::local_chunk_pos(pos.0.x, pos.0.y);
        let index = RegionHeader::chunk_index(local_x, local_z);
        let manager = RegionManager::new(&directory);
        manager
            .acquire_chunk(pos)
            .await
            .expect("region should open");
        let saved = manager
            .save_chunk_data(saved_chunk(pos, 1), &test_thread_pool())
            .await;
        assert!(matches!(saved, Ok(true)));
        let entry = open_region_entry(&manager, pos).await;

        // Until the table on disk drops the entry, its sectors stay reserved, whether the
        // rewrite fails, as here, or is abandoned after the entry left the in-memory table.
        manager
            .regions
            .write()
            .await
            .get_mut(&region_pos)
            .expect("open")
            .fail_full_table_writes = true;
        assert!(
            manager
                .clear_corrupt_chunk_if_unchanged(region_pos, index, entry)
                .await
                .is_err()
        );
        {
            let regions = manager.regions.read().await;
            let handle = &regions[&region_pos];
            assert!(handle.header_dirty);
            assert!(handle.reserved_sectors.contains(&(
                entry.sector_offset,
                entry.sector_offset + entry.sector_count()
            )));
        }
        manager
            .regions
            .write()
            .await
            .get_mut(&region_pos)
            .expect("open")
            .fail_full_table_writes = false;
        assert!(
            manager
                .clear_corrupt_chunk_if_unchanged(region_pos, index, entry)
                .await
                .expect("the retried clear should succeed")
        );
        let regions = manager.regions.read().await;
        assert_eq!(regions[&region_pos].reserved_sectors, []);
        drop(regions);
        fs::remove_dir_all(directory)
            .await
            .expect("test directory should be removable");
    }
}
