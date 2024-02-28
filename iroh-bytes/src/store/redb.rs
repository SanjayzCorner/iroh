//! redb backed storage

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use bao_tree::io::{
    fsm::Outboard,
    outboard::PostOrderMemOutboard,
    sync::{ReadAt, Size},
};
use bytes::Bytes;
use futures::{channel::oneshot, Future, FutureExt, Stream, StreamExt};

use iroh_base::hash::{BlobFormat, Hash, HashAndFormat};
use iroh_io::AsyncSliceReader;
use redb::{ReadTransaction, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tokio::io::AsyncWriteExt;
use tracing::trace_span;

use crate::{
    store::bao_file::{BaoFileStorage, CompleteMemOrFileStorage},
    util::{
        progress::{IdGenerator, IgnoreProgressSender, ProgressSender},
        LivenessTracker, MemOrFile,
    },
    Tag, TempTag, IROH_BLOCK_SIZE,
};

use super::{
    bao_file::{raw_outboard_size, BaoFileConfig, BaoFileHandle},
    flatten_to_io, temp_name, BaoBatchWriter, EntryStatus, ExportMode, ImportMode, ImportProgress,
    MapEntry, ReadableStore, TempCounterMap,
};

use super::BaoBlobSize;

const BLOBS_TABLE: TableDefinition<Hash, EntryState> = TableDefinition::new("blobs-0");

const TAGS_TABLE: TableDefinition<Tag, HashAndFormat> = TableDefinition::new("tags-0");

const INLINE_DATA_TABLE: TableDefinition<Hash, &[u8]> = TableDefinition::new("inline-data-0");

const INLINE_OUTBOARD_TABLE: TableDefinition<Hash, &[u8]> =
    TableDefinition::new("inline-outboard-0");

/// Location of the data.
///
/// Data can be inlined in the database, a file conceptually owned by the store,
/// or a number of external files conceptually owned by the user.
///
/// Only complete data can be inlined.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum DataLocation<I = (), E = ()> {
    /// Data is in the inline_data table.
    Inline(I),
    /// Data is in the canonical location in the data directory.
    Owned(E),
    /// Data is in several external locations. This should be a non-empty list.
    External(Vec<PathBuf>, E),
}

impl<X> DataLocation<X, u64> {
    fn size(&self) -> Option<u64> {
        match self {
            DataLocation::Inline(_) => None,
            DataLocation::Owned(size) => Some(*size),
            DataLocation::External(_, size) => Some(*size),
        }
    }
}

impl<I, E> DataLocation<I, E> {
    fn discard_extra_data(self) -> DataLocation<(), ()> {
        match self {
            DataLocation::Inline(_) => DataLocation::Inline(()),
            DataLocation::Owned(_) => DataLocation::Owned(()),
            DataLocation::External(paths, _) => DataLocation::External(paths, ()),
        }
    }
    fn discard_inline_data(self) -> DataLocation<(), E> {
        match self {
            DataLocation::Inline(_) => DataLocation::Inline(()),
            DataLocation::Owned(x) => DataLocation::Owned(x),
            DataLocation::External(paths, x) => DataLocation::External(paths, x),
        }
    }
}

/// Location of the outboard.
///
/// Outboard can be inlined in the database or a file conceptually owned by the store.
/// Outboards are implementation specific to the store and as such are always owned.
///
/// Only complete outboards can be inlined.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum OutboardLocation<I = ()> {
    /// Outboard is in the inline_outboard table.
    Inline(I),
    /// Outboard is in the canonical location in the data directory.
    Owned,
    /// Outboard is not needed,
    NotNeeded,
}

impl<I> OutboardLocation<I> {
    fn discard_extra_data(self) -> OutboardLocation<()> {
        match self {
            Self::Inline(_) => OutboardLocation::Inline(()),
            Self::Owned => OutboardLocation::Owned,
            Self::NotNeeded => OutboardLocation::NotNeeded,
        }
    }
}

/// The information about an entry that we keep in the entry table for quick access.
///
/// The exact info to store here is TBD, so usually you should use the accessor methods.
#[derive(Debug, Serialize, Deserialize)]
enum EntryState {
    /// For a complete entry we always know the size. It does not make much sense
    /// to write to a complete entry, so they are much easier to share.
    Complete {
        /// Location of the data.
        data_location: DataLocation<(), u64>,
        /// Location of the outboard.
        outboard_location: OutboardLocation,
    },
    /// Partial entries are entries for which we know the hash, but don't have
    /// all the data. They are created when syncing from somewhere else by hash.
    ///
    /// As such they are always owned. There is also no inline storage for them.
    /// Non short lived partial entries always live in the file system, and for
    /// short lived ones we never create a database entry in the first place.
    Partial {
        /// Once we get the last chunk of a partial entry, we have validated
        /// the size of the entry despite it still being incomplete.
        ///
        /// E.g. a giant file where we just requested the last chunk.
        size: Option<u64>,
    },
}

impl Default for EntryState {
    fn default() -> Self {
        Self::Partial { size: None }
    }
}

impl EntryState {
    fn union(self, that: Self) -> io::Result<Self> {
        match (self, that) {
            (a @ Self::Complete { .. }, Self::Complete { .. }) => Ok(a),
            (a @ Self::Complete { .. }, Self::Partial { .. }) => Ok(a),
            (Self::Partial { .. }, b @ Self::Complete { .. }) => Ok(b),
            (Self::Partial { size: a_size }, Self::Partial { size: b_size }) => Ok(Self::Partial {
                size: a_size.or(b_size),
            }),
        }
    }

    fn complete(&self) -> bool {
        match self {
            Self::Complete { .. } => true,
            Self::Partial { .. } => false,
        }
    }

    /// If this is true, there should be a corresponding entry in the inline_outboard table.
    ///
    /// It is false either if there is no outboard at all, or if it in a file.
    fn inline_outboard(&self) -> bool {
        matches!(
            self,
            Self::Complete {
                outboard_location: OutboardLocation::Inline(_),
                ..
            }
        )
    }

    /// If this is true, there should be a corresponding entry in the inline_data table.
    ///
    /// It is false either if the data is in an owned file or in one or more external files.
    fn inline_data(&self) -> bool {
        matches!(
            self,
            Self::Complete {
                data_location: DataLocation::Inline(_),
                ..
            }
        )
    }

    fn owned(&self) -> bool {
        match self {
            Self::Complete { data_location, .. } => matches!(data_location, DataLocation::Owned(_)),
            Self::Partial { .. } => true,
        }
    }
}

impl redb::RedbValue for EntryState {
    type SelfType<'a> = EntryState;

    type AsBytes<'a> = SmallVec<[u8; 128]>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        postcard::from_bytes(data).unwrap()
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        postcard::to_extend(value, SmallVec::new()).unwrap()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("EntryState")
    }
}

#[derive(Debug, Clone)]
struct Options {
    /// Path to the directory where data and outboard files are stored.
    data_path: PathBuf,
    /// Path to the directory where temp files are stored.
    /// This *must* be on the same device as `data_path`, since we need to
    /// atomically move temp files into place.
    temp_path: PathBuf,
    max_data_inlined: u64,
    max_outboard_inlined: u64,
    move_threshold: u64,
}

