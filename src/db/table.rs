use crate::db::Hash;
use crate::errors::AppError;
use colored::Colorize;
use futures::future;
use log::trace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use tokio::fs::{read_dir, remove_file, OpenOptions};
use tokio::task;
use tokio::time::sleep;

const CHUNK_FILE_EXTENSION: &str = "yml";
const LOCK_ACQUIRE_SLEEP_MILLIS: u64 = 50;
const LOCK_ACQUIRE_TIMEOUT: u64 = 2;
const LOCK_FILE_EXTENSION: &str = "lock";

/// A table of items of type [`T`] stored by key of type [`Hash<K>`].
///
/// Get and set operations are performed directly on the file system.
///
/// Chunks are determind by truncating the key to a [`Hash<C>`]
/// All items in a chunk are serialized and written together to a [`CHUNK_FILE_EXTENSION`] file.
///
/// Chunking achieves a balance between minimizing the number of file operations and
/// the performance cost of serializing large numbers of items to a flat file format that can be
/// manually edited and version controlled.
///
/// Write operations are protected by [`LOCK_FILE_EXTENSION`] files.
pub struct Table<const K: usize, const C: usize, T> {
    /// Directory for storing the data
    pub(crate) directory: PathBuf,
    pub phantom: PhantomData<T>,
}

impl<const K: usize, const C: usize, T> Table<K, C, T> {
    /// Create a new [`Table`]
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            phantom: PhantomData,
        }
    }

    /// Get the path to the chunk file.
    fn get_chunk_path(&self, hash: Hash<C>) -> PathBuf {
        self.directory
            .join(format!("{hash}.{CHUNK_FILE_EXTENSION}"))
    }
}

impl<const K: usize, const C: usize, T> Default for Table<K, C, T> {
    fn default() -> Self {
        Self::new(PathBuf::new())
    }
}

impl<const K: usize, const C: usize, T> Table<K, C, T>
where
    T: Clone + for<'de> Deserialize<'de>,
{
    /// Get an item by hash.
    ///
    /// Returns `None` if the item is not found.
    pub fn get(&self, hash: Hash<K>) -> Result<Option<T>, AppError> {
        let chunk_path = self.get_chunk_path(get_chunk_hash(hash));
        if chunk_path.exists() {
            let chunk = read_chunk::<K, C, T>(&chunk_path)?;
            Ok(chunk.get(&hash).cloned())
        } else {
            Ok(None)
        }
    }

    /// Get all items.
    ///
    /// Items are unsorted.
    pub async fn get_all(&self) -> Result<BTreeMap<Hash<K>, T>, AppError> {
        let mut items = BTreeMap::new();
        let mut dir = read_dir(&self.directory)
            .await
            .or_else(|e| AppError::io(e, "read directory"))?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .or_else(|e| AppError::io(e, "read entry"))?
        {
            trace!("{} entry: {}", "Reading".bold(), entry.path().display());
            let path = entry.path();
            let extension = path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !path.is_file() || extension != CHUNK_FILE_EXTENSION {
                trace!("Skipping non-chunk file: {}", path.display());
                continue;
            }
            let chunk = read_chunk::<K, C, T>(&path)?;
            items.extend(chunk);
        }
        Ok(items)
    }
}

#[allow(dead_code)]
impl<const K: usize, const C: usize, T> Table<K, C, T>
where
    T: Clone + Send + Serialize + for<'de> Deserialize<'de> + 'static,
{
    /// Add or replace an item.
    pub async fn set(&self, hash: Hash<K>, item: T) -> Result<(), AppError> {
        let chunk_path = self.get_chunk_path(get_chunk_hash(hash));
        let lock = acquire_lock(&chunk_path).await?;
        let mut chunk = if chunk_path.exists() {
            read_chunk::<K, C, T>(&chunk_path)?
        } else {
            BTreeMap::new()
        };
        chunk.insert(hash, item.clone());
        write_chunk::<K, C, T>(chunk_path, chunk)?;
        release_lock(lock).await?;
        Ok(())
    }

    /// Add many items.
    ///
    /// If `replace` is true then existing items are replaced
    ///
    /// Items are chunked together to minimize IO operations.
    ///
    /// Returns the number of items added
    pub async fn set_many(
        &self,
        items: BTreeMap<Hash<K>, T>,
        replace: bool,
    ) -> Result<usize, AppError> {
        let chunks = group_by_chunk(items);
        let futures = chunks.into_iter().map(|(chunk_hash, new_chunk)| {
            let chunk_path = self.get_chunk_path(chunk_hash);
            task::spawn(
                async move { update_chunk::<K, C, T>(chunk_path, new_chunk, replace).await },
            )
        });
        let mut added = 0;
        for result in future::join_all(futures).await {
            added += result
                .unwrap_or_else(|e| AppError::external("update table", "tokio", e.to_string()))?;
        }
        Ok(added)
    }
}

/// Get the chunk hash from [`hash`]
fn get_chunk_hash<const K: usize, const C: usize>(hash: Hash<K>) -> Hash<C> {
    hash.truncate::<C>().expect("should be able to truncate")
}

fn group_by_chunk<const K: usize, const C: usize, T>(
    items: BTreeMap<Hash<K>, T>,
) -> BTreeMap<Hash<C>, BTreeMap<Hash<K>, T>> {
    let mut chunks: BTreeMap<Hash<C>, BTreeMap<Hash<K>, T>> = BTreeMap::new();
    for (hash, item) in items {
        let chunk_hash = get_chunk_hash(hash);
        chunks.entry(chunk_hash).or_insert_with(|| BTreeMap::new());
        chunks
            .get_mut(&chunk_hash)
            .expect("should be created in not exist")
            .insert(hash, item);
    }
    chunks
}

/// Read a chunk from a file.
fn read_chunk<const K: usize, const C: usize, T>(
    path: &PathBuf,
) -> Result<BTreeMap<Hash<K>, T>, AppError>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() || !path.is_file() {
        return AppError::explained(
            "read chunk",
            format!("Chunk file does not exist: {}", path.display()),
        );
    }
    trace!("{} chunk file: {}", "Reading".bold(), path.display());

    let file = File::open(path).or_else(|e| AppError::io(e, "open chunk file"))?;
    let reader = BufReader::new(file);
    serde_yaml::from_reader(reader).or_else(|e| AppError::yaml(e, "deserialize chunk"))
}

/// Write a chunk to a file
fn write_chunk<const K: usize, const C: usize, T>(
    path: PathBuf,
    chunk: BTreeMap<Hash<K>, T>,
) -> Result<(), AppError>
where
    T: Serialize,
{
    trace!("{} chunk file: {}", "Writing".bold(), path.display());
    let file = File::create(path).or_else(|e| AppError::io(e, "create chunk file"))?;
    let mut writer = BufWriter::new(file);
    serde_yaml::to_writer(&mut writer, &chunk).or_else(|e| AppError::yaml(e, "write chunk"))?;
    writer
        .flush()
        .or_else(|e| AppError::external("flush chunk", "BufWriter", format!("{e}")))?;
    Ok(())
}

/// Update the items in a chunk
///
/// If `replace` is true then existing items are replaced
async fn update_chunk<const K: usize, const C: usize, T>(
    chunk_path: PathBuf,
    new_chunk: BTreeMap<Hash<K>, T>,
    replace: bool,
) -> Result<usize, AppError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let mut added = 0;
    let lock = acquire_lock(&chunk_path).await?;
    let mut chunk = if chunk_path.exists() {
        read_chunk::<K, C, T>(&chunk_path)?
    } else {
        BTreeMap::new()
    };
    for (hash, item) in new_chunk {
        if replace || !chunk.contains_key(&hash) {
            chunk.insert(hash, item);
            added += 1;
        }
    }
    write_chunk::<K, C, T>(chunk_path, chunk)?;
    release_lock(lock).await?;
    Ok(added)
}

/// Acquire a lock
///
/// If the lock is already in use then wait
async fn acquire_lock(path: &Path) -> Result<PathBuf, AppError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(LOCK_ACQUIRE_TIMEOUT);
    let mut lock: PathBuf = path.to_path_buf();
    lock.set_extension(LOCK_FILE_EXTENSION);
    loop {
        if OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .await
            .is_ok()
        {
            return Ok(lock);
        }
        if start.elapsed() > timeout {
            return AppError::explained(
                "acquire lock",
                format!("Exceeded timeout for acquiring lock: {}", lock.display()),
            );
        }
        sleep(Duration::from_millis(LOCK_ACQUIRE_SLEEP_MILLIS)).await;
    }
}

async fn release_lock(path: PathBuf) -> Result<(), AppError> {
    remove_file(path)
        .await
        .or_else(|e| AppError::io(e, "release lock"))
}