impl Options {
    fn owned_data_path(&self, hash: &Hash) -> PathBuf {
        self.data_path.join(format!("{}.data", hash.to_hex()))
    }

    fn owned_outboard_path(&self, hash: &Hash) -> PathBuf {
        self.data_path.join(format!("{}.obao4", hash.to_hex()))
    }

    fn create_default(data_path: PathBuf, temp_path: PathBuf) -> Self {
        Self {
            data_path,
            temp_path,
            max_data_inlined: 1024 * 16,
            max_outboard_inlined: 1024 * 16,
            move_threshold: 1024 * 16,
        }
    }
}

#[derive(derive_more::Debug)]
enum ImportFile {
    TempFile(PathBuf),
    External(PathBuf),
    Memory(#[debug(skip)] Bytes),
}

impl ImportFile {
    fn content(&self) -> MemOrFile<&[u8], &Path> {
        match self {
            Self::TempFile(path) => MemOrFile::File(path.as_path()),
            Self::External(path) => MemOrFile::File(path.as_path()),
            Self::Memory(data) => MemOrFile::Mem(data.as_ref()),
        }
    }

    fn len(&self) -> io::Result<u64> {
        match self {
            Self::TempFile(path) => std::fs::metadata(path).map(|m| m.len()),
            Self::External(path) => std::fs::metadata(path).map(|m| m.len()),
            Self::Memory(data) => Ok(data.len() as u64),
        }
    }
}

///
#[derive(Debug, Clone, derive_more::From)]
pub struct Entry(BaoFileHandle);

impl super::MapEntry for Entry {
    fn hash(&self) -> Hash {
        self.0.hash().into()
    }

    fn size(&self) -> BaoBlobSize {
        let size = self.0.current_size().unwrap();
        tracing::info!("redb::Entry::size() = {}", size);
        BaoBlobSize::new(size, self.is_complete())
    }

    fn is_complete(&self) -> bool {
        self.0.is_complete()
    }

    async fn available_ranges(&self) -> io::Result<bao_tree::ChunkRanges> {
        todo!()
    }

    async fn outboard(&self) -> io::Result<impl Outboard> {
        self.0.outboard()
    }

    async fn data_reader(&self) -> io::Result<impl AsyncSliceReader> {
        Ok(self.0.data_reader())
    }
}

impl super::MapEntryMut for Entry {
    async fn batch_writer(&self) -> io::Result<impl BaoBatchWriter> {
        Ok(self.0.writer())
    }
}

fn to_io_err(e: impl Into<redb::Error>) -> io::Error {
    let e = e.into();
    match e {
        redb::Error::Io(e) => e,
        e => io::Error::new(io::ErrorKind::Other, e),
    }
}

/// Synchronously compute the outboard of a file, and return hash and outboard.
///
/// It is assumed that the file is not modified while this is running.
///
/// If it is modified while or after this is running, the outboard will be
/// invalid, so any attempt to compute a slice from it will fail.
///
/// If the size of the file is changed while this is running, an error will be
/// returned.
///
/// The computed outboard is without length prefix.
fn compute_outboard(
    read: impl Read,
    size: u64,
    progress: impl Fn(u64) -> io::Result<()> + Send + Sync + 'static,
) -> io::Result<(Hash, Option<Vec<u8>>)> {
    // compute outboard size so we can pre-allocate the buffer.
    let outboard_size = usize::try_from(raw_outboard_size(size))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "size too large"))?;
    let mut outboard = Vec::with_capacity(outboard_size);

    // wrap the reader in a progress reader, so we can report progress.
    let reader = ProgressReader2::new(read, progress);
    // wrap the reader in a buffered reader, so we read in large chunks
    // this reduces the number of io ops and also the number of progress reports
    let mut reader = BufReader::with_capacity(1024 * 1024, reader);

    let hash =
        bao_tree::io::sync::outboard_post_order(&mut reader, size, IROH_BLOCK_SIZE, &mut outboard)?;
    let ob = PostOrderMemOutboard::load(hash, &outboard, IROH_BLOCK_SIZE)?.flip();
    tracing::trace!(%hash, "done");
    let ob = ob.into_inner();
    let ob = if !ob.is_empty() { Some(ob) } else { None };
    Ok((hash.into(), ob))
}

pub(crate) struct ProgressReader2<R, F: Fn(u64) -> io::Result<()>> {
    inner: R,
    offset: u64,
    cb: F,
}

impl<R: io::Read, F: Fn(u64) -> io::Result<()>> ProgressReader2<R, F> {
    #[allow(dead_code)]
    pub fn new(inner: R, cb: F) -> Self {
        Self {
            inner,
            offset: 0,
            cb,
        }
    }
}

impl<R: io::Read, F: Fn(u64) -> io::Result<()>> io::Read for ProgressReader2<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.offset += read as u64;
        (self.cb)(self.offset)?;
        Ok(read)
    }
}

/// overwrite a file with the given data.
///
/// This is almost like `std::fs::write`, but it does not truncate the file.
///
/// So if you overwrite a file with less data than it had before, the file will
/// still have the same size as before.
///
/// Also, if you overwrite a file with the same data as it had before, the
/// file will be unchanged even if the overwrite operation is interrupted.
fn overwrite_and_sync(path: &Path, data: &[u8]) -> io::Result<std::fs::File> {
    let mut file = OpenOptions::new().write(true).create(true).open(&path)?;
    file.write_all(data)?;
    // todo: figure out the consequences of not syncing here
    file.sync_all()?;
    Ok(file)
}

/// Read a file into memory and then delete it.
fn read_and_remove(path: &Path) -> io::Result<Vec<u8>> {
    let data = std::fs::read(&path)?;
    // todo: should we fail here or just log a warning?
    // remove could fail e.g. on windows if the file is still open
    std::fs::remove_file(&path)?;
    Ok(data)
}

fn dump(db: &redb::Database) -> ActorResult<()> {
    let tx = db.begin_read()?;
    let blobs = tx.open_table(BLOBS_TABLE)?;
    let tags = tx.open_table(TAGS_TABLE)?;
    let inline_data = tx.open_table(INLINE_DATA_TABLE)?;
    let inline_outboard = tx.open_table(INLINE_OUTBOARD_TABLE)?;
    for e in blobs.iter()? {
        let (k, v) = e?;
        let k = k.value();
        let v = v.value();
        println!("blobs: {} -> {:?}", k.to_hex(), v);
    }
    for e in tags.iter()? {
        let (k, v) = e?;
        let k = k.value();
        let v = v.value();
        println!("tags: {} -> {:?}", k, v);
    }
    for e in inline_data.iter()? {
        let (k, v) = e?;
        let k = k.value();
        let v = v.value();
        println!("inline_data: {} -> {:?}", k.to_hex(), v.len());
    }
    for e in inline_outboard.iter()? {
        let (k, v) = e?;
        let k = k.value();
        let v = v.value();
        println!("inline_outboard: {} -> {:?}", k.to_hex(), v.len());
    }
    Ok(())
}

fn load_data(
    options: &Options,
    tx: &ReadTransaction,
    location: DataLocation<(), u64>,
    hash: &Hash,
) -> io::Result<MemOrFile<Bytes, (std::fs::File, u64)>> {
    Ok(match location {
        DataLocation::Inline(()) => {
            let data = tx.open_table(INLINE_DATA_TABLE).map_err(to_io_err)?;
            let Some(data) = data.get(hash).map_err(to_io_err)? else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "inconsistent database state: {} should have inline data but does not",
                        hash.to_hex()
                    ),
                ));
            };
            MemOrFile::Mem(Bytes::copy_from_slice(data.value()))
        }
        DataLocation::Owned(data_size) => {
            let path = options.owned_data_path(&hash);
            let Ok(file) = std::fs::File::open(&path) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("file not found: {}", path.display()),
                ));
            };
            MemOrFile::File((file, data_size))
        }
        DataLocation::External(_paths, _size) => {
            unimplemented!()
        }
    })
}

fn load_outboard(
    options: &Options,
    tx: &ReadTransaction,
    location: OutboardLocation,
    size: u64,
    hash: &Hash,
) -> io::Result<MemOrFile<Bytes, (std::fs::File, u64)>> {
    Ok(match location {
        OutboardLocation::NotNeeded => MemOrFile::Mem(Bytes::new()),
        OutboardLocation::Inline(_) => {
            let outboard = tx.open_table(INLINE_OUTBOARD_TABLE).map_err(to_io_err)?;
            let Some(outboard) = outboard.get(hash).map_err(to_io_err)? else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "inconsistent database state: {} should have inline outboard but does not",
                        hash.to_hex()
                    ),
                ));
            };
            MemOrFile::Mem(Bytes::copy_from_slice(outboard.value()))
        }
        OutboardLocation::Owned => {
            let outboard_size = raw_outboard_size(size);
            let path = options.owned_outboard_path(&hash);
            let Ok(file) = std::fs::File::open(&path) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("file not found: {} size={}", path.display(), outboard_size),
                ));
            };
            MemOrFile::File((file, outboard_size))
        }
    })
}

/// Take a possibly incomplete storage and turn it into complete
fn complete_storage(
    storage: BaoFileStorage,
    hash: &Hash,
    options: &Options,
) -> io::Result<std::result::Result<CompleteMemOrFileStorage, CompleteMemOrFileStorage>> {
    let (data, outboard, _sizes) = match storage {
        BaoFileStorage::Complete(c) => return Ok(Err(c)),
        BaoFileStorage::IncompleteMem(storage) => {
            let (data, outboard, sizes) = storage.into_parts();
            (
                MemOrFile::Mem(Bytes::from(data.into_parts().0)),
                MemOrFile::Mem(Bytes::from(outboard.into_parts().0)),
                MemOrFile::Mem(Bytes::from(sizes.to_vec()?)),
            )
        }
        BaoFileStorage::IncompleteFile(storage) => {
            let (data, outboard, sizes) = storage.into_parts();
            (
                MemOrFile::File(data),
                MemOrFile::File(outboard),
                MemOrFile::File(sizes),
            )
        }
    };
    let data_size = data.size()?.unwrap();
    let outboard_size = outboard.size()?.unwrap();
    // todo: perform more sanity checks if in debug mode
    debug_assert!(raw_outboard_size(data_size) == outboard_size);
    // inline data if needed, or write to file if needed
    let data = if data_size <= options.max_data_inlined {
        match data {
            MemOrFile::File(data) => {
                let mut buf = vec![0; data_size as usize];
                data.read_at(0, &mut buf)?;
                let path: PathBuf = options.owned_data_path(&hash);
                // this whole file removal thing is not great. It should either fail, or try
                // again until it works. Maybe have a set of stuff to delete and do it in gc?
                if let Err(cause) = std::fs::remove_file(path) {
                    tracing::error!("failed to remove file: {}", cause);
                };
                MemOrFile::Mem(Bytes::from(buf))
            }
            MemOrFile::Mem(data) => MemOrFile::Mem(data),
        }
    } else {
        match data {
            MemOrFile::Mem(data) => {
                let path = options.owned_data_path(&hash);
                let file = overwrite_and_sync(&path, &data)?;
                MemOrFile::File((file, data_size))
            }
            MemOrFile::File(data) => MemOrFile::File((data, data_size)),
        }
    };
    // inline outboard if needed, or write to file if needed
    let outboard = if outboard_size == 0 {
        Default::default()
    } else if outboard_size <= options.max_outboard_inlined {
        match outboard {
            MemOrFile::File(outboard) => {
                let mut buf = vec![0; outboard_size as usize];
                outboard.read_at(0, &mut buf)?;
                drop(outboard);
                let path: PathBuf = options.owned_outboard_path(&hash);
                // this whole file removal thing is not great. It should either fail, or try
                // again until it works. Maybe have a set of stuff to delete and do it in gc?
                if let Err(cause) = std::fs::remove_file(path) {
                    tracing::error!("failed to remove file: {}", cause);
                };
                MemOrFile::Mem(Bytes::from(buf))
            }
            MemOrFile::Mem(outboard) => MemOrFile::Mem(outboard),
        }
    } else {
        match outboard {
            MemOrFile::Mem(outboard) => {
                let path = options.owned_outboard_path(&hash);
                let file = overwrite_and_sync(&path, &outboard)?;
                MemOrFile::File((file, outboard_size))
            }
            MemOrFile::File(outboard) => MemOrFile::File((outboard, outboard_size)),
        }
    };
    Ok(Ok(CompleteMemOrFileStorage { data, outboard }))
}

#[derive(derive_more::Debug)]
pub(crate) enum RedbActorMessage {
    Get {
        hash: Hash,
        tx: oneshot::Sender<Option<BaoFileHandle>>,
    },
    DataLocation {
        hash: Hash,
        tx: oneshot::Sender<Option<MemOrFile<Bytes, (PathBuf, u64, bool)>>>,
    },
    GetOrCreate {
        hash: Hash,
        tx: oneshot::Sender<BaoFileHandle>,
    },
    OnInlineSizeExceeded {
        hash: Hash,
    },
    OnComplete {
        hash: Hash,
    },
    ImportEntry {
        hash: Hash,
        data_location: DataLocation<Bytes, u64>,
        outboard_location: OutboardLocation<Bytes>,
        tx: tokio::sync::oneshot::Sender<()>,
    },
    EntryStatus {
        hash: Hash,
        tx: tokio::sync::oneshot::Sender<EntryStatus>,
    },
    Blobs {
        tx: oneshot::Sender<Vec<io::Result<Hash>>>,
    },
    PartialBlobs {
        tx: oneshot::Sender<Vec<io::Result<Hash>>>,
    },
    Tags {
        tx: oneshot::Sender<Vec<io::Result<(Tag, HashAndFormat)>>>,
    },
    SetTag {
        tag: Tag,
        value: Option<HashAndFormat>,
        tx: oneshot::Sender<ActorResult<()>>,
    },
    CreateTag {
        hash: HashAndFormat,
        tx: oneshot::Sender<ActorResult<Tag>>,
    },
    Sync {
        tx: oneshot::Sender<()>,
    },
    Dump,
    Delete {
        hashes: Vec<Hash>,
        tx: oneshot::Sender<()>,
    },
    Shutdown,
}

///
#[derive(Debug, Clone)]
pub struct Store(Arc<StoreInner>);

impl Store {
    ///
    pub async fn load(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = root.as_ref();
        let db_path = path.join("meta").join("blobs.db");
        let options = Options {
            data_path: path.join("data"),
            temp_path: path.join("temp"),
            max_data_inlined: 1024 * 16,
            max_outboard_inlined: 1024 * 16,
            move_threshold: 1024 * 16,
        };
        Self::new(&db_path, options)
    }

    fn new(path: &Path, options: Options) -> io::Result<Self> {
        Ok(Self(Arc::new(StoreInner::new(path, options)?)))
    }

    async fn dump(&self) -> io::Result<()> {
        Ok(self.0.dump().await?)
    }

    async fn sync(&self) -> io::Result<()> {
        Ok(self.0.sync().await?)
    }
}

#[derive(Debug)]
struct StoreInner {
    tx: flume::Sender<RedbActorMessage>,
    state: Arc<Mutex<State2>>,
    handle: Option<std::thread::JoinHandle<()>>,
    options: Arc<Options>,
}

impl std::ops::Deref for StoreInner {
    type Target = Options;

    fn deref(&self) -> &Self::Target {
        &self.options
    }
}

#[derive(Debug, Default)]
struct State2 {
    temp: TempCounterMap,
    live: BTreeSet<Hash>,
}

impl LivenessTracker for Mutex<State2> {
    fn on_clone(&self, content: &HashAndFormat) {
        self.lock().unwrap().temp.inc(content);
    }

    fn on_drop(&self, content: &HashAndFormat) {
        self.lock().unwrap().temp.dec(content);
    }
}

impl StoreInner {
    pub fn new(path: &Path, options: Options) -> io::Result<Self> {
        std::fs::create_dir_all(&options.data_path)?;
        std::fs::create_dir_all(&options.temp_path)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let (actor, tx) = RedbActor::new(path, options.clone())?;
        let handle = std::thread::spawn(move || {
            if let Err(cause) = actor.run() {
                println!("redb actor failed: {}", cause);
            }
        });
        Ok(Self {
            tx,
            state: Default::default(),
            handle: Some(handle),
            options: Arc::new(options),
        })
    }

    pub async fn get(&self, hash: Hash) -> OuterResult<Option<BaoFileHandle>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::Get { hash, tx })
            .await?;
        Ok(rx.await?)
    }

    pub async fn data_location(
        &self,
        hash: Hash,
    ) -> OuterResult<Option<MemOrFile<Bytes, (PathBuf, u64, bool)>>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::DataLocation { hash, tx })
            .await?;
        Ok(rx.await?)
    }

    pub async fn get_or_create(&self, hash: Hash) -> OuterResult<BaoFileHandle> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::GetOrCreate { hash, tx })
            .await?;
        Ok(rx.await?)
    }

    pub async fn blobs(&self) -> OuterResult<Vec<io::Result<Hash>>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send_async(RedbActorMessage::Blobs { tx }).await?;
        Ok(rx.await?)
    }

    pub async fn partial_blobs(&self) -> OuterResult<Vec<io::Result<Hash>>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::PartialBlobs { tx })
            .await?;
        Ok(rx.await?)
    }

    pub async fn tags(&self) -> OuterResult<Vec<io::Result<(Tag, HashAndFormat)>>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send_async(RedbActorMessage::Tags { tx }).await?;
        Ok(rx.await?)
    }

    pub async fn set_tag(&self, tag: Tag, value: Option<HashAndFormat>) -> OuterResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::SetTag { tag, value, tx })
            .await?;
        Ok(rx.await??)
    }

    pub async fn create_tag(&self, hash: HashAndFormat) -> OuterResult<Tag> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::CreateTag { hash, tx })
            .await?;
        Ok(rx.await??)
    }

    pub async fn delete(&self, hashes: Vec<Hash>) -> OuterResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::Delete { hashes, tx })
            .await?;
        Ok(rx.await?)
    }

    pub async fn entry_status(&self, hash: &Hash) -> OuterResult<EntryStatus> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send_async(RedbActorMessage::EntryStatus { hash: *hash, tx })
            .await?;
        Ok(rx.await?)
    }

    pub fn entry_status_sync(&self, hash: &Hash) -> OuterResult<EntryStatus> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(RedbActorMessage::EntryStatus { hash: *hash, tx })?;
        Ok(rx.blocking_recv()?)
    }

    pub async fn complete(&self, hash: Hash) -> OuterResult<()> {
        self.tx
            .send_async(RedbActorMessage::OnComplete { hash })
            .await?;
        Ok(())
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        Ok(if let Some(handle) = self.handle.take() {
            self.tx.send_async(RedbActorMessage::Shutdown).await?;
            handle
                .join()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "redb actor thread panicked"))?
        })
    }

    pub async fn dump(&self) -> OuterResult<()> {
        self.tx.send_async(RedbActorMessage::Dump).await?;
        Ok(())
    }

    pub async fn sync(&self) -> OuterResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send_async(RedbActorMessage::Sync { tx }).await?;
        Ok(rx.await?)
    }

    pub fn temp_tag(&self, content: HashAndFormat) -> TempTag {
        TempTag::new(content, Some(self.state.clone()))
    }

    fn import_file_sync(
        &self,
        path: PathBuf,
        mode: ImportMode,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> io::Result<(TempTag, u64)> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must be absolute",
            ));
        }
        if !path.is_file() && !path.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a file or symlink",
            ));
        }
        let id = progress.new_id();
        progress.blocking_send(ImportProgress::Found {
            id,
            name: path.to_string_lossy().to_string(),
        })?;
        let file = match mode {
            ImportMode::TryReference => ImportFile::External(path),
            ImportMode::Copy => {
                let size = path.metadata()?.len();
                if size <= self.max_data_inlined {
                    let data = Bytes::from(std::fs::read(&path)?);
                    ImportFile::Memory(data)
                } else {
                    let temp_path = self.temp_file_path();
                    // copy the data, since it is not stable
                    progress.try_send(ImportProgress::CopyProgress { id, offset: 0 })?;
                    if reflink_copy::reflink_or_copy(&path, &temp_path)?.is_none() {
                        tracing::debug!("reflinked {} to {}", path.display(), temp_path.display());
                    } else {
                        tracing::debug!("copied {} to {}", path.display(), temp_path.display());
                    }
                    ImportFile::TempFile(temp_path)
                }
            }
        };
        let (tag, size) = self.finalize_import_sync(file, format, id, progress)?;
        Ok((tag, size))
    }

    fn import_bytes_sync(&self, data: Bytes, format: BlobFormat) -> io::Result<TempTag> {
        let id = 0;
        let file = ImportFile::Memory(data);
        let progress = IgnoreProgressSender::default();
        let (tag, _size) = self.finalize_import_sync(file, format, id, progress)?;
        Ok(tag)
    }

    fn finalize_import_sync(
        &self,
        file: ImportFile,
        format: BlobFormat,
        id: u64,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> io::Result<(TempTag, u64)> {
        let data_size = file.len()?;
        let outboard_size = raw_outboard_size(data_size);
        let inline_data = data_size <= self.max_data_inlined;
        let inline_outboard = outboard_size <= self.max_outboard_inlined && outboard_size != 0;
        tracing::info!("finalize_import_sync {:?} {}", file, data_size);
        progress.blocking_send(ImportProgress::Size {
            id,
            size: data_size,
        })?;
        let progress2 = progress.clone();
        let (hash, outboard) = match file.content() {
            MemOrFile::File(path) => {
                let span = trace_span!("outboard.compute", path = %path.display());
                let _guard = span.enter();
                let file = std::fs::File::open(&path)?;
                compute_outboard(file, data_size, move |offset| {
                    Ok(progress2.try_send(ImportProgress::OutboardProgress { id, offset })?)
                })?
            }
            MemOrFile::Mem(bytes) => {
                // todo: progress? usually this is will be small enough that progress might not be needed.
                compute_outboard(bytes, data_size, |_| Ok(()))?
            }
        };
        progress.blocking_send(ImportProgress::OutboardDone { id, hash })?;
        // from here on, everything related to the hash is protected by the temp tag
        let tag = self.temp_tag(HashAndFormat { hash, format });
        let hash = *tag.hash();
        // move the data file into place, or create a reference to it
        let data_location = match file {
            ImportFile::External(external_path) => {
                tracing::info!("stored external reference {}", external_path.display());
                if inline_data {
                    tracing::info!(
                        "reading external data to inline it: {}",
                        external_path.display()
                    );
                    let data = Bytes::from(std::fs::read(&external_path)?);
                    DataLocation::Inline(data)
                } else {
                    DataLocation::External(vec![external_path], data_size)
                }
            }
            ImportFile::TempFile(temp_data_path) => {
                if inline_data {
                    tracing::info!(
                        "reading and deleting temp file to inline it: {}",
                        temp_data_path.display()
                    );
                    let data = Bytes::from(read_and_remove(&temp_data_path)?);
                    DataLocation::Inline(data)
                } else {
                    let data_path = self.owned_data_path(&hash);
                    std::fs::rename(&temp_data_path, &data_path)?;
                    tracing::info!("created file {}", data_path.display());
                    DataLocation::Owned(data_size)
                }
            }
            ImportFile::Memory(data) => {
                if inline_data {
                    DataLocation::Inline(data)
                } else {
                    let data_path = self.owned_data_path(&hash);
                    overwrite_and_sync(&data_path, &data)?;
                    tracing::info!("created file {}", data_path.display());
                    DataLocation::Owned(data_size)
                }
            }
        };
        let outboard_location = if let Some(outboard) = outboard {
            if inline_outboard {
                OutboardLocation::Inline(outboard.into())
            } else {
                let outboard_path = self.owned_outboard_path(&hash);
                overwrite_and_sync(&outboard_path, &outboard)?;
                OutboardLocation::Owned
            }
        } else {
            OutboardLocation::NotNeeded
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        // blocking send for the import
        self.tx
            .send(RedbActorMessage::ImportEntry {
                hash,
                tx,
                data_location,
                outboard_location,
            })
            .unwrap();
        rx.blocking_recv().unwrap();
        Ok((tag, data_size))
    }

    fn temp_file_path(&self) -> PathBuf {
        self.temp_path.join(temp_name())
    }
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.tx.send(RedbActorMessage::Shutdown).ok();
            handle.join().ok();
        }
    }
}

struct RedbActor {
    db: redb::Database,
    state: BTreeMap<Hash, BaoFileHandle>,
    msgs: flume::Receiver<RedbActorMessage>,
    options: Options,
    create_options: Arc<BaoFileConfig>,
}

impl RedbActor {
    fn recv_batch(&self, n: usize) -> (Vec<RedbActorMessage>, bool) {
        let mut res = Vec::new();
        match self.msgs.recv() {
            Ok(msg) => res.push(msg),
            Err(flume::RecvError::Disconnected) => return (res, true),
        }
        let mut done = false;
        for _ in 1..n {
            if let Ok(msg) = self.msgs.try_recv() {
                res.push(msg);
            } else {
                done = true;
                break;
            }
        }
        (res, done)
    }
}

/// Error type for message handler functions of the redb actor.
///
/// What can go wrong are various things with redb, as well as io errors related
/// to files other than redb.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ActorError {
    #[error("table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

impl From<ActorError> for io::Error {
    fn from(e: ActorError) -> Self {
        match e {
            ActorError::Io(e) => e,
            e @ _ => io::Error::new(io::ErrorKind::Other, e),
        }
    }
}

/// Result type for handler functions of the redb actor.
///
/// See [`ActorError`] for what can go wrong.
pub(crate) type ActorResult<T> = std::result::Result<T, ActorError>;

/// Error type for calling the redb actor from the store.
///
/// What can go wrong is all the things in [`ActorError`] and in addition
/// sending and receiving messages.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OuterError {
    #[error("inner error: {0}")]
    Inner(#[from] ActorError),
    #[error("send error: {0}")]
    Flume(#[from] flume::SendError<RedbActorMessage>),
    #[error("recv error: {0}")]
    Recv(#[from] oneshot::Canceled),
    #[error("recv error: {0}")]
    Recv2(#[from] tokio::sync::oneshot::error::RecvError),
    #[error("join error: {0}")]
    JoinTask(#[from] tokio::task::JoinError),
}

/// Result type for calling the redb actor from the store.
///
/// See [`OuterError`] for what can go wrong.
pub(crate) type OuterResult<T> = std::result::Result<T, OuterError>;

impl From<OuterError> for io::Error {
    fn from(e: OuterError) -> Self {
        match e {
            OuterError::Inner(ActorError::Io(e)) => e,
            e @ _ => io::Error::new(io::ErrorKind::Other, e),
        }
    }
}

impl crate::store::traits::Map for Store {
    type Entry = Entry;

    async fn get(&self, hash: &Hash) -> io::Result<Option<Self::Entry>> {
        Ok(self.0.get(*hash).await?.map(From::from))
    }
}

impl crate::store::traits::MapMut for Store {
    type EntryMut = Entry;

    async fn get_or_create(&self, hash: Hash, _size: u64) -> io::Result<Self::EntryMut> {
        Ok(self.0.get_or_create(hash).await?.into())
    }

    async fn entry_status(&self, hash: &Hash) -> io::Result<EntryStatus> {
        Ok(self.0.entry_status(hash).await?)
    }

    async fn get_possibly_partial(
        &self,
        hash: &Hash,
    ) -> io::Result<super::PossiblyPartialEntry<Self>> {
        match self.0.get(*hash).await? {
            Some(entry) => Ok({
                if entry.is_complete() {
                    super::PossiblyPartialEntry::Complete(entry.into())
                } else {
                    super::PossiblyPartialEntry::Partial(entry.into())
                }
            }),
            None => Ok(super::PossiblyPartialEntry::NotFound),
        }
    }

    async fn insert_complete(&self, entry: Self::EntryMut) -> io::Result<()> {
        Ok(self.0.complete(entry.hash()).await?)
    }

    fn entry_status_sync(&self, hash: &Hash) -> io::Result<EntryStatus> {
        Ok(self.0.entry_status_sync(hash)?)
    }
}

impl ReadableStore for Store {
    async fn blobs(&self) -> io::Result<super::DbIter<Hash>> {
        Ok(Box::new(self.0.blobs().await?.into_iter()))
    }

    async fn partial_blobs(&self) -> io::Result<super::DbIter<Hash>> {
        Ok(Box::new(self.0.partial_blobs().await?.into_iter()))
    }

    async fn tags(&self) -> io::Result<super::DbIter<(Tag, HashAndFormat)>> {
        Ok(Box::new(self.0.tags().await?.into_iter()))
    }

    fn temp_tags(&self) -> Box<dyn Iterator<Item = HashAndFormat> + Send + Sync + 'static> {
        Box::new(self.0.state.lock().unwrap().temp.keys())
    }

    async fn validate(
        &self,
        tx: tokio::sync::mpsc::Sender<super::ValidateProgress>,
    ) -> io::Result<()> {
        self.0.dump().await?;
        Ok(())
    }

    async fn export(
        &self,
        hash: Hash,
        target: PathBuf,
        mode: ExportMode,
        progress: impl Fn(u64) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<()> {
        let tt = self.0.temp_tag(HashAndFormat::raw(hash));
        let Some(source) = self.0.data_location(hash).await? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("hash not found: {}", hash.to_hex()),
            ));
        };
        let options = self.0.options.clone();
        tokio::task::spawn_blocking(move || {
            tracing::trace!("exporting {} to {} ({:?})", hash, target.display(), mode);

            if !target.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "target path must be absolute",
                ));
            }
            let parent = target.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "target path has no parent directory",
                )
            })?;
            // create the directory in which the target file is
            std::fs::create_dir_all(parent)?;
            let stable = mode == ExportMode::TryReference;
            match source {
                MemOrFile::Mem(data) => {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)?;
                    file.write_all(&data)?;
                }
                MemOrFile::File((source, size, owned)) => {
                    // todo
                    let owned = false;
                    if size >= options.move_threshold && stable && owned {
                        tracing::debug!("moving {} to {}", source.display(), target.display());
                        // we need to atomically move the file to the new location and update the redb entry.
                        // we can't do this here! That's why owned is set to false for now.
                        std::fs::rename(source, &target)?;
                    } else {
                        tracing::debug!("copying {} to {}", source.display(), target.display());
                        progress(0)?;
                        // todo: progress? not needed if the file is small
                        if reflink_copy::reflink_or_copy(&source, &target)?.is_none() {
                            tracing::debug!(
                                "reflinked {} to {}",
                                source.display(),
                                target.display()
                            );
                        } else {
                            tracing::debug!("copied {} to {}", source.display(), target.display());
                        }
                        progress(size)?;
                        // todo: should we add the new location to the entry if it was already non-owned?
                    }
                }
            };
            Ok(())
        })
        .await??;
        drop(tt);
        Ok(())
    }
}

impl crate::store::traits::Store for Store {
    async fn import_file(
        &self,
        path: PathBuf,
        mode: ImportMode,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> io::Result<(crate::TempTag, u64)> {
        let this = self.0.clone();
        tokio::task::spawn_blocking(move || this.import_file_sync(path, mode, format, progress))
            .map(flatten_to_io)
            .await
    }

    async fn import_bytes(
        &self,
        data: bytes::Bytes,
        format: iroh_base::hash::BlobFormat,
    ) -> io::Result<crate::TempTag> {
        let this = self.0.clone();
        tokio::task::spawn_blocking(move || this.import_bytes_sync(data, format))
            .map(flatten_to_io)
            .await
    }

    async fn import_stream(
        &self,
        mut data: impl Stream<Item = io::Result<Bytes>> + Unpin + Send + 'static,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> io::Result<(TempTag, u64)> {
        let this = self.clone();
        let id = progress.new_id();
        // write to a temp file
        let temp_data_path = this.0.temp_file_path();
        let name = temp_data_path
            .file_name()
            .expect("just created")
            .to_string_lossy()
            .to_string();
        progress.send(ImportProgress::Found { id, name }).await?;
        let mut writer = tokio::fs::File::create(&temp_data_path).await?;
        let mut offset = 0;
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            writer.write_all(&chunk).await?;
            offset += chunk.len() as u64;
            progress.try_send(ImportProgress::CopyProgress { id, offset })?;
        }
        writer.flush().await?;
        drop(writer);
        let file = ImportFile::TempFile(temp_data_path);
        tokio::task::spawn_blocking(move || this.0.finalize_import_sync(file, format, id, progress))
            .map(flatten_to_io)
            .await
    }

    async fn set_tag(&self, name: Tag, hash: Option<HashAndFormat>) -> io::Result<()> {
        Ok(self.0.set_tag(name, hash).await?)
    }

    async fn create_tag(&self, hash: HashAndFormat) -> io::Result<Tag> {
        Ok(self.0.create_tag(hash).await?)
    }

    async fn delete(&self, hashes: Vec<Hash>) -> io::Result<()> {
        Ok(self.0.delete(hashes).await?)
    }

    fn temp_tag(&self, value: HashAndFormat) -> TempTag {
        self.0.temp_tag(value)
    }

    async fn clear_live(&self) {
        self.0.state.lock().unwrap().live.clear();
    }

    fn add_live(&self, live: impl IntoIterator<Item = Hash>) -> impl Future<Output = ()> + Send {
        self.0.state.lock().unwrap().live.extend(live);
        futures::future::ready(())
    }

    fn is_live(&self, hash: &Hash) -> bool {
        let state = self.0.state.lock().unwrap();
        state.live.contains(hash) || state.temp.contains(hash)
    }
}

impl RedbActor {
    fn new(path: &Path, options: Options) -> ActorResult<(Self, flume::Sender<RedbActorMessage>)> {
        let db = redb::Database::create(path)?;
        let tx = db.begin_write()?;
        {
            let _blobs = tx.open_table(BLOBS_TABLE)?;
            let _inline_data = tx.open_table(INLINE_DATA_TABLE)?;
            let _inline_outboard = tx.open_table(INLINE_OUTBOARD_TABLE)?;
            let _tags = tx.open_table(TAGS_TABLE)?;
        }
        tx.commit()?;
        let (tx, rx) = flume::unbounded();
        let tx2 = tx.clone();
        let create_options = BaoFileConfig::new(
            Arc::new(options.data_path.clone()),
            16 * 1024,
            Some(Arc::new(move |hash| {
                // todo: make the callback allow async
                tx2.send(RedbActorMessage::OnInlineSizeExceeded {
                    hash: (*hash).into(),
                })
                .ok();
                Ok(())
            })),
        );
        Ok((
            Self {
                db,
                state: BTreeMap::new(),
                msgs: rx,
                options,
                create_options: Arc::new(create_options),
            },
            tx,
        ))
    }

    fn entry_status(&mut self, hash: Hash) -> ActorResult<EntryStatus> {
        if let Some(entry) = self.state.get(&hash) {
            return Ok(if entry.is_complete() {
                EntryStatus::Complete
            } else {
                EntryStatus::Partial
            });
        }
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let entry = blobs.get(hash)?;
        Ok(if let Some(entry) = entry {
            if entry.value().complete() {
                EntryStatus::Complete
            } else {
                EntryStatus::Partial
            }
        } else {
            EntryStatus::NotFound
        })
    }

    fn get(&mut self, hash: Hash) -> ActorResult<Option<BaoFileHandle>> {
        if let Some(entry) = self.state.get(&hash) {
            return Ok(Some(entry.clone()));
        }
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let Some(entry) = blobs.get(hash)? else {
            tracing::debug!("redb get not found {}", hash.to_hex());
            return Ok(None);
        };
        // todo: if complete, load inline data and/or outboard into memory if needed,
        // and return a complete entry.
        let entry = entry.value();
        let config = self.create_options.clone();
        let handle = match entry {
            EntryState::Complete {
                data_location,
                outboard_location,
            } => {
                let data = load_data(&self.options, &tx, data_location, &hash)?;
                let outboard =
                    load_outboard(&self.options, &tx, outboard_location, data.size(), &hash)?;
                BaoFileHandle::new_complete(config, hash.into(), data, outboard)
            }
            EntryState::Partial { .. } => BaoFileHandle::new_partial(config, hash.into())?,
        };
        self.state.insert(hash, handle.clone());
        Ok(Some(handle))
    }

    fn data_location(
        &mut self,
        hash: Hash,
    ) -> ActorResult<Option<MemOrFile<Bytes, (PathBuf, u64, bool)>>> {
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let data = tx.open_table(INLINE_DATA_TABLE)?;
        let Some(entry) = blobs.get(hash)? else {
            return Ok(None);
        };
        Ok(match entry.value() {
            EntryState::Complete { data_location, .. } => match data_location {
                DataLocation::Inline(()) => data
                    .get(hash)?
                    .map(|x| MemOrFile::Mem(Bytes::copy_from_slice(x.value()))),
                DataLocation::External(paths, size) => {
                    Some(MemOrFile::File((paths[0].clone(), size, false)))
                }
                DataLocation::Owned(size) => Some(MemOrFile::File((
                    self.options.owned_data_path(&hash),
                    size,
                    true,
                ))),
            },
            EntryState::Partial { size } => {
                // todo: return partial data here as well?
                None
            }
        })
    }

    fn import_entry(
        &mut self,
        hash: Hash,
        data_location: DataLocation<Bytes, u64>,
        outboard_location: OutboardLocation<Bytes>,
    ) -> ActorResult<()> {
        let tx = self.db.begin_write()?;
        {
            let mut blobs = tx.open_table(BLOBS_TABLE)?;
            let mut inline_data = tx.open_table(INLINE_DATA_TABLE)?;
            let mut inline_outboard = tx.open_table(INLINE_OUTBOARD_TABLE)?;
            if let DataLocation::Inline(data) = &data_location {
                inline_data.insert(hash, data.as_ref())?;
            }
            if let OutboardLocation::Inline(outboard) = &outboard_location {
                inline_outboard.insert(hash, outboard.as_ref())?;
            }
            let entry = blobs.get(hash)?;
            let entry = entry.map(|x| x.value()).unwrap_or_default();
            let entry = entry.union(EntryState::Complete {
                data_location: data_location.discard_inline_data(),
                outboard_location: outboard_location.discard_extra_data(),
            })?;
            blobs.insert(hash, entry)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get_or_create(&mut self, hash: Hash) -> ActorResult<BaoFileHandle> {
        if let Some(entry) = self.state.get(&hash) {
            return Ok(entry.clone());
        }
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let entry = blobs.get(hash)?;
        let handle = if let Some(entry) = entry {
            let entry = entry.value();
            match entry {
                EntryState::Complete {
                    data_location,
                    outboard_location,
                    ..
                } => {
                    let data = load_data(&self.options, &tx, data_location, &hash)?;
                    let outboard =
                        load_outboard(&self.options, &tx, outboard_location, data.size(), &hash)?;
                    println!("creating complete entry for {}", hash.to_hex());
                    BaoFileHandle::new_complete(self.create_options.clone(), hash, data, outboard)
                }
                EntryState::Partial { .. } => {
                    println!("creating partial entry for {}", hash.to_hex());
                    BaoFileHandle::new_partial(self.create_options.clone(), hash)?
                }
            }
        } else {
            BaoFileHandle::new_mem(self.create_options.clone(), hash)
        };
        self.state.insert(hash, handle.clone());
        Ok(handle)
    }

    fn blobs(&mut self) -> ActorResult<Vec<io::Result<Hash>>> {
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let mut res = Vec::new();
        for blob in blobs.iter()? {
            match blob {
                Ok((k, v)) => {
                    if v.value().complete() {
                        res.push(Ok(k.value()));
                    }
                }
                Err(e) => res.push(Err(ActorError::from(e).into())),
            }
        }
        Ok(res)
    }

    fn partial_blobs(&mut self) -> ActorResult<Vec<io::Result<Hash>>> {
        let tx = self.db.begin_read()?;
        let blobs = tx.open_table(BLOBS_TABLE)?;
        let mut res = Vec::new();
        for blob in blobs.iter()? {
            match blob {
                Ok((k, v)) => {
                    if !v.value().complete() {
                        res.push(Ok(k.value()));
                    }
                }
                Err(e) => res.push(Err(ActorError::from(e).into())),
            }
        }
        Ok(res)
    }

    fn tags(&mut self) -> ActorResult<Vec<io::Result<(Tag, HashAndFormat)>>> {
        let tx = self.db.begin_read()?;
        let tags = tx.open_table(TAGS_TABLE)?;
        let mut res = Vec::new();
        for tag in tags.iter()? {
            match tag {
                Ok((k, v)) => {
                    let tag = k.value();
                    let hash = v.value();
                    res.push(Ok((tag, hash)));
                }
                Err(e) => res.push(Err(ActorError::from(e).into())),
            }
        }
        Ok(res)
    }

    fn create_tag(&mut self, content: HashAndFormat) -> ActorResult<Tag> {
        let tx = self.db.begin_write()?;
        let tag = {
            let mut tags = tx.open_table(TAGS_TABLE)?;
            let tag = Tag::auto(SystemTime::now(), |x| {
                match tags.get(Tag(Bytes::copy_from_slice(x))) {
                    Ok(Some(_)) => true,
                    _ => false,
                }
            });
            tags.insert(tag.clone(), content)?;
            tag
        };
        tx.commit()?;
        Ok(tag)
    }

    fn set_tag(&mut self, tag: Tag, value: Option<HashAndFormat>) -> ActorResult<()> {
        let tx = self.db.begin_write()?;
        {
            let mut tags = tx.open_table(TAGS_TABLE)?;
            match value {
                Some(value) => {
                    tags.insert(tag, value)?;
                }
                None => {
                    tags.remove(tag)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn on_inline_size_exceeded(&mut self, hash: Hash) -> ActorResult<()> {
        let tx = self.db.begin_write()?;
        {
            let mut blobs = tx.open_table(BLOBS_TABLE)?;
            let entry = blobs.get(hash)?.map(|x| x.value()).unwrap_or_default();
            let entry = entry.union(EntryState::Partial { size: None })?;
            blobs.insert(hash, entry)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn delete(&mut self, hashes: Vec<Hash>) -> ActorResult<()> {
        let tx = self.db.begin_write()?;
        {
            let mut blobs = tx.open_table(BLOBS_TABLE)?;
            let mut inline_data = tx.open_table(INLINE_DATA_TABLE)?;
            let mut inline_outboard = tx.open_table(INLINE_OUTBOARD_TABLE)?;
            for hash in hashes {
                if let Some(entry) = blobs.remove(hash)? {
                    if let EntryState::Complete {
                        data_location,
                        outboard_location,
                        ..
                    } = entry.value()
                    {
                        match data_location {
                            DataLocation::Inline(_) => {
                                inline_data.remove(hash)?;
                            }
                            DataLocation::Owned(_) => {
                                let path = self.options.owned_data_path(&hash);
                                if let Err(cause) = std::fs::remove_file(&path) {
                                    tracing::error!("failed to remove file: {}", cause);
                                };
                            }
                            DataLocation::External(_, _) => {}
                        }
                        match outboard_location {
                            OutboardLocation::Inline(_) => {
                                inline_outboard.remove(hash)?;
                            }
                            OutboardLocation::Owned => {
                                let path = self.options.owned_outboard_path(&hash);
                                if let Err(cause) = std::fs::remove_file(&path) {
                                    tracing::error!("failed to remove file: {}", cause);
                                };
                            }
                            OutboardLocation::NotNeeded => {}
                        }
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn on_complete(&mut self, hash: Hash) -> ActorResult<()> {
        println!("on_complete({})", hash.to_hex());
        let Some(entry) = self.state.get(&hash) else {
            println!("entry does not exist");
            return Ok(());
        };
        let mut info = None;
        entry.transform(|state| {
            println!("on_complete transform {:?}", state);
            let entry = match complete_storage(state, &hash, &self.options)? {
                Ok(entry) => {
                    // store the info so we can insert it into the db later
                    info = Some((
                        entry.data_size(),
                        entry.data.mem().cloned(),
                        entry.outboard_size(),
                        entry.outboard.mem().cloned(),
                    ));
                    entry
                }
                Err(entry) => {
                    // the entry was already complete, nothing to do
                    entry
                }
            };
            Ok(BaoFileStorage::Complete(entry))
        })?;
        if let Some((data_size, data, outboard_size, outboard)) = info {
            let data_location = if data.is_some() {
                DataLocation::Inline(())
            } else {
                DataLocation::Owned(data_size)
            };
            let outboard_location = if outboard_size == 0 {
                OutboardLocation::NotNeeded
            } else if outboard.is_some() {
                OutboardLocation::Inline(())
            } else {
                OutboardLocation::Owned
            };
            // todo: just mark the entry for batch write if it is a mem entry?
            let tx = self.db.begin_write()?;
            {
                let mut blobs = tx.open_table(BLOBS_TABLE)?;
                tracing::info!(
                    "inserting complete entry for {}, {} bytes",
                    hash.to_hex(),
                    data_size,
                );
                blobs.insert(
                    hash,
                    EntryState::Complete {
                        data_location,
                        outboard_location,
                    },
                )?;
                if let Some(data) = data {
                    let mut inline_data = tx.open_table(INLINE_DATA_TABLE)?;
                    inline_data.insert(hash, data.as_ref())?;
                }
                if let Some(outboard) = outboard {
                    let mut inline_outboard = tx.open_table(INLINE_OUTBOARD_TABLE)?;
                    inline_outboard.insert(hash, outboard.as_ref())?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn run(mut self) -> ActorResult<()> {
        loop {
            println!("calling recv");
            match self.msgs.recv() {
                Ok(msg) => {
                    println!("{:?}", msg);
                    match msg {
                        RedbActorMessage::GetOrCreate { hash, tx } => {
                            tx.send(self.get_or_create(hash)?).ok();
                        }
                        RedbActorMessage::ImportEntry {
                            hash,
                            data_location,
                            outboard_location,
                            tx,
                        } => {
                            tx.send(self.import_entry(hash, data_location, outboard_location)?)
                                .ok();
                        }
                        RedbActorMessage::Get { hash, tx } => {
                            tx.send(self.get(hash)?).ok();
                        }
                        RedbActorMessage::DataLocation { hash, tx } => {
                            tx.send(self.data_location(hash)?).ok();
                        }
                        RedbActorMessage::EntryStatus { hash, tx } => {
                            tx.send(self.entry_status(hash)?).ok();
                        }
                        RedbActorMessage::Blobs { tx } => {
                            tx.send(self.blobs()?).ok();
                        }
                        RedbActorMessage::PartialBlobs { tx } => {
                            tx.send(self.blobs()?).ok();
                        }
                        RedbActorMessage::Tags { tx } => {
                            tx.send(self.tags()?).ok();
                        }
                        RedbActorMessage::CreateTag { hash, tx } => {
                            tx.send(self.create_tag(hash)).ok();
                        }
                        RedbActorMessage::SetTag { tag, value, tx } => {
                            tx.send(self.set_tag(tag, value)).ok();
                        }
                        RedbActorMessage::OnInlineSizeExceeded { hash } => {
                            self.on_inline_size_exceeded(hash)?;
                        }
                        RedbActorMessage::OnComplete { hash } => {
                            self.on_complete(hash)?;
                        }
                        RedbActorMessage::Dump => {
                            dump(&self.db)?;
                        }
                        RedbActorMessage::Sync { tx } => {
                            tx.send(()).ok();
                        }
                        RedbActorMessage::Delete { hashes, tx } => {
                            self.delete(hashes)?;
                            tx.send(()).ok();
                        }
                        RedbActorMessage::Shutdown => {
                            break;
                        }
                    }
                }
                Err(flume::RecvError::Disconnected) => {
                    break;
                }
            }
        }
        println!("redb actor done");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::store::bao_file::test_support::{
        decode_response_into_batch, make_wire_data, random_test_data, validate,
    };

    use crate::store::{MapEntryMut, MapMut};

    use super::*;

    #[tokio::test]
    async fn actor_store_smoke() {
        let testdir = tempfile::tempdir().unwrap();
        let db_path = testdir.path().join("test.redb");
        let temp_path = testdir.path().join("temp");
        let data_path = testdir.path().join("data");
        let options = Options {
            data_path,
            temp_path,
            max_data_inlined: 1024 * 16,
            max_outboard_inlined: 1024 * 16,
            move_threshold: 1024 * 16,
        };
        let db = Store::new(&db_path, options).unwrap();
        db.dump().await.unwrap();
        let data = random_test_data(1024 * 1024);
        let ranges = [0..data.len() as u64];
        let (hash, chunk_ranges, wire_data) = make_wire_data(&data, &ranges);
        let handle = db.get_or_create(hash, 0).await.unwrap();
        decode_response_into_batch(
            hash,
            IROH_BLOCK_SIZE,
            chunk_ranges.clone(),
            Cursor::new(wire_data),
            handle.batch_writer().await.unwrap(),
        )
        .await
        .unwrap();
        validate(&handle.0, &data, &ranges).await;
        db.insert_complete(handle).await.unwrap();
        db.sync().await.unwrap();
        db.dump().await.unwrap();
    }
}
