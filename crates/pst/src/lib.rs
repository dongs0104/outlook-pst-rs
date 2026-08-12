#![doc = include_str!("../README.md")]

use byteorder::WriteBytesExt;
use std::{
    cell::RefMut,
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    mem,
    path::Path,
    rc::Rc,
    sync::Mutex,
};
use thiserror::Error;
use tracing::{error, instrument, warn};

pub mod ltp;
pub mod messaging;
pub mod ndb;

mod block_sig;
mod crc;
mod encode;

use ltp::{
    heap::*, prop_context::*, prop_type::PropertyType, read_write::*, table_context::*, tree::*,
};
use messaging::read_write::SearchReadWrite;
use messaging::{folder::*, message::*, named_prop::*, search::*, store::*};
use ndb::{
    block::*, block_id::*, block_ref::*, byte_index::*, header::*, node_id::*, page::*,
    read_write::*, root::*, *,
};

#[derive(Error, Debug)]
pub enum PstError {
    #[error("Opened read-only")]
    OpenedReadOnly,
    #[error("Cannot write to file: {0}")]
    NoWriteAccess(String),
    #[error("I/O error: {0:?}")]
    Io(#[from] io::Error),
    #[error("I/O error: {0}")]
    BorrowedIo(String),
    #[error("Failed to lock file")]
    LockError,
    #[error("Integer conversion failed")]
    IntegerConversion,
    #[error("Node Database error: {0}")]
    NodeDatabaseError(#[from] NdbError),
    #[error("AllocationMapPage not found: {0}")]
    AllocationMapPageNotFound(usize),
    #[error("Invalid BTree page: offset: 0x{0:X}")]
    InvalidBTreePage(u64),
}

impl From<&PstError> for io::Error {
    fn from(err: &PstError) -> Self {
        match err {
            PstError::NoWriteAccess(path) => {
                Self::new(io::ErrorKind::PermissionDenied, path.as_str())
            }
            err => Self::other(format!("{err:?}")),
        }
    }
}

impl From<PstError> for io::Error {
    fn from(err: PstError) -> Self {
        match err {
            PstError::NoWriteAccess(path) => {
                Self::new(io::ErrorKind::PermissionDenied, path.as_str())
            }
            PstError::Io(err) => err,
            err => Self::other(err),
        }
    }
}

impl From<&io::Error> for PstError {
    fn from(err: &io::Error) -> Self {
        Self::BorrowedIo(format!("{err:?}"))
    }
}

type PstResult<T> = std::result::Result<T, PstError>;

/// The methods on this trait and the [`PstFileInner`] struct are not public, PST modifications
/// have to go through `pub fn` methods on the [`PstFileLockGuard`] type which encapsulates a `dyn`
/// reference to this trait.
trait PstFileLock<Pst>
where
    Pst: PstFile,
{
    fn start_write(&mut self) -> io::Result<()>;
    fn finish_write(&mut self) -> io::Result<()>;

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Pst as PstFile>::BlockBTree>>;
    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Pst as PstFile>::NodeBTree>>;
}

/// This is the public interface for writing to a PST.
pub struct PstFileLockGuard<'a, Pst>
where
    Pst: PstFile,
{
    pst: &'a mut dyn PstFileLock<Pst>,
}

impl<'a, Pst> PstFileLockGuard<'a, Pst>
where
    Pst: PstFile,
{
    fn new(pst: &'a mut dyn PstFileLock<Pst>) -> io::Result<Self> {
        pst.start_write()?;
        Ok(Self { pst })
    }

    /// Explicitly flush pending updates to the PST file. This will still happen implicitly when
    /// the [`PstFileLockGuard`] is dropped, but this allows you to handle errors.
    #[instrument(skip_all)]
    pub fn flush(&mut self) -> io::Result<()> {
        self.pst.finish_write().inspect_err(|err| {
            error!(
                name: "PstFinishWriteFailed",
                ?err,
                "PstFileLock::finish_write failed"
            );
        })?;

        Ok(())
    }
}

impl<Pst> Drop for PstFileLockGuard<'_, Pst>
where
    Pst: PstFile,
{
    #[instrument(skip_all)]
    fn drop(&mut self) {
        if let Err(err) = self.flush() {
            error!(
                name: "PstFileLockGuardFlushFailed",
                ?err,
                "Writing to the PST file failed"
            );
        }
    }
}

pub trait PstReader: Read + Seek {}

impl<T> PstReader for T where T: Read + Seek {}

/// [PST File](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/6b57253b-0853-47bb-99bb-d4b8f78105f0)
pub trait PstFile: Sized {
    type BlockId: BlockId<Index = Self::BTreeKey> + BlockIdReadWrite;
    type PageId: BlockId<Index = Self::BTreeKey> + BlockIdReadWrite;
    type ByteIndex: ByteIndex + ByteIndexReadWrite;
    type BlockRef: BlockRef<Block = Self::BlockId, Index = Self::ByteIndex> + BlockRefReadWrite;
    type PageRef: BlockRef<Block = Self::PageId, Index = Self::ByteIndex> + BlockRefReadWrite;
    type Root: Root<Self>;
    type Header: Header<Self>;
    type PageTrailer: PageTrailer<BlockId = Self::PageId> + PageTrailerReadWrite;
    type BTreeKey: BTreeEntryKey;
    type NodeBTreeEntry: NodeBTreeEntry<Block = Self::BlockId> + BTreeEntry<Key = Self::BTreeKey>;
    type NodeBTree: NodeBTree<Self, Self::NodeBTreeEntry>;
    type BlockBTreeEntry: BlockBTreeEntry<Block = Self::BlockRef> + BTreeEntry<Key = Self::BTreeKey>;
    type BlockBTree: BlockBTree<Self, Self::BlockBTreeEntry>;
    type BlockTrailer: BlockTrailer<BlockId = Self::BlockId>;
    type AllocationMapPage: AllocationMapPage<Self>;
    type AllocationPageMapPage: AllocationPageMapPage<Self>;
    type FreeMapPage: FreeMapPage<Self>;
    type FreePageMapPage: FreePageMapPage<Self>;
    type DensityListPage: DensityListPage<Self>;
    type DataTreeEntry: IntermediateTreeEntry + IntermediateDataTreeEntry<Self>;
    type DataTreeBlock: IntermediateTreeBlock<
        Header = DataTreeBlockHeader,
        Entry = Self::DataTreeEntry,
        Trailer = Self::BlockTrailer,
    >;
    type DataBlock: Block<Trailer = Self::BlockTrailer>;
    type SubNodeTreeBlockHeader: IntermediateTreeHeader;
    type SubNodeTreeBlock: IntermediateTreeBlock<
        Header = Self::SubNodeTreeBlockHeader,
        Entry = IntermediateSubNodeTreeEntry<Self::BlockId>,
        Trailer = Self::BlockTrailer,
    >;
    type SubNodeBlock: IntermediateTreeBlock<
        Header = Self::SubNodeTreeBlockHeader,
        Entry = LeafSubNodeTreeEntry<Self::BlockId>,
        Trailer = Self::BlockTrailer,
    >;
    type TableContext: TableContext;
    type PropertyContext: PropertyContext;
    type HeapNode: HeapNode;
    type PropertyTree: HeapTree<Key = PropertyTreeRecordKey, Value = PropertyTreeRecordValue>;
    type Store: Store;
    type Folder: Folder;
    type Message: Message;
    type NamedPropertyMap: NamedPropertyMap;
    type SearchUpdateQueue: SearchUpdateQueue;

    fn header(&self) -> &Self::Header;
    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error>;
    fn reader(&self) -> &Mutex<Box<dyn PstReader>>;
    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>>;

    fn read_node(&self, node: NodeId) -> io::Result<Self::NodeBTreeEntry>;
    fn read_block(&self, block: Self::BlockId) -> io::Result<Vec<u8>>;
}

struct PstFileInner<Pst>
where
    Pst: PstFile,
{
    reader: Mutex<Box<dyn PstReader>>,
    writer: PstResult<Mutex<BufWriter<File>>>,
    header: Pst::Header,
    density_list: io::Result<Pst::DensityListPage>,
    node_cache: NodeBTreePageCache<Pst>,
    block_cache: BlockBTreePageCache<Pst>,
}

pub struct UnicodePstFile {
    inner: PstFileInner<Self>,
}

/// A recipient type written to `PR_RECIPIENT_TYPE`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnicodePstRecipientType {
    To = 1,
    Cc = 2,
    Bcc = 3,
}

/// A message recipient for the Unicode PST writer.
#[derive(Clone, Copy)]
pub struct UnicodePstRecipient<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub recipient_type: UnicodePstRecipientType,
}

/// Message input for the Unicode PST writer.
#[derive(Clone, Copy)]
pub struct UnicodePstMessage<'a> {
    pub subject: &'a str,
    pub sender_name: &'a str,
    pub sender_email: &'a str,
    pub recipients: &'a [UnicodePstRecipient<'a>],
    pub body: &'a str,
    /// UTF-8 HTML for `PR_HTML`; `body` remains the plain-text fallback.
    pub html_body: Option<&'a str>,
    pub message_id: &'a str,
    /// Number of 100-nanosecond intervals since 1601-01-01 UTC.
    pub delivery_time: i64,
}

/// A by-value file attachment for the Unicode PST writer.
pub struct UnicodePstAttachment<'a> {
    /// File name shown by mail clients.
    pub filename: &'a str,
    /// MIME content type, such as `application/pdf`.
    pub mime_type: &'a str,
    /// MIME Content-ID for an inline HTML attachment. Surrounding `<` and `>` are removed.
    /// The HTML body must reference the normalized value with the `cid:` scheme.
    pub content_id: Option<&'a str>,
    /// Exact attachment bytes.
    pub data: &'a [u8],
}

/// One message and its attachments for a bulk append operation.
#[derive(Clone, Copy)]
pub struct UnicodePstBatchMessage<'a> {
    pub message: UnicodePstMessage<'a>,
    pub attachments: &'a [UnicodePstAttachment<'a>],
}

impl UnicodePstFile {
    pub fn read_from(reader: Box<dyn PstReader>) -> io::Result<Self> {
        let inner = PstFileInner::read_from(reader)?;
        Ok(Self { inner })
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let inner = PstFileInner::open(path)?;
        Ok(Self { inner })
    }

    /// Create a new Unicode PST containing Inbox and one message.
    pub fn create(path: impl AsRef<Path>, message: &UnicodePstMessage<'_>) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, &[], None)?;
        Self::open(path)
    }

    /// Create a new Unicode PST containing one message and its by-value attachments.
    pub fn create_with_attachments(
        path: impl AsRef<Path>,
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, attachments, None)?;
        Self::open(path)
    }

    /// Create a new Unicode PST and place its first message in `folder_path`, relative to the
    /// IPM subtree. Missing folders are created; for example `&["Projects", "2026"]`.
    pub fn create_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, &[], Some(folder_path))?;
        Self::open(path)
    }

    /// Create a new Unicode PST and place its first message and attachments in `folder_path`.
    pub fn create_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, attachments, Some(folder_path))?;
        Self::open(path)
    }

    /// Append one message to the default receive folder (normally Inbox) of an
    /// existing unencrypted Unicode PST.
    pub fn append(path: impl AsRef<Path>, message: &UnicodePstMessage<'_>) -> io::Result<Self> {
        append_unicode_pst(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments: &[],
            }],
            None,
        )?;
        Self::open(path)
    }

    /// Append one message and its by-value attachments to the default receive folder of an
    /// existing unencrypted Unicode PST.
    pub fn append_with_attachments(
        path: impl AsRef<Path>,
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments,
            }],
            None,
        )?;
        Self::open(path)
    }

    /// Append messages to the default receive folder in one bulk commit.
    pub fn append_many(
        path: impl AsRef<Path>,
        messages: &[UnicodePstMessage<'_>],
    ) -> io::Result<Self> {
        let messages = messages
            .iter()
            .copied()
            .map(|message| UnicodePstBatchMessage {
                message,
                attachments: &[],
            })
            .collect::<Vec<_>>();
        append_unicode_pst(path.as_ref(), &messages, None)?;
        Self::open(path)
    }

    /// Append messages and attachments to the default receive folder in one bulk commit. Message
    /// and attachment blocks are written incrementally instead of retained for the whole batch.
    pub fn append_many_with_attachments(
        path: impl AsRef<Path>,
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst(path.as_ref(), messages, None)?;
        Self::open(path)
    }

    /// Append one message to `folder_path`, relative to the IPM subtree. Missing folders are
    /// created and existing folder contents are preserved.
    pub fn append_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
    ) -> io::Result<Self> {
        append_unicode_pst(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments: &[],
            }],
            Some(folder_path),
        )?;
        Self::open(path)
    }

    /// Append one message and its attachments to `folder_path`, creating missing folders.
    pub fn append_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments,
            }],
            Some(folder_path),
        )?;
        Self::open(path)
    }

    /// Append messages to `folder_path` in one bulk commit, creating missing folders.
    pub fn append_many_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        messages: &[UnicodePstMessage<'_>],
    ) -> io::Result<Self> {
        let messages = messages
            .iter()
            .copied()
            .map(|message| UnicodePstBatchMessage {
                message,
                attachments: &[],
            })
            .collect::<Vec<_>>();
        append_unicode_pst(path.as_ref(), &messages, Some(folder_path))?;
        Self::open(path)
    }

    /// Append messages and attachments to `folder_path` in one bulk commit, creating missing
    /// folders. Message and attachment blocks are written incrementally.
    pub fn append_many_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst(path.as_ref(), messages, Some(folder_path))?;
        Self::open(path)
    }
}

impl PstFileLock<UnicodePstFile> for UnicodePstFile {
    fn start_write(&mut self) -> io::Result<()> {
        self.inner.start_write()
    }

    fn finish_write(&mut self) -> io::Result<()> {
        self.inner.finish_write()
    }

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::BlockBTree>> {
        self.inner.block_cache.borrow_mut()
    }

    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::NodeBTree>> {
        self.inner.node_cache.borrow_mut()
    }
}

impl PstFile for UnicodePstFile {
    type BlockId = UnicodeBlockId;
    type PageId = UnicodePageId;
    type ByteIndex = UnicodeByteIndex;
    type BlockRef = UnicodeBlockRef;
    type PageRef = UnicodePageRef;
    type Root = UnicodeRoot;
    type Header = UnicodeHeader;
    type PageTrailer = UnicodePageTrailer;
    type BTreeKey = u64;
    type NodeBTreeEntry = UnicodeNodeBTreeEntry;
    type NodeBTree = UnicodeNodeBTree;
    type BlockBTreeEntry = UnicodeBlockBTreeEntry;
    type BlockBTree = UnicodeBlockBTree;
    type BlockTrailer = UnicodeBlockTrailer;
    type AllocationMapPage = UnicodeMapPage<{ PageType::AllocationMap as u8 }>;
    type AllocationPageMapPage = UnicodeMapPage<{ PageType::AllocationPageMap as u8 }>;
    type FreeMapPage = UnicodeMapPage<{ PageType::FreeMap as u8 }>;
    type FreePageMapPage = UnicodeMapPage<{ PageType::FreePageMap as u8 }>;
    type DensityListPage = UnicodeDensityListPage;
    type DataTreeEntry = UnicodeDataTreeEntry;
    type DataTreeBlock = UnicodeDataTreeBlock;
    type DataBlock = UnicodeDataBlock;
    type SubNodeTreeBlockHeader = UnicodeSubNodeTreeBlockHeader;
    type SubNodeTreeBlock = UnicodeIntermediateSubNodeTreeBlock;
    type SubNodeBlock = UnicodeLeafSubNodeTreeBlock;
    type HeapNode = UnicodeHeapNode;
    type PropertyTree = UnicodeHeapTree<PropertyTreeRecordKey, PropertyTreeRecordValue>;
    type TableContext = UnicodeTableContext;
    type PropertyContext = UnicodePropertyContext;
    type Store = UnicodeStore;
    type Folder = UnicodeFolder;
    type Message = UnicodeMessage;
    type NamedPropertyMap = UnicodeNamedPropertyMap;
    type SearchUpdateQueue = UnicodeSearchUpdateQueue;

    fn header(&self) -> &Self::Header {
        &self.inner.header
    }

    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error> {
        self.inner.density_list.as_ref().map(|dl| dl as _)
    }

    fn reader(&self) -> &Mutex<Box<dyn PstReader>> {
        &self.inner.reader
    }

    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>> {
        PstFileLockGuard::new(self)
    }

    fn read_node(&self, node: NodeId) -> io::Result<UnicodeNodeBTreeEntry> {
        self.inner.read_node(node)
    }

    fn read_block(&self, block: UnicodeBlockId) -> io::Result<Vec<u8>> {
        self.inner.read_block(block)
    }
}

pub struct AnsiPstFile {
    inner: PstFileInner<Self>,
}

impl AnsiPstFile {
    pub fn read_from(reader: Box<dyn PstReader>) -> io::Result<Self> {
        let inner = PstFileInner::read_from(reader)?;
        Ok(Self { inner })
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let inner = PstFileInner::open(path)?;
        Ok(Self { inner })
    }
}

impl PstFileLock<AnsiPstFile> for AnsiPstFile {
    fn start_write(&mut self) -> io::Result<()> {
        self.inner.start_write()
    }

    fn finish_write(&mut self) -> io::Result<()> {
        self.inner.finish_write()
    }

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::BlockBTree>> {
        self.inner.block_cache.borrow_mut()
    }

    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::NodeBTree>> {
        self.inner.node_cache.borrow_mut()
    }
}

impl PstFile for AnsiPstFile {
    type BlockId = AnsiBlockId;
    type PageId = AnsiPageId;
    type ByteIndex = AnsiByteIndex;
    type BlockRef = AnsiBlockRef;
    type PageRef = AnsiPageRef;
    type Root = AnsiRoot;
    type Header = AnsiHeader;
    type PageTrailer = AnsiPageTrailer;
    type BTreeKey = u32;
    type NodeBTreeEntry = AnsiNodeBTreeEntry;
    type NodeBTree = AnsiNodeBTree;
    type BlockBTreeEntry = AnsiBlockBTreeEntry;
    type BlockBTree = AnsiBlockBTree;
    type BlockTrailer = AnsiBlockTrailer;
    type AllocationMapPage = AnsiMapPage<{ PageType::AllocationMap as u8 }>;
    type AllocationPageMapPage = AnsiMapPage<{ PageType::AllocationPageMap as u8 }>;
    type FreeMapPage = AnsiMapPage<{ PageType::FreeMap as u8 }>;
    type FreePageMapPage = AnsiMapPage<{ PageType::FreePageMap as u8 }>;
    type DensityListPage = AnsiDensityListPage;
    type DataTreeEntry = AnsiDataTreeEntry;
    type DataTreeBlock = AnsiDataTreeBlock;
    type DataBlock = AnsiDataBlock;
    type SubNodeTreeBlockHeader = AnsiSubNodeTreeBlockHeader;
    type SubNodeTreeBlock = AnsiIntermediateSubNodeTreeBlock;
    type SubNodeBlock = AnsiLeafSubNodeTreeBlock;
    type HeapNode = AnsiHeapNode;
    type PropertyTree = AnsiHeapTree<PropertyTreeRecordKey, PropertyTreeRecordValue>;
    type TableContext = AnsiTableContext;
    type PropertyContext = AnsiPropertyContext;
    type Store = AnsiStore;
    type Folder = AnsiFolder;
    type Message = AnsiMessage;
    type NamedPropertyMap = AnsiNamedPropertyMap;
    type SearchUpdateQueue = AnsiSearchUpdateQueue;

    fn header(&self) -> &Self::Header {
        &self.inner.header
    }

    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error> {
        self.inner.density_list.as_ref().map(|dl| dl as _)
    }

    fn reader(&self) -> &Mutex<Box<dyn PstReader>> {
        &self.inner.reader
    }

    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>> {
        PstFileLockGuard::new(self)
    }

    fn read_node(&self, node: NodeId) -> io::Result<AnsiNodeBTreeEntry> {
        self.inner.read_node(node)
    }

    fn read_block(&self, block: AnsiBlockId) -> io::Result<Vec<u8>> {
        self.inner.read_block(block)
    }
}

const AMAP_FIRST_OFFSET: u64 = 0x4400;
const AMAP_DATA_SIZE: u64 = size_of::<MapBits>() as u64 * 8 * 64;

const PMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + PAGE_SIZE as u64;
const PMAP_PAGE_COUNT: u64 = 8;
const PMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * PMAP_PAGE_COUNT;

const FMAP_FIRST_SIZE: u64 = 128;
const FMAP_FIRST_DATA_SIZE: u64 = AMAP_DATA_SIZE * FMAP_FIRST_SIZE;
const FMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + FMAP_FIRST_DATA_SIZE + (2 * PAGE_SIZE) as u64;
const FMAP_PAGE_COUNT: u64 = size_of::<MapBits>() as u64;
const FMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * FMAP_PAGE_COUNT;

const FPMAP_FIRST_SIZE: u64 = 128 * 64;
const FPMAP_FIRST_DATA_SIZE: u64 = AMAP_DATA_SIZE * FPMAP_FIRST_SIZE;
const FPMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + FPMAP_FIRST_DATA_SIZE + (3 * PAGE_SIZE) as u64;
const FPMAP_PAGE_COUNT: u64 = size_of::<MapBits>() as u64 * 64;
const FPMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * FPMAP_PAGE_COUNT;

const MAX_DATA_BLOCK_SIZE: usize =
    (ndb::block::MAX_BLOCK_SIZE - UnicodeBlockTrailer::SIZE) as usize;
const MAX_HEAP_ALLOCATION_SIZE: usize = MAX_DATA_BLOCK_SIZE - 12;

const OUTLOOK_UNICODE_PST_TEMPLATE: &[u8] = include_bytes!("../examples/Empty.pst");

fn heap_id(allocation_index: usize) -> io::Result<HeapId> {
    Ok(HeapId::new(
        u16::try_from(allocation_index).map_err(|_| PstError::IntegerConversion)?,
        0,
    )?)
}

fn utf16_bytes(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn finish_heap(
    mut allocations: Vec<Vec<u8>>,
    client_signature: HeapNodeType,
    user_root: HeapId,
) -> io::Result<Vec<u8>> {
    const HEAP_HEADER_SIZE: usize = 12;
    let page_map_offset =
        (allocations.iter().map(Vec::len).sum::<usize>() + HEAP_HEADER_SIZE).next_multiple_of(2);
    let page_map_offset =
        u16::try_from(page_map_offset).map_err(|_| PstError::IntegerConversion)?;
    let header = HeapNodeHeader::new(
        page_map_offset,
        client_signature,
        user_root,
        [HeapFillLevel::Empty; 8],
    );
    header.write(&mut allocations[0])?;

    let mut data = allocations.remove(0);
    let mut offsets = Vec::with_capacity(allocations.len() + 1);
    offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    for allocation in allocations {
        data.extend_from_slice(&allocation);
        offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    }

    data.resize(usize::from(page_map_offset), 0);
    data.write_u16::<byteorder::LittleEndian>(
        u16::try_from(offsets.len() - 1).map_err(|_| PstError::IntegerConversion)?,
    )?;
    data.write_u16::<byteorder::LittleEndian>(0)?;
    for offset in offsets {
        data.write_u16::<byteorder::LittleEndian>(offset)?;
    }
    Ok(data)
}

struct HeapBuilder {
    pages: Vec<Vec<Vec<u8>>>,
}

impl HeapBuilder {
    fn new() -> Self {
        Self {
            pages: vec![vec![Vec::new()]],
        }
    }

    fn from_pages(pages: Vec<Vec<Vec<u8>>>) -> Self {
        Self { pages }
    }

    fn header_size(page: usize) -> usize {
        if page == 0 {
            12
        } else if page % 128 == 8 {
            66
        } else {
            2
        }
    }

    fn alloc(&mut self, data: Vec<u8>) -> io::Result<HeapId> {
        if data.len() > MAX_HEAP_ALLOCATION_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "heap allocation exceeds one Unicode PST data block",
            ));
        }

        let mut page = self.pages.len() - 1;
        let fits = |page: usize, allocations: &[Vec<u8>]| {
            let allocation_bytes = allocations.iter().map(Vec::len).sum::<usize>() + data.len();
            let allocation_count = allocations.len() + 1;
            // Leave one byte for alignment after reserving a non-final page's padding entry.
            Self::header_size(page) + allocation_bytes + 4 + 2 * (allocation_count + 1) + 2
                < MAX_DATA_BLOCK_SIZE
        };
        if !fits(page, &self.pages[page]) {
            self.pages.push(vec![Vec::new()]);
            page += 1;
            if !fits(page, &self.pages[page]) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "heap allocation does not fit in a Unicode PST data block",
                ));
            }
        }

        let allocation = self.pages[page].len();
        let heap_id = HeapId::new(
            u16::try_from(allocation).map_err(|_| PstError::IntegerConversion)?,
            u16::try_from(page).map_err(|_| PstError::IntegerConversion)?,
        )?;
        self.pages[page].push(data);
        Ok(heap_id)
    }

    fn finish(self, client_signature: HeapNodeType, user_root: HeapId) -> io::Result<Vec<Vec<u8>>> {
        let page_count = self.pages.len();
        self.pages
            .into_iter()
            .enumerate()
            .map(|(page, mut allocations)| {
                let header_size = Self::header_size(page);
                if page + 1 < page_count {
                    let allocations_end =
                        header_size + allocations.iter().skip(1).map(Vec::len).sum::<usize>();
                    let encoded_size = allocations_end + 4 + 2 * allocations.len();
                    if encoded_size < MAX_DATA_BLOCK_SIZE {
                        let padding = MAX_DATA_BLOCK_SIZE
                            .checked_sub(encoded_size + 2)
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "heap page has no room for a padding allocation",
                                )
                            })?;
                        allocations.push(vec![0; padding]);
                    } else if encoded_size > MAX_DATA_BLOCK_SIZE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "encoded heap page exceeds one Unicode PST data block",
                        ));
                    }
                }
                let page_map_offset = (header_size
                    + allocations.iter().skip(1).map(Vec::len).sum::<usize>())
                .next_multiple_of(2);
                let page_map_offset =
                    u16::try_from(page_map_offset).map_err(|_| PstError::IntegerConversion)?;
                let mut header = Vec::with_capacity(header_size);
                if page == 0 {
                    HeapNodeHeader::new(
                        page_map_offset,
                        client_signature,
                        user_root,
                        [HeapFillLevel::Empty; 8],
                    )
                    .write(&mut header)?;
                } else if page % 128 == 8 {
                    HeapNodeBitmapHeader::new(page_map_offset, [HeapFillLevel::Empty; 128])
                        .write(&mut header)?;
                } else {
                    HeapNodePageHeader::new(page_map_offset).write(&mut header)?;
                }
                allocations[0] = header;

                let mut data = allocations.remove(0);
                let mut offsets = Vec::with_capacity(allocations.len() + 1);
                offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
                for allocation in allocations {
                    data.extend_from_slice(&allocation);
                    offsets
                        .push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
                }
                data.resize(usize::from(page_map_offset), 0);
                data.write_u16::<byteorder::LittleEndian>(
                    u16::try_from(offsets.len() - 1).map_err(|_| PstError::IntegerConversion)?,
                )?;
                let free_count = offsets
                    .windows(2)
                    .filter(|offsets| offsets[0] == offsets[1])
                    .count();
                data.write_u16::<byteorder::LittleEndian>(
                    u16::try_from(free_count).map_err(|_| PstError::IntegerConversion)?,
                )?;
                for offset in offsets {
                    data.write_u16::<byteorder::LittleEndian>(offset)?;
                }
                if data.len() > MAX_DATA_BLOCK_SIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "encoded heap page exceeds one Unicode PST data block",
                    ));
                }
                Ok(data)
            })
            .collect()
    }
}

fn property_value_bytes(value: &PropertyValue) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    value.write(&mut data)?;
    match value {
        PropertyValue::String8(_) => data.push(0),
        PropertyValue::Unicode(_) => data.extend_from_slice(&[0, 0]),
        _ => {}
    }
    Ok(data)
}

fn small_property_record(value: &PropertyValue) -> Option<u32> {
    match value {
        PropertyValue::Integer16(value) => Some(u32::from(*value as u16)),
        PropertyValue::Integer32(value) | PropertyValue::ErrorCode(value) => Some(*value as u32),
        PropertyValue::Floating32(value) => Some(value.to_bits()),
        PropertyValue::Boolean(value) => Some(u32::from(*value)),
        _ => None,
    }
}

type RebuiltPropertyContext = (Vec<Vec<u8>>, Vec<(NodeId, Vec<u8>)>);

fn build_property_tree(
    heap: &mut HeapBuilder,
    records: &[(u16, PropertyType, PropertyValueRecord)],
) -> io::Result<HeapId> {
    let mut children = records
        .chunks(750)
        .map(|chunk| {
            let mut data = Vec::with_capacity(chunk.len() * 8);
            for (id, prop_type, value) in chunk {
                PropertyTreeRecord::new(*id, *prop_type, *value).write(&mut data)?;
            }
            Ok((chunk[0].0, heap.alloc(data)?))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut levels = 0;
    while children.len() > 1 {
        children = children
            .chunks(900)
            .map(|chunk| {
                let mut data = Vec::with_capacity(chunk.len() * 6);
                for (key, child) in chunk {
                    HeapTreeIntermediateEntry::new(*key, *child).write(&mut data)?;
                }
                Ok((chunk[0].0, heap.alloc(data)?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        levels += 1;
    }
    let root = children.first().map(|entry| entry.1).unwrap_or_default();
    let mut header = Vec::new();
    HeapTreeHeader::new(2, 6, levels, root)?.write(&mut header)?;
    heap.alloc(header)
}

fn property_context_pages(
    properties: impl IntoIterator<Item = (u16, PropertyValue)>,
    mut next_subnode_index: u32,
) -> io::Result<RebuiltPropertyContext> {
    let mut heap = HeapBuilder::new();
    let mut external = Vec::new();
    let mut records = properties
        .into_iter()
        .filter(|(_, value)| !matches!(value, PropertyValue::Null))
        .map(|(id, value)| {
            let prop_type = PropertyType::from(&value);
            let record = if let Some(small) = small_property_record(&value) {
                PropertyValueRecord::Small(small)
            } else {
                let data = property_value_bytes(&value)?;
                if data.is_empty() {
                    PropertyValueRecord::Heap(HeapId::default())
                } else if data.len() > MAX_HEAP_ALLOCATION_SIZE {
                    let node = NodeId::new(NodeIdType::Internal, next_subnode_index)?;
                    next_subnode_index = next_subnode_index
                        .checked_add(1)
                        .ok_or(PstError::IntegerConversion)?;
                    external.push((node, data));
                    PropertyValueRecord::Node(node)
                } else {
                    PropertyValueRecord::Heap(heap.alloc(data)?)
                }
            };
            Ok((id, prop_type, record))
        })
        .collect::<io::Result<Vec<_>>>()?;
    records.sort_by_key(|record| record.0);
    let user_root = build_property_tree(&mut heap, &records)?;
    Ok((heap.finish(HeapNodeType::Properties, user_root)?, external))
}

enum TableCell {
    Value(PropertyValue),
    Bytes(Vec<u8>),
    Node(NodeId),
}

type RebuiltTable = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<NodeId>);
type RebuiltContentsTable = (RebuiltTable, Vec<(NodeId, Vec<u8>)>);

fn table_pointer_type(prop_type: PropertyType) -> bool {
    matches!(
        prop_type,
        PropertyType::String8
            | PropertyType::Unicode
            | PropertyType::Guid
            | PropertyType::Binary
            | PropertyType::Object
            | PropertyType::MultipleInteger16
            | PropertyType::MultipleInteger32
            | PropertyType::MultipleFloating32
            | PropertyType::MultipleFloating64
            | PropertyType::MultipleCurrency
            | PropertyType::MultipleFloatingTime
            | PropertyType::MultipleInteger64
            | PropertyType::MultipleString8
            | PropertyType::MultipleUnicode
            | PropertyType::MultipleTime
            | PropertyType::MultipleGuid
            | PropertyType::MultipleBinary
    )
}

fn write_fixed_table_value(
    mut target: &mut [u8],
    prop_type: PropertyType,
    value: &PropertyValue,
) -> io::Result<()> {
    match (prop_type, value) {
        (PropertyType::Integer16, PropertyValue::Integer16(value)) => {
            target.write_i16::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Integer32, PropertyValue::Integer32(value)) => {
            target.write_i32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Floating32, PropertyValue::Floating32(value)) => {
            target.write_f32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Floating64, PropertyValue::Floating64(value)) => {
            target.write_f64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Currency, PropertyValue::Currency(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::FloatingTime, PropertyValue::FloatingTime(value)) => {
            target.write_f64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::ErrorCode, PropertyValue::ErrorCode(value)) => {
            target.write_i32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Boolean, PropertyValue::Boolean(value)) => target.write_u8(u8::from(*value)),
        (PropertyType::Integer64, PropertyValue::Integer64(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Time, PropertyValue::Time(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("table value does not match {prop_type:?}"),
        )),
    }
}

fn encode_table_row(
    context: &TableContextInfo,
    heap: &mut HeapBuilder,
    row_id: impl Into<u32>,
    unique: u32,
    cells: Vec<Option<TableCell>>,
) -> io::Result<Vec<u8>> {
    let mut row = vec![0; usize::from(context.end_existence_bitmap())];
    (&mut row[0..4]).write_u32::<byteorder::LittleEndian>(row_id.into())?;
    (&mut row[4..8]).write_u32::<byteorder::LittleEndian>(unique)?;

    for (column, cell) in context.columns().iter().zip(cells) {
        let Some(cell) = cell else { continue };
        let bit = usize::from(column.existence_bitmap_index());
        row[usize::from(context.end_1byte_values()) + bit / 8] |= 0x80 >> (bit % 8);

        let offset = usize::from(column.offset());
        let size = usize::from(column.size());
        let mut target = &mut row[offset..offset + size];
        if table_pointer_type(column.prop_type()) {
            let value = match cell {
                TableCell::Node(node) => u32::from(node),
                TableCell::Bytes(data) => u32::from(heap.alloc(data)?),
                TableCell::Value(value) => u32::from(heap.alloc(property_value_bytes(&value)?)?),
            };
            target.write_u32::<byteorder::LittleEndian>(value)?;
        } else {
            let TableCell::Value(value) = cell else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixed-width table column has an indirect value",
                ));
            };
            write_fixed_table_value(target, column.prop_type(), &value)?;
        }
    }
    Ok(row)
}

fn build_row_index<T>(heap: &mut HeapBuilder, rows: &[T]) -> io::Result<HeapId>
where
    T: Copy,
    u32: From<T>,
{
    let mut children = rows
        .chunks(750)
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let mut data = Vec::with_capacity(chunk.len() * 8);
            for (index, row) in chunk.iter().enumerate() {
                TableRowId::new(u32::from(*row)).write(&mut data)?;
                data.write_u32::<byteorder::LittleEndian>(
                    u32::try_from(chunk_index * 750 + index)
                        .map_err(|_| PstError::IntegerConversion)?,
                )?;
            }
            Ok((TableRowId::new(u32::from(chunk[0])), heap.alloc(data)?))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut levels = 0;
    while children.len() > 1 {
        children = children
            .chunks(750)
            .map(|chunk| {
                let mut data = Vec::with_capacity(chunk.len() * 8);
                for (key, child) in chunk {
                    HeapTreeIntermediateEntry::new(*key, *child).write(&mut data)?;
                }
                Ok((chunk[0].0, heap.alloc(data)?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        levels += 1;
    }
    let root = children.first().map(|entry| entry.1).unwrap_or_default();
    let mut header = Vec::new();
    HeapTreeHeader::new(4, 4, levels, root)?.write(&mut header)?;
    heap.alloc(header)
}

fn string_table_cell(prop_type: PropertyType, value: &str) -> Option<TableCell> {
    match prop_type {
        PropertyType::String8 => {
            let mut data = value.as_bytes().to_vec();
            data.push(0);
            Some(TableCell::Bytes(data))
        }
        PropertyType::Unicode => Some(TableCell::Bytes(utf16_bytes(value))),
        _ => None,
    }
}

fn recipient_display(
    recipients: &[UnicodePstRecipient<'_>],
    recipient_type: UnicodePstRecipientType,
) -> String {
    recipients
        .iter()
        .filter(|recipient| recipient.recipient_type == recipient_type)
        .map(|recipient| {
            if recipient.name.is_empty() {
                recipient.email
            } else {
                recipient.name
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn recipient_display_table_cell(
    prop_type: PropertyType,
    recipients: &[UnicodePstRecipient<'_>],
    recipient_type: UnicodePstRecipientType,
) -> Option<TableCell> {
    let display = recipient_display(recipients, recipient_type);
    (!display.is_empty())
        .then(|| string_table_cell(prop_type, &display))
        .flatten()
}

fn message_size(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    message: NodeId,
    row_version: u32,
) -> io::Result<i32> {
    let (message_pages, external) = message_data(input, attachments, message, row_version, 0)?;
    let (recipient_pages, recipient_rows) = recipient_table_data(input.recipients, row_version)?;
    let attachment_ids = (0..attachments.len())
        .map(|index| {
            let index = u32::try_from(index).map_err(|_| PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(NodeIdType::Attachment, 0x100 + index)?)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let (attachment_pages, attachment_rows) =
        attachment_table_data(&attachment_ids, attachments, row_version)?;

    let mut size = message_pages
        .iter()
        .chain(recipient_pages.iter())
        .chain(attachment_pages.iter())
        .chain(external.iter().map(|(_, data)| data))
        .try_fold(0_usize, |size, data| size.checked_add(data.len()))
        .ok_or(PstError::IntegerConversion)?;
    for rows in [recipient_rows, attachment_rows].into_iter().flatten() {
        size = rows
            .1
            .iter()
            .try_fold(size, |size, data| size.checked_add(data.len()))
            .ok_or(PstError::IntegerConversion)?;
    }
    for (index, attachment) in attachments.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| PstError::IntegerConversion)?;
        let data_node = (!attachment.data.is_empty())
            .then(|| NodeId::new(NodeIdType::Internal, 0x200 + index))
            .transpose()?;
        size = size
            .checked_add(attachment_data(attachment, data_node)?.len())
            .and_then(|size| size.checked_add(attachment.data.len()))
            .ok_or(PstError::IntegerConversion)?;
    }

    // Outlook excludes the six-byte HN client/root header from message-owned payloads.
    i32::try_from(size.checked_sub(6).ok_or(PstError::IntegerConversion)?)
        .map_err(|_| PstError::IntegerConversion.into())
}

#[allow(clippy::too_many_arguments)]
fn new_message_table_cell(
    column: &TableColumnDescriptor,
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    store_record_key: [u8; 16],
    folder: NodeId,
    message: NodeId,
    row_version: u32,
    message_size: i32,
) -> io::Result<Option<TableCell>> {
    let prop_type = column.prop_type();
    let cell = match column.prop_id() {
        LTP_ROW_ID_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(u32::from(message) as i32),
        )),
        LTP_ROW_VERSION_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(row_version as i32),
        )),
        0x001A => string_table_cell(prop_type, "IPM.Note"),
        0x0037 | 0x0E1D | 0x0070 | 0x3001 => string_table_cell(prop_type, input.subject),
        0x0042 | 0x0C1A => string_table_cell(prop_type, input.sender_name),
        0x0C1F => string_table_cell(prop_type, input.sender_email),
        0x0E02 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::Bcc)
        }
        0x0E03 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::Cc)
        }
        0x0E04 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::To)
        }
        0x1035 => string_table_cell(prop_type, input.message_id),
        0x0039 | 0x0E06 | 0x3008 if prop_type == PropertyType::Time => {
            Some(TableCell::Value(PropertyValue::Time(input.delivery_time)))
        }
        0x0E17 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(0)))
        }
        0x0E07 if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(1 | if attachments.is_empty() { 0 } else { 0x10 }),
        )),
        0x0E08 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(message_size)))
        }
        0x0E1B if prop_type == PropertyType::Boolean => Some(TableCell::Value(
            PropertyValue::Boolean(!attachments.is_empty()),
        )),
        0x3FDE if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(65001)))
        }
        0x0E09 | 0x0FFF if prop_type == PropertyType::Binary => {
            let node = if column.prop_id() == 0x0E09 {
                folder
            } else {
                message
            };
            let entry_id = EntryId::new(StoreRecordKey::new(store_record_key), node);
            Some(TableCell::Bytes(Vec::try_from(&entry_id)?))
        }
        0x300B if prop_type == PropertyType::Binary => {
            Some(TableCell::Bytes(input.message_id.as_bytes().to_vec()))
        }
        0x3013 if prop_type == PropertyType::Binary => None,
        _ => None,
    };
    Ok(cell)
}

fn rebuild_table_with_rows_and_version(
    table: &dyn TableContext,
    new_rows: Vec<(NodeId, Vec<Option<TableCell>>)>,
    rows_node: Option<NodeId>,
    row_version: u32,
) -> io::Result<RebuiltTable> {
    rebuild_table(table, new_rows, rows_node, row_version, None, false)
}

fn rebuild_table(
    table: &dyn TableContext,
    new_rows: Vec<(NodeId, Vec<Option<TableCell>>)>,
    rows_node: Option<NodeId>,
    row_version: u32,
    property_update: Option<(NodeId, u16, PropertyValue)>,
    preserve_existing_rows: bool,
) -> io::Result<RebuiltTable> {
    let old_context = table.context();
    let mut heap = if preserve_existing_rows {
        HeapBuilder::from_pages(table.writer_heap_pages()?)
    } else {
        HeapBuilder::new()
    };
    let row_size = usize::from(old_context.end_existence_bitmap());
    let mut row_chunks: Vec<Vec<u8>> = Vec::new();
    let mut row_ids = Vec::new();
    let push_row = |row_chunks: &mut Vec<Vec<u8>>, row: Vec<u8>| -> io::Result<()> {
        if row_size == 0 || row_size > MAX_DATA_BLOCK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "table rows do not fit in a Unicode PST data block",
            ));
        }
        if row_chunks
            .last()
            .is_none_or(|chunk| chunk.len() + row_size > MAX_DATA_BLOCK_SIZE)
        {
            row_chunks.push(Vec::with_capacity(MAX_DATA_BLOCK_SIZE));
        }
        row_chunks.last_mut().unwrap().extend(row);
        Ok(())
    };

    for old_row in table.rows_matrix() {
        let row_id = NodeId::from(u32::from(old_row.id()));
        if preserve_existing_rows
            && property_update
                .as_ref()
                .is_none_or(|(update_row, _, _)| *update_row != row_id)
        {
            push_row(&mut row_chunks, old_row.writer_data()?)?;
            row_ids.push(row_id);
            continue;
        }
        let mut cells = old_row
            .columns(old_context)?
            .into_iter()
            .zip(old_context.columns())
            .map(|(value, column)| match value {
                None => Ok(None),
                Some(TableRowColumnValue::Small(value)) => Ok(Some(TableCell::Value(value))),
                Some(TableRowColumnValue::Heap(id)) if u32::from(id) == 0 => Ok(None),
                Some(TableRowColumnValue::Node(id)) if u32::from(id) == 0 => Ok(None),
                Some(value @ TableRowColumnValue::Heap(_)) => table
                    .read_column(&value, column.prop_type())
                    .map(TableCell::Value)
                    .map(Some),
                Some(TableRowColumnValue::Node(node)) => Ok(Some(TableCell::Node(node))),
            })
            .collect::<io::Result<Vec<_>>>()?;
        if preserve_existing_rows {
            for (column, cell) in old_context.columns().iter().zip(&mut cells) {
                if matches!(column.prop_id(), 0x0E30 | 0x0E33 | 0x0E34) {
                    *cell = None;
                }
            }
        }
        let mut unique = old_row.unique();
        if let Some((update_row, property, value)) = &property_update {
            if row_id == *update_row {
                unique = row_version;
                for (column, cell) in old_context.columns().iter().zip(&mut cells) {
                    if column.prop_id() == *property
                        && column.prop_type() == PropertyType::from(value)
                    {
                        *cell = Some(TableCell::Value(value.clone()));
                    } else if column.prop_id() == LTP_ROW_VERSION_PROP_ID
                        && column.prop_type() == PropertyType::Integer32
                    {
                        *cell = Some(TableCell::Value(PropertyValue::Integer32(
                            row_version as i32,
                        )));
                    }
                }
            }
        }
        let row = encode_table_row(old_context, &mut heap, row_id, unique, cells)?;
        push_row(&mut row_chunks, row)?;
        row_ids.push(row_id);
    }

    for (row_id, cells) in new_rows {
        let row = encode_table_row(old_context, &mut heap, row_id, row_version, cells)?;
        push_row(&mut row_chunks, row)?;
        row_ids.push(row_id);
    }

    let row_index = build_row_index(&mut heap, &row_ids)?;
    let rows_node = if row_ids.is_empty() {
        None
    } else {
        Some(rows_node.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "table rows node is missing")
        })?)
    };
    let context = TableContextInfo::new(
        old_context.end_4byte_values(),
        old_context.end_2byte_values(),
        old_context.end_1byte_values(),
        old_context.end_existence_bitmap(),
        row_index,
        rows_node,
        old_context.columns().to_vec(),
    )?;
    let mut context_data = Vec::new();
    context.write(&mut context_data)?;
    let user_root = heap.alloc(context_data)?;
    let heap_pages = heap.finish(HeapNodeType::Table, user_root)?;
    Ok((heap_pages, row_chunks, row_ids))
}

#[allow(clippy::too_many_arguments)]
fn rebuild_contents_table(
    table: &dyn TableContext,
    inputs: &[UnicodePstBatchMessage<'_>],
    store_record_key: [u8; 16],
    folder: NodeId,
    messages: &[NodeId],
    message_sizes: &[i32],
    rows_node: NodeId,
    mut next_subnode_index: u32,
    row_version: u32,
) -> io::Result<RebuiltContentsTable> {
    let mut external = Vec::new();
    let rows = inputs
        .iter()
        .zip(messages)
        .zip(message_sizes)
        .map(|((input, message), message_size)| {
            let mut cells = table
                .context()
                .columns()
                .iter()
                .map(|column| {
                    new_message_table_cell(
                        column,
                        &input.message,
                        input.attachments,
                        store_record_key,
                        folder,
                        *message,
                        row_version,
                        *message_size,
                    )
                })
                .collect::<io::Result<Vec<_>>>()?;
            for cell in cells.iter_mut().flatten() {
                let TableCell::Bytes(data) = cell else {
                    continue;
                };
                if data.len() > MAX_HEAP_ALLOCATION_SIZE {
                    let node = NodeId::new(NodeIdType::Internal, next_subnode_index)?;
                    next_subnode_index = next_subnode_index
                        .checked_add(1)
                        .ok_or(PstError::IntegerConversion)?;
                    external.push((node, std::mem::take(data)));
                    *cell = TableCell::Node(node);
                }
            }
            Ok((*message, cells))
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok((
        rebuild_table_with_rows_and_version(table, rows, Some(rows_node), row_version)?,
        external,
    ))
}

fn property_context_data(
    properties: &[(u16, PropertyType, Option<Vec<u8>>, u32)],
) -> io::Result<Vec<u8>> {
    property_context_data_with_nodes(properties, &[])
}

fn property_context_data_with_nodes(
    properties: &[(u16, PropertyType, Option<Vec<u8>>, u32)],
    nodes: &[(u16, NodeId)],
) -> io::Result<Vec<u8>> {
    let mut allocations = vec![vec![], vec![], vec![]];
    let mut records = Vec::with_capacity(properties.len());

    for (prop_id, prop_type, value, small) in properties {
        let record = if let Some((_, node)) = nodes.iter().find(|(id, _)| id == prop_id) {
            PropertyTreeRecord::new(*prop_id, *prop_type, PropertyValueRecord::Node(*node))
        } else if let Some(value) = value {
            let allocation = allocations.len();
            allocations.push(value.clone());
            PropertyTreeRecord::new(
                *prop_id,
                *prop_type,
                PropertyValueRecord::Heap(heap_id(allocation)?),
            )
        } else {
            PropertyTreeRecord::new(*prop_id, *prop_type, PropertyValueRecord::Small(*small))
        };
        record.write(&mut records)?;
    }

    let tree_root = heap_id(2)?;
    let tree_header = HeapTreeHeader::new(2, 6, 0, tree_root)?;
    tree_header.write(&mut allocations[1])?;
    allocations[2] = records;

    finish_heap(allocations, HeapNodeType::Properties, heap_id(1)?)
}

fn table_context_data(row_ids: &[NodeId]) -> io::Result<Vec<u8>> {
    let row_index = heap_id(2)?;
    let rows_heap = (!row_ids.is_empty()).then(|| heap_id(4)).transpose()?;
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let context = TableContextInfo::new(
        8,
        8,
        8,
        9,
        row_index,
        rows_heap.map(|id| NodeId::from(u32::from(id))),
        columns,
    )?;

    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;

    let tree_root = if row_ids.is_empty() {
        HeapId::from(0)
    } else {
        heap_id(3)?
    };
    let tree_header = HeapTreeHeader::new(4, 4, 0, tree_root)?;
    let mut tree_header_bytes = Vec::new();
    tree_header.write(&mut tree_header_bytes)?;

    let mut index_bytes = Vec::new();
    let mut rows = Vec::new();
    for (index, node_id) in row_ids.iter().enumerate() {
        let row_id = TableRowId::new(u32::from(*node_id));
        row_id.write(&mut index_bytes)?;
        index_bytes.write_u32::<byteorder::LittleEndian>(
            u32::try_from(index).map_err(|_| PstError::IntegerConversion)?,
        )?;

        rows.write_u32::<byteorder::LittleEndian>(u32::from(*node_id))?;
        rows.write_u32::<byteorder::LittleEndian>(0)?;
        rows.push(0xC0);
    }

    let mut allocations = vec![vec![], context_bytes, tree_header_bytes];
    if !row_ids.is_empty() {
        allocations.push(index_bytes);
        allocations.push(rows);
    }
    finish_heap(allocations, HeapNodeType::Table, heap_id(1)?)
}

type RebuiltRecipientTable = (Vec<Vec<u8>>, Option<(NodeId, Vec<Vec<u8>>)>);

fn one_off_entry_id(recipient: &UnicodePstRecipient<'_>) -> Vec<u8> {
    let mut entry_id = vec![0; 4];
    entry_id.extend_from_slice(&[
        0x81, 0x2B, 0x1F, 0xA4, 0xBE, 0xA3, 0x10, 0x19, 0x9D, 0x6E, 0x00, 0xDD, 0x01, 0x0F, 0x54,
        0x02,
    ]);
    entry_id.extend_from_slice(&0u16.to_le_bytes());
    entry_id.extend_from_slice(&0x8000u16.to_le_bytes());
    entry_id.extend(utf16_bytes(if recipient.name.is_empty() {
        recipient.email
    } else {
        recipient.name
    }));
    entry_id.extend(utf16_bytes("SMTP"));
    entry_id.extend(utf16_bytes(recipient.email));
    entry_id
}

fn recipient_search_key(email: &str) -> Vec<u8> {
    let mut key = format!("SMTP:{email}").to_ascii_uppercase().into_bytes();
    key.push(0);
    key
}

fn recipient_table_data(
    recipients: &[UnicodePstRecipient<'_>],
    row_version: u32,
) -> io::Result<RebuiltRecipientTable> {
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0C15, 8, 4, 2),
        TableColumnDescriptor::new(PropertyType::Boolean, 0x0E0F, 52, 1, 3),
        TableColumnDescriptor::new(PropertyType::Binary, 0x0FF9, 12, 4, 4),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0FFE, 16, 4, 5),
        TableColumnDescriptor::new(PropertyType::Binary, 0x0FFF, 20, 4, 6),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3001, 24, 4, 7),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3002, 28, 4, 8),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3003, 32, 4, 9),
        TableColumnDescriptor::new(PropertyType::Binary, 0x300B, 36, 4, 10),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x3900, 40, 4, 11),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x39FE, 44, 4, 12),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x39FF, 48, 4, 13),
        TableColumnDescriptor::new(PropertyType::Boolean, 0x3A40, 53, 1, 14),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let row_context =
        TableContextInfo::new(52, 52, 54, 56, HeapId::default(), None, columns.clone())?;
    let row_ids = (1..=recipients.len())
        .map(|id| u32::try_from(id).map_err(|_| PstError::IntegerConversion.into()))
        .collect::<io::Result<Vec<_>>>()?;
    let mut heap = HeapBuilder::new();
    let rows = row_ids
        .iter()
        .zip(recipients)
        .map(|(row_id, recipient)| {
            let entry_id = one_off_entry_id(recipient);
            let display_name = if recipient.name.is_empty() {
                recipient.email
            } else {
                recipient.name
            };
            encode_table_row(
                &row_context,
                &mut heap,
                *row_id,
                row_version,
                vec![
                    Some(TableCell::Value(PropertyValue::Integer32(
                        recipient.recipient_type as i32,
                    ))),
                    Some(TableCell::Value(PropertyValue::Boolean(true))),
                    None,
                    Some(TableCell::Value(PropertyValue::Integer32(6))),
                    Some(TableCell::Bytes(entry_id)),
                    string_table_cell(PropertyType::Unicode, display_name),
                    string_table_cell(PropertyType::Unicode, "SMTP"),
                    string_table_cell(PropertyType::Unicode, recipient.email),
                    Some(TableCell::Bytes(recipient_search_key(recipient.email))),
                    Some(TableCell::Value(PropertyValue::Integer32(0))),
                    string_table_cell(PropertyType::Unicode, recipient.email),
                    string_table_cell(PropertyType::Unicode, display_name),
                    Some(TableCell::Value(PropertyValue::Boolean(false))),
                    Some(TableCell::Value(PropertyValue::Integer32(
                        i32::try_from(*row_id).map_err(|_| PstError::IntegerConversion)?,
                    ))),
                    Some(TableCell::Value(PropertyValue::Integer32(
                        row_version as i32,
                    ))),
                ],
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    let row_index = build_row_index(&mut heap, &row_ids)?;
    let rows_node = (!rows.is_empty())
        .then(|| NodeId::new(NodeIdType::Internal, 0x80))
        .transpose()?;
    let context = TableContextInfo::new(52, 52, 54, 56, row_index, rows_node, columns)?;
    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;
    let user_root = heap.alloc(context_bytes)?;
    let pages = heap.finish(HeapNodeType::Table, user_root)?;
    let rows_per_block = MAX_DATA_BLOCK_SIZE / 56;
    let row_chunks = rows
        .chunks(rows_per_block)
        .map(|chunk| chunk.concat())
        .collect::<Vec<_>>();
    Ok((pages, rows_node.map(|node| (node, row_chunks))))
}

fn attachment_table_data(
    attachment_ids: &[NodeId],
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
) -> io::Result<RebuiltRecipientTable> {
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0E20, 8, 4, 2),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3704, 12, 4, 3),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x3705, 16, 4, 4),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x370B, 20, 4, 5),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let row_context =
        TableContextInfo::new(24, 24, 24, 25, HeapId::default(), None, columns.clone())?;
    let mut heap = HeapBuilder::new();
    let rows = attachment_ids
        .iter()
        .zip(attachments)
        .map(|(attachment_id, attachment)| {
            let size = attachment_size(attachment)?;
            encode_table_row(
                &row_context,
                &mut heap,
                *attachment_id,
                row_version,
                vec![
                    Some(TableCell::Value(PropertyValue::Integer32(size))),
                    string_table_cell(PropertyType::Unicode, attachment.filename),
                    Some(TableCell::Value(PropertyValue::Integer32(1))),
                    Some(TableCell::Value(PropertyValue::Integer32(-1))),
                    Some(TableCell::Value(PropertyValue::Integer32(
                        u32::from(*attachment_id) as i32,
                    ))),
                    Some(TableCell::Value(PropertyValue::Integer32(
                        row_version as i32,
                    ))),
                ],
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    let row_index = build_row_index(&mut heap, attachment_ids)?;
    let rows_node = (!rows.is_empty())
        .then(|| NodeId::new(NodeIdType::Internal, 0x80))
        .transpose()?;
    let context = TableContextInfo::new(24, 24, 24, 25, row_index, rows_node, columns)?;
    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;
    let user_root = heap.alloc(context_bytes)?;
    let pages = heap.finish(HeapNodeType::Table, user_root)?;
    let rows_per_block = MAX_DATA_BLOCK_SIZE / 25;
    let row_chunks = rows
        .chunks(rows_per_block)
        .map(|chunk| chunk.concat())
        .collect::<Vec<_>>();
    Ok((pages, rows_node.map(|node| (node, row_chunks))))
}

fn validate_folder_path(folder_path: &[&str]) -> io::Result<()> {
    if folder_path.is_empty() || folder_path.iter().any(|name| name.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "folder path must contain only non-empty names",
        ));
    }
    Ok(())
}

fn folder_data(name: &str, content_count: usize, has_subfolders: bool) -> io::Result<Vec<u8>> {
    let content_count = u32::try_from(content_count).map_err(|_| PstError::IntegerConversion)?;
    property_context_data(&[
        (0x3001, PropertyType::Unicode, Some(utf16_bytes(name)), 0),
        (0x3602, PropertyType::Integer32, None, content_count),
        (0x3603, PropertyType::Integer32, None, 0),
        (
            0x360A,
            PropertyType::Boolean,
            None,
            u32::from(has_subfolders),
        ),
        (
            0x3613,
            PropertyType::Unicode,
            Some(utf16_bytes("IPF.Note")),
            0,
        ),
    ])
}

fn message_data(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    message: NodeId,
    row_version: u32,
    message_size: i32,
) -> io::Result<RebuiltPropertyContext> {
    let has_attachments = !attachments.is_empty();
    let unicode =
        |value: &str| PropertyValue::Unicode(UnicodeValue::new(value.encode_utf16().collect()));
    let mut properties = vec![
        (0x001A, unicode("IPM.Note")),
        (0x0037, unicode(input.subject)),
        (0x0039, PropertyValue::Time(input.delivery_time)),
        (0x0042, unicode(input.sender_name)),
        (0x0070, unicode(input.subject)),
        (0x0C1A, unicode(input.sender_name)),
        (0x0C1F, unicode(input.sender_email)),
        (0x0E06, PropertyValue::Time(input.delivery_time)),
        (
            0x0E07,
            PropertyValue::Integer32(1 | if has_attachments { 0x10 } else { 0 }),
        ),
        (0x0E08, PropertyValue::Integer32(message_size)),
        (0x0E17, PropertyValue::Integer32(0)),
        (0x0E1B, PropertyValue::Boolean(has_attachments)),
        (0x1000, unicode(input.body)),
        (0x1035, unicode(input.message_id)),
        (0x3007, PropertyValue::Time(input.delivery_time)),
        (0x3008, PropertyValue::Time(input.delivery_time)),
        (
            0x300B,
            PropertyValue::Binary(BinaryValue::new(input.message_id.as_bytes().to_vec())),
        ),
        (0x3FDE, PropertyValue::Integer32(65001)),
        (
            LTP_ROW_ID_PROP_ID,
            PropertyValue::Integer32(u32::from(message) as i32),
        ),
        (
            LTP_ROW_VERSION_PROP_ID,
            PropertyValue::Integer32(row_version as i32),
        ),
    ];
    for (id, recipient_type) in [
        (0x0E02, UnicodePstRecipientType::Bcc),
        (0x0E03, UnicodePstRecipientType::Cc),
        (0x0E04, UnicodePstRecipientType::To),
    ] {
        let display = recipient_display(input.recipients, recipient_type);
        if !display.is_empty() {
            properties.push((id, unicode(&display)));
        }
    }
    if let Some(html) = input.html_body {
        properties.push((
            0x1013,
            PropertyValue::Binary(BinaryValue::new(html.as_bytes().to_vec())),
        ));
    }
    property_context_pages(properties, 0x80)
}

fn attachment_data(
    input: &UnicodePstAttachment<'_>,
    data_node: Option<NodeId>,
) -> io::Result<Vec<u8>> {
    let size = attachment_size(input)?;
    let mut properties = vec![
        (0x0E20, PropertyType::Integer32, None, size as u32),
        (0x3701, PropertyType::Binary, None, 0),
        (
            0x3704,
            PropertyType::Unicode,
            Some(utf16_bytes(input.filename)),
            0,
        ),
        (0x3705, PropertyType::Integer32, None, 1),
        (
            0x3707,
            PropertyType::Unicode,
            Some(utf16_bytes(input.filename)),
            0,
        ),
        (0x370B, PropertyType::Integer32, None, u32::MAX),
        (
            0x370E,
            PropertyType::Unicode,
            Some(utf16_bytes(input.mime_type)),
            0,
        ),
    ];
    if let Some(content_id) = attachment_content_id(input)? {
        properties.extend([
            (
                0x3712,
                PropertyType::Unicode,
                Some(utf16_bytes(content_id)),
                0,
            ),
            // PidTagAttachFlags bit 0x4 marks an attachment as referenced by HTML.
            (0x3714, PropertyType::Integer32, None, 0x0000_0004),
        ]);
    }
    property_context_data_with_nodes(
        &properties,
        &data_node.map(|node| [(0x3701, node)]).unwrap_or_default(),
    )
}

fn attachment_content_id<'a>(input: &'a UnicodePstAttachment<'_>) -> io::Result<Option<&'a str>> {
    let Some(content_id) = input.content_id else {
        return Ok(None);
    };
    let content_id = content_id.strip_prefix('<').unwrap_or(content_id);
    let content_id = content_id.strip_suffix('>').unwrap_or(content_id);
    if content_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment Content-ID must not be empty",
        ));
    }
    Ok(Some(content_id))
}

fn attachment_size(input: &UnicodePstAttachment<'_>) -> io::Result<i32> {
    let filenames_size = utf16_bytes(input.filename)
        .len()
        .checked_mul(2)
        .ok_or(PstError::IntegerConversion)?;
    let mut size = input
        .data
        .len()
        .checked_add(12)
        .and_then(|size| size.checked_add(filenames_size))
        .and_then(|size| size.checked_add(utf16_bytes(input.mime_type).len()))
        .ok_or(PstError::IntegerConversion)?;
    if let Some(content_id) = attachment_content_id(input)? {
        size = size
            .checked_add(utf16_bytes(content_id).len())
            .and_then(|size| size.checked_add(4))
            .ok_or(PstError::IntegerConversion)?;
    }
    i32::try_from(size).map_err(|_| PstError::IntegerConversion.into())
}

enum NewBlockData {
    Data(Vec<u8>),
    DataTree(Vec<UnicodeDataTreeEntry>, u8, u32),
    SubNodes(Vec<UnicodeLeafSubNodeTreeEntry>),
    IntermediateSubNodes(Vec<UnicodeIntermediateSubNodeTreeEntry>, u8),
}

struct NewBlock {
    id: UnicodeBlockId,
    offset: u64,
    size: u16,
    data: NewBlockData,
}

enum NewBTreePageData {
    BlockLeaf(Vec<UnicodeBlockBTreeEntry>),
    NodeLeaf(Vec<UnicodeNodeBTreeEntry>),
    Intermediate(PageType, u8, Vec<UnicodeBTreePageEntry>),
}

struct NewBTreePage {
    page_ref: UnicodePageRef,
    data: NewBTreePageData,
}

fn collect_block_btree_entries(
    file: &mut File,
    page_ref: UnicodePageRef,
) -> io::Result<Vec<UnicodeBlockBTreeEntry>> {
    fn visit(
        file: &mut File,
        page_ref: UnicodePageRef,
        depth: u8,
        entries: &mut Vec<UnicodeBlockBTreeEntry>,
    ) -> io::Result<()> {
        if depth > 8 {
            return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
        }
        match UnicodeBlockBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => entries.extend_from_slice(page.entries()),
            RootBTreePage::Intermediate(page, ..) => {
                for entry in page.entries() {
                    visit(file, entry.block(), depth + 1, entries)?;
                }
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    visit(file, page_ref, 0, &mut entries)?;
    entries.sort_by_key(BTreeEntry::key);
    Ok(entries)
}

fn collect_node_btree_entries(
    file: &mut File,
    page_ref: UnicodePageRef,
) -> io::Result<Vec<UnicodeNodeBTreeEntry>> {
    fn visit(
        file: &mut File,
        page_ref: UnicodePageRef,
        depth: u8,
        entries: &mut Vec<UnicodeNodeBTreeEntry>,
    ) -> io::Result<()> {
        if depth > 8 {
            return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
        }
        match UnicodeNodeBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => entries.extend_from_slice(page.entries()),
            RootBTreePage::Intermediate(page, ..) => {
                for entry in page.entries() {
                    visit(file, entry.block(), depth + 1, entries)?;
                }
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    visit(file, page_ref, 0, &mut entries)?;
    entries.sort_by_key(BTreeEntry::key);
    Ok(entries)
}

fn reserve_btree_page(
    pages: &mut Vec<NewBTreePage>,
    next_page: &mut UnicodePageId,
    next_offset: &mut u64,
    data: NewBTreePageData,
) -> io::Result<UnicodePageRef> {
    let id = *next_page;
    *next_page = next_page.next()?;
    let offset = reserve_file_range(next_offset, PAGE_SIZE as u64, PAGE_SIZE as u64);
    let page_ref = UnicodePageRef::new(id, UnicodeByteIndex::new(offset));
    pages.push(NewBTreePage { page_ref, data });
    Ok(page_ref)
}

fn plan_intermediate_btree_pages(
    page_type: PageType,
    mut children: Vec<(u64, UnicodePageRef)>,
    pages: &mut Vec<NewBTreePage>,
    next_page: &mut UnicodePageId,
    next_offset: &mut u64,
) -> io::Result<UnicodePageRef> {
    let mut level = 1_u8;
    while children.len() > 1 {
        if level > 8 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unicode PST B-tree exceeds eight levels",
            ));
        }
        children = children
            .chunks(20)
            .map(|chunk| {
                let key = chunk[0].0;
                let entries = chunk
                    .iter()
                    .map(|(key, page_ref)| UnicodeBTreePageEntry::new(*key, *page_ref))
                    .collect();
                let page_ref = reserve_btree_page(
                    pages,
                    next_page,
                    next_offset,
                    NewBTreePageData::Intermediate(page_type, level, entries),
                )?;
                Ok((key, page_ref))
            })
            .collect::<io::Result<Vec<_>>>()?;
        level += 1;
    }
    Ok(children[0].1)
}

fn plan_block_btree(
    entries: &[UnicodeBlockBTreeEntry],
    pages: &mut Vec<NewBTreePage>,
    next_page: &mut UnicodePageId,
    next_offset: &mut u64,
) -> io::Result<UnicodePageRef> {
    let children = entries
        .chunks(20)
        .map(|chunk| {
            let key = chunk[0].key();
            let page_ref = reserve_btree_page(
                pages,
                next_page,
                next_offset,
                NewBTreePageData::BlockLeaf(chunk.to_vec()),
            )?;
            Ok((key, page_ref))
        })
        .collect::<io::Result<Vec<_>>>()?;
    plan_intermediate_btree_pages(
        PageType::BlockBTree,
        children,
        pages,
        next_page,
        next_offset,
    )
}

fn plan_node_btree(
    entries: &[UnicodeNodeBTreeEntry],
    pages: &mut Vec<NewBTreePage>,
    next_page: &mut UnicodePageId,
    next_offset: &mut u64,
) -> io::Result<UnicodePageRef> {
    let children = entries
        .chunks(15)
        .map(|chunk| {
            let key = chunk[0].key();
            let page_ref = reserve_btree_page(
                pages,
                next_page,
                next_offset,
                NewBTreePageData::NodeLeaf(chunk.to_vec()),
            )?;
            Ok((key, page_ref))
        })
        .collect::<io::Result<Vec<_>>>()?;
    plan_intermediate_btree_pages(PageType::NodeBTree, children, pages, next_page, next_offset)
}

fn write_btree_page(file: &mut File, page: NewBTreePage) -> io::Result<()> {
    let page_type = match &page.data {
        NewBTreePageData::BlockLeaf(_) => PageType::BlockBTree,
        NewBTreePageData::NodeLeaf(_) => PageType::NodeBTree,
        NewBTreePageData::Intermediate(page_type, ..) => *page_type,
    };
    let trailer = UnicodePageTrailer::new(
        page_type,
        page_type.signature(
            page.page_ref.index().index(),
            page.page_ref.block().into_u64(),
        ),
        page.page_ref.block(),
        0,
    );
    match page.data {
        NewBTreePageData::BlockLeaf(entries) => UnicodeBlockBTree::Leaf(Box::new(
            UnicodeBlockBTreePage::new(0, 20, 24, &entries, trailer)?,
        ))
        .write(file, page.page_ref),
        NewBTreePageData::NodeLeaf(entries) => UnicodeNodeBTree::Leaf(Box::new(
            UnicodeNodeBTreePage::new(0, 15, 32, &entries, trailer)?,
        ))
        .write(file, page.page_ref),
        NewBTreePageData::Intermediate(_, level, entries) => {
            file.seek(SeekFrom::Start(page.page_ref.index().index()))?;
            let page = UnicodeBTreeEntryPage::new(level, 20, 24, &entries, trailer)?;
            <UnicodeBTreeEntryPage as UnicodeBTreePageReadWrite<UnicodeBTreePageEntry>>::write(
                &page, file,
            )
        }
    }
}

fn push_block(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    internal: bool,
    data: NewBlockData,
) -> io::Result<UnicodeBlockId> {
    let id = UnicodeBlockId::new(internal, *next_bid)?;
    *next_bid += 1;
    push_block_with_id(blocks, next_offset, id, data)?;
    Ok(id)
}

fn push_block_with_id(
    blocks: &mut Vec<NewBlock>,
    next_offset: &mut u64,
    id: UnicodeBlockId,
    data: NewBlockData,
) -> io::Result<()> {
    let size = match &data {
        NewBlockData::Data(data) => u16::try_from(data.len()),
        NewBlockData::DataTree(entries, ..) => u16::try_from(
            usize::from(DataTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeDataTreeEntry::ENTRY_SIZE),
        ),
        NewBlockData::SubNodes(entries) => u16::try_from(
            usize::from(UnicodeSubNodeTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeLeafSubNodeTreeEntry::ENTRY_SIZE),
        ),
        NewBlockData::IntermediateSubNodes(entries, ..) => u16::try_from(
            usize::from(UnicodeSubNodeTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeIntermediateSubNodeTreeEntry::ENTRY_SIZE),
        ),
    }
    .map_err(|_| PstError::IntegerConversion)?;
    UnicodeBlockTrailer::new(size, 0, 0, id)?;
    let disk_size = u64::from(ndb::block::block_size(size + UnicodeBlockTrailer::SIZE));
    let offset = reserve_file_range(next_offset, disk_size, 64);
    blocks.push(NewBlock {
        id,
        offset,
        size,
        data,
    });
    Ok(())
}

fn map_page_count(amap_index: u64) -> u64 {
    let has_pmap = amap_index % PMAP_PAGE_COUNT == 0;
    let has_fmap = has_pmap
        && amap_index >= FMAP_FIRST_SIZE
        && (amap_index - FMAP_FIRST_SIZE) % FMAP_PAGE_COUNT == 0;
    let has_fpmap = has_pmap
        && amap_index >= FPMAP_FIRST_SIZE
        && (amap_index - FPMAP_FIRST_SIZE) % FPMAP_PAGE_COUNT == 0;
    // FPMaps start three pages after their AMap, so reserve all four pages through the FPMap.
    1 + u64::from(has_pmap) + u64::from(has_fmap) + 2 * u64::from(has_fpmap)
}

fn reserve_file_range(next_offset: &mut u64, size: u64, alignment: u64) -> u64 {
    let mut offset = next_offset.div_ceil(alignment) * alignment;
    loop {
        if offset < AMAP_FIRST_OFFSET {
            *next_offset = offset + size;
            return offset;
        }

        let amap_index = (offset - AMAP_FIRST_OFFSET) / AMAP_DATA_SIZE;
        let amap_offset = AMAP_FIRST_OFFSET + amap_index * AMAP_DATA_SIZE;
        let reserved_end = amap_offset + map_page_count(amap_index) * PAGE_SIZE as u64;
        if offset < reserved_end {
            offset = reserved_end.div_ceil(alignment) * alignment;
            continue;
        }

        let next_amap = amap_offset + AMAP_DATA_SIZE;
        if offset + size > next_amap {
            let next_index = amap_index + 1;
            offset = (next_amap + map_page_count(next_index) * PAGE_SIZE as u64)
                .div_ceil(alignment)
                * alignment;
            continue;
        }

        *next_offset = offset + size;
        return offset;
    }
}

fn push_data_tree(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    data: &[u8],
) -> io::Result<UnicodeBlockId> {
    push_data_tree_chunks(
        blocks,
        next_bid,
        next_offset,
        data.chunks(MAX_DATA_BLOCK_SIZE).map(<[u8]>::to_vec),
    )
}

fn push_data_tree_chunks(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    chunks: impl IntoIterator<Item = Vec<u8>>,
) -> io::Result<UnicodeBlockId> {
    let mut chunks = chunks.into_iter().collect::<Vec<_>>();
    if chunks.is_empty() || chunks.iter().any(Vec::is_empty) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data tree cannot contain empty blocks",
        ));
    }
    if chunks.iter().any(|chunk| chunk.len() > MAX_DATA_BLOCK_SIZE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data tree block exceeds the Unicode PST block limit",
        ));
    }
    let intermediate_count = chunks.len().saturating_sub(1);
    for chunk in chunks.iter_mut().take(intermediate_count) {
        chunk.resize(MAX_DATA_BLOCK_SIZE, 0);
    }
    let mut ids = chunks
        .into_iter()
        .map(|chunk| {
            let size = u32::try_from(chunk.len()).map_err(|_| PstError::IntegerConversion)?;
            let id = push_block(
                blocks,
                next_bid,
                next_offset,
                false,
                NewBlockData::Data(chunk),
            )?;
            Ok((id, size))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut level = 1;
    while ids.len() > 1 {
        ids = ids
            .chunks(1021)
            .map(|entries| {
                let total_size = entries.iter().try_fold(0_u32, |total, (_, size)| {
                    total.checked_add(*size).ok_or(PstError::IntegerConversion)
                })?;
                let id = push_block(
                    blocks,
                    next_bid,
                    next_offset,
                    true,
                    NewBlockData::DataTree(
                        entries
                            .iter()
                            .map(|(id, _)| UnicodeDataTreeEntry::from(*id))
                            .collect(),
                        level,
                        total_size,
                    ),
                )?;
                Ok((id, total_size))
            })
            .collect::<io::Result<Vec<_>>>()?;
        level += 1;
    }
    Ok(ids[0].0)
}

fn push_recipient_table(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    recipients: &[UnicodePstRecipient<'_>],
    row_version: u32,
) -> io::Result<(UnicodeBlockId, Option<UnicodeBlockId>)> {
    let (pages, rows) = recipient_table_data(recipients, row_version)?;
    let data = push_data_tree_chunks(blocks, next_bid, next_offset, pages)?;
    let subnodes = if let Some((rows_node, row_chunks)) = rows {
        let rows = push_data_tree_chunks(blocks, next_bid, next_offset, row_chunks)?;
        Some(push_subnode_tree(
            blocks,
            next_bid,
            next_offset,
            vec![UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None)],
        )?)
    } else {
        None
    };
    Ok((data, subnodes))
}

fn push_subnode_tree(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    mut entries: Vec<UnicodeLeafSubNodeTreeEntry>,
) -> io::Result<UnicodeBlockId> {
    const LEAF_CAPACITY: usize = (MAX_DATA_BLOCK_SIZE
        - UnicodeSubNodeTreeBlockHeader::HEADER_SIZE as usize)
        / UnicodeLeafSubNodeTreeEntry::ENTRY_SIZE as usize;
    const INTERMEDIATE_CAPACITY: usize = (MAX_DATA_BLOCK_SIZE
        - UnicodeSubNodeTreeBlockHeader::HEADER_SIZE as usize)
        / UnicodeIntermediateSubNodeTreeEntry::ENTRY_SIZE as usize;

    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "subnode tree cannot be empty",
        ));
    }
    entries.sort_by_key(|entry| u32::from(entry.node()));
    let mut level_entries = entries
        .chunks(LEAF_CAPACITY)
        .map(|chunk| {
            let first = chunk[0].node();
            let block = push_block(
                blocks,
                next_bid,
                next_offset,
                true,
                NewBlockData::SubNodes(chunk.to_vec()),
            )?;
            Ok((first, block))
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut level = 1;
    while level_entries.len() > 1 {
        level_entries = level_entries
            .chunks(INTERMEDIATE_CAPACITY)
            .map(|chunk| {
                let first = chunk[0].0;
                let entries = chunk
                    .iter()
                    .map(|(node, block)| UnicodeIntermediateSubNodeTreeEntry::new(*node, *block))
                    .collect();
                let block = push_block(
                    blocks,
                    next_bid,
                    next_offset,
                    true,
                    NewBlockData::IntermediateSubNodes(entries, level),
                )?;
                Ok((first, block))
            })
            .collect::<io::Result<Vec<_>>>()?;
        level += 1;
    }
    Ok(level_entries[0].1)
}

fn write_new_block(file: &mut File, block: NewBlock) -> io::Result<()> {
    let block_signature = block_sig::compute_sig(block.offset as u32, u64::from(block.id) as u32);
    let trailer = UnicodeBlockTrailer::new(block.size, block_signature, 0, block.id)?;
    file.seek(SeekFrom::Start(block.offset))?;
    match block.data {
        NewBlockData::Data(data) => {
            UnicodeDataBlock::new(NdbCryptMethod::None, data, trailer)?.write(file)
        }
        NewBlockData::DataTree(entries, level, total_size) => {
            let header = DataTreeBlockHeader::new(
                level,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
                total_size,
            );
            UnicodeDataTreeBlock::new(header, entries, trailer)?.write(file)
        }
        NewBlockData::SubNodes(entries) => {
            let header = UnicodeSubNodeTreeBlockHeader::new(
                0,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
            );
            let tree = UnicodeLeafSubNodeTreeBlock::new(header, entries, trailer)?;
            SubNodeTree::<UnicodePstFile>::Leaf(Box::new(tree)).write(
                file,
                &UnicodeBlockBTreeEntry::new(
                    UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                    block.size,
                ),
            )
        }
        NewBlockData::IntermediateSubNodes(entries, level) => {
            let header = UnicodeSubNodeTreeBlockHeader::new(
                level,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
            );
            let tree = UnicodeIntermediateSubNodeTreeBlock::new(header, entries, trailer)?;
            SubNodeTree::<UnicodePstFile>::Intermediate(Box::new(tree)).write(
                file,
                &UnicodeBlockBTreeEntry::new(
                    UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                    block.size,
                ),
            )
        }
    }
}

fn push_attachments(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    sub_nodes: &mut Vec<UnicodeLeafSubNodeTreeEntry>,
) -> io::Result<()> {
    if attachments.is_empty() {
        return Ok(());
    }
    let attachment_table = NID_ATTACHMENT_TABLE;
    let attachment_ids = (0..attachments.len())
        .map(|index| {
            let index = u32::try_from(index).map_err(|_| PstError::IntegerConversion)?;
            Ok(NodeId::new(NodeIdType::Attachment, 0x100 + index)?)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let (table_pages, table_rows) =
        attachment_table_data(&attachment_ids, attachments, row_version)?;
    let table_block = push_data_tree_chunks(blocks, next_bid, next_offset, table_pages)?;
    let table_subnodes = if let Some((rows_node, rows)) = table_rows {
        let rows = push_data_tree_chunks(blocks, next_bid, next_offset, rows)?;
        Some(push_subnode_tree(
            blocks,
            next_bid,
            next_offset,
            vec![UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None)],
        )?)
    } else {
        None
    };
    sub_nodes.push(UnicodeLeafSubNodeTreeEntry::new(
        attachment_table,
        table_block,
        table_subnodes,
    ));

    for (index, (attachment_id, attachment)) in
        attachment_ids.into_iter().zip(attachments).enumerate()
    {
        let data_node = if attachment.data.is_empty() {
            None
        } else {
            Some(NodeId::new(
                NodeIdType::Internal,
                0x200 + u32::try_from(index).map_err(|_| PstError::IntegerConversion)?,
            )?)
        };
        let attachment_subnode = if let Some(data_node) = data_node {
            let data_block = push_data_tree(blocks, next_bid, next_offset, attachment.data)?;
            Some(push_block(
                blocks,
                next_bid,
                next_offset,
                true,
                NewBlockData::SubNodes(vec![UnicodeLeafSubNodeTreeEntry::new(
                    data_node, data_block, None,
                )]),
            )?)
        } else {
            None
        };
        let attachment_block = push_block(
            blocks,
            next_bid,
            next_offset,
            false,
            NewBlockData::Data(attachment_data(attachment, data_node)?),
        )?;
        sub_nodes.push(UnicodeLeafSubNodeTreeEntry::new(
            attachment_id,
            attachment_block,
            attachment_subnode,
        ));
    }
    Ok(())
}

fn finish_unicode_pst_metadata(path: &Path) -> io::Result<()> {
    let mut pst = UnicodePstFile::open(path)?;
    pst.inner.rebuild_allocation_map()?;
    pst.inner.ensure_density_list()?;
    pst.inner.finish_write()?;
    drop(pst);
    OpenOptions::new().write(true).open(path)?.sync_all()
}

fn create_unicode_pst(
    path: &Path,
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    folder_path: Option<&[&str]>,
) -> io::Result<()> {
    if let Some(folder_path) = folder_path {
        validate_folder_path(folder_path)?;
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    let setup = (|| {
        file.write_all(OUTLOOK_UNICODE_PST_TEMPLATE)?;
        file.sync_data()?;

        file.seek(SeekFrom::Start(0))?;
        let mut header = UnicodeHeader::read(&mut file)?;
        let entries = collect_block_btree_entries(&mut file, *header.root().block_btree())?;
        for entry in entries
            .iter()
            .filter(|entry| !entry.block().block().is_internal())
        {
            file.seek(SeekFrom::Start(entry.block().index().index()))?;
            let block = UnicodeDataBlock::read(&mut file, entry.size(), header.crypt_method())?;
            file.seek(SeekFrom::Start(entry.block().index().index()))?;
            UnicodeDataBlock::new(
                NdbCryptMethod::None,
                block.data().to_vec(),
                *block.trailer(),
            )?
            .write(&mut file)?;
        }
        header.set_crypt_method(NdbCryptMethod::None);
        file.seek(SeekFrom::Start(0))?;
        header.write(&mut file)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = setup {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }

    if let Err(error) = ensure_unicode_folder_path(path, &["Inbox"]) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    let inbox = ["Inbox"];
    let result = append_unicode_pst(
        path,
        &[UnicodePstBatchMessage {
            message: *input,
            attachments,
        }],
        Some(folder_path.unwrap_or(&inbox)),
    );
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}
fn default_receive_folder(
    store: &Rc<UnicodeStore>,
    nbt_entries: &[UnicodeNodeBTreeEntry],
) -> io::Result<NodeId> {
    let ipm = store.open_folder(&store.properties().ipm_sub_tree_entry_id()?)?;
    let hierarchy = ipm
        .hierarchy_table()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "IPM hierarchy table not found"))?;
    for row in hierarchy.rows_matrix() {
        let node = NodeId::from(u32::from(row.id()));
        let folder = store.open_folder(&store.properties().make_entry_id(node)?)?;
        if folder
            .properties()
            .display_name()
            .is_ok_and(|name| name.eq_ignore_ascii_case("Inbox"))
        {
            return Ok(node);
        }
    }

    let mut best = None;
    for entry in nbt_entries
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::ReceiveFolderTable)))
    {
        let Ok(table) = UnicodeTableContext::read(store.clone(), *entry) else {
            continue;
        };
        for row in table.rows_matrix() {
            let values = row.columns(table.context())?;
            let mut message_class = None;
            let mut folder = None;
            for (column, value) in table.context().columns().iter().zip(values) {
                let Some(value) = value else { continue };
                if matches!(&value, TableRowColumnValue::Heap(id) if u32::from(*id) == 0)
                    || matches!(&value, TableRowColumnValue::Node(id) if u32::from(*id) == 0)
                {
                    continue;
                }
                match column.prop_id() {
                    0x001A => match table.read_column(&value, column.prop_type())? {
                        PropertyValue::String8(value) => message_class = Some(value.to_string()),
                        PropertyValue::Unicode(value) => message_class = Some(value.to_string()),
                        _ => {}
                    },
                    0x6605 => {
                        if let PropertyValue::Integer32(value) =
                            table.read_column(&value, column.prop_type())?
                        {
                            if value != 0 {
                                folder = Some(NodeId::from(value as u32));
                            }
                        }
                    }
                    0x0FFF => {
                        if let PropertyValue::Binary(value) =
                            table.read_column(&value, column.prop_type())?
                        {
                            folder = Some(EntryId::try_from(value.buffer())?.node_id())
                        }
                    }
                    _ => {}
                }
            }
            let Some(folder) = folder else { continue };
            let class = message_class.unwrap_or_default();
            let rank = if class.eq_ignore_ascii_case("IPM.Note") {
                3
            } else if class.eq_ignore_ascii_case("IPM") {
                2
            } else if class.is_empty() {
                1
            } else {
                0
            };
            if rank > best.map(|(rank, _)| rank).unwrap_or(0) {
                best = Some((rank, folder));
            }
        }
    }
    if let Some((_, inbox)) = best {
        return Ok(inbox);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "default receive-folder mapping not found",
    ))
}

fn find_child_folder(
    store: &Rc<UnicodeStore>,
    parent: NodeId,
    name: &str,
) -> io::Result<Option<NodeId>> {
    let parent = store.open_folder(&store.properties().make_entry_id(parent)?)?;
    let Some(hierarchy) = parent.hierarchy_table() else {
        return Ok(None);
    };
    for row in hierarchy.rows_matrix() {
        let child = NodeId::from(u32::from(row.id()));
        let child_folder = store.open_folder(&store.properties().make_entry_id(child)?)?;
        if child_folder
            .properties()
            .display_name()
            .is_ok_and(|display_name| display_name.eq_ignore_ascii_case(name))
        {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

fn new_folder_table_cell(
    column: &TableColumnDescriptor,
    store_record_key: [u8; 16],
    folder: NodeId,
    name: &str,
    has_subfolders: bool,
    row_version: u32,
) -> io::Result<Option<TableCell>> {
    let prop_type = column.prop_type();
    let cell = match column.prop_id() {
        LTP_ROW_ID_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(u32::from(folder) as i32),
        )),
        LTP_ROW_VERSION_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(row_version as i32),
        )),
        0x3001 => string_table_cell(prop_type, name),
        0x3602 | 0x3603 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(0)))
        }
        0x360A if prop_type == PropertyType::Boolean => {
            Some(TableCell::Value(PropertyValue::Boolean(has_subfolders)))
        }
        0x3613 => string_table_cell(prop_type, "IPF.Note"),
        0x0FFF if prop_type == PropertyType::Binary => {
            let entry_id = EntryId::new(StoreRecordKey::new(store_record_key), folder);
            Some(TableCell::Bytes(Vec::try_from(&entry_id)?))
        }
        _ => None,
    };
    Ok(cell)
}

fn rebuild_hierarchy_table(
    table: &dyn TableContext,
    store_record_key: [u8; 16],
    folder: NodeId,
    name: &str,
    has_subfolders: bool,
    rows_node: NodeId,
    row_version: u32,
) -> io::Result<RebuiltTable> {
    let cells = table
        .context()
        .columns()
        .iter()
        .map(|column| {
            new_folder_table_cell(
                column,
                store_record_key,
                folder,
                name,
                has_subfolders,
                row_version,
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    rebuild_table(
        table,
        vec![(folder, cells)],
        Some(rows_node),
        row_version,
        None,
        true,
    )
}

fn table_rows_node(
    table: &dyn TableContext,
    subnodes: &[UnicodeLeafSubNodeTreeEntry],
) -> io::Result<NodeId> {
    table
        .context()
        .rows()
        .filter(|node| !matches!(node.id_type(), Ok(NodeIdType::HeapNode)))
        .map(Ok)
        .unwrap_or_else(|| {
            let index = subnodes
                .iter()
                .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::Internal)))
                .map(|entry| entry.node().index())
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(NodeIdType::Internal, index)?)
        })
}

fn writable_folder_properties(folder: &dyn Folder) -> Vec<(u16, PropertyValue)> {
    folder
        .properties()
        .iter()
        .filter(|(id, _)| !matches!(**id, 0x0FFF | 0x3601))
        .map(|(id, value)| (*id, value.clone()))
        .collect()
}

fn set_folder_integer_property(properties: &mut Vec<(u16, PropertyValue)>, id: u16, value: i32) {
    if let Some((_, property)) = properties.iter_mut().find(|(property, _)| *property == id) {
        *property = PropertyValue::Integer32(value);
    } else {
        properties.push((id, PropertyValue::Integer32(value)));
    }
}

fn set_folder_boolean_property(properties: &mut Vec<(u16, PropertyValue)>, id: u16, value: bool) {
    if let Some((_, property)) = properties.iter_mut().find(|(property, _)| *property == id) {
        *property = PropertyValue::Boolean(value);
    } else {
        properties.push((id, PropertyValue::Boolean(value)));
    }
}

fn collect_subnode_entries(
    file: &mut File,
    bbt_ref: UnicodePageRef,
    subnode: Option<UnicodeBlockId>,
) -> io::Result<Vec<UnicodeLeafSubNodeTreeEntry>> {
    let Some(subnode) = subnode else {
        return Ok(Vec::new());
    };
    let bbt = UnicodeBlockBTree::read(file, bbt_ref)?;
    let mut cache = Default::default();
    let block = bbt.find_entry(file, subnode.search_key(), &mut cache)?;
    let tree = SubNodeTree::<UnicodePstFile>::read(file, &block)?;
    Ok(tree.entries(file, &bbt, &mut cache)?.collect())
}

fn write_pending_blocks(
    file: &mut File,
    bbt_entries: &mut Vec<UnicodeBlockBTreeEntry>,
    blocks: &mut Vec<NewBlock>,
) -> io::Result<()> {
    for block in blocks.drain(..) {
        bbt_entries.push(UnicodeBlockBTreeEntry::new(
            UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
            block.size,
        ));
        write_new_block(file, block)?;
    }
    Ok(())
}

fn append_search_updates(
    store: &Rc<UnicodeStore>,
    nbt_entries: &mut Vec<UnicodeNodeBTreeEntry>,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut u64,
    updates: impl IntoIterator<Item = SearchUpdateData>,
) -> io::Result<()> {
    let Some(queue_entry) = nbt_entries
        .iter()
        .find(|entry| entry.node() == NID_SEARCH_MANAGEMENT_QUEUE)
        .copied()
    else {
        return Ok(());
    };
    let mut data = Vec::new();
    for update in store
        .search_update_queue()?
        .updates()
        .iter()
        .copied()
        .chain(updates.into_iter().map(SearchUpdate::new))
    {
        update.write(&mut data)?;
    }
    let data = push_data_tree(blocks, next_bid, next_offset, &data)?;
    nbt_entries.retain(|entry| entry.node() != NID_SEARCH_MANAGEMENT_QUEUE);
    nbt_entries.push(UnicodeNodeBTreeEntry::new(
        NID_SEARCH_MANAGEMENT_QUEUE,
        data,
        queue_entry.sub_node(),
        queue_entry.parent(),
    ));
    Ok(())
}

fn record_block_reference(
    block: UnicodeBlockId,
    references: &mut BTreeMap<u64, u16>,
    pending: &mut VecDeque<UnicodeBlockId>,
) -> io::Result<()> {
    let key = block.search_key();
    if key == 0 {
        return Ok(());
    }
    let count = references.entry(key).or_default();
    if *count == 0 {
        pending.push_back(block);
    }
    *count = count.checked_add(1).ok_or(PstError::IntegerConversion)?;
    Ok(())
}

fn compact_unicode_block_tree(
    file: &mut File,
    entries: &mut Vec<UnicodeBlockBTreeEntry>,
    nodes: &[UnicodeNodeBTreeEntry],
) -> io::Result<()> {
    entries.sort_by_key(BTreeEntry::key);
    let mut references = BTreeMap::new();
    let mut pending = VecDeque::new();
    for node in nodes {
        record_block_reference(node.data(), &mut references, &mut pending)?;
        if let Some(block) = node.sub_node() {
            record_block_reference(block, &mut references, &mut pending)?;
        }
    }

    while let Some(block) = pending.pop_front() {
        let entry = entries
            .binary_search_by_key(&block.search_key(), BTreeEntry::key)
            .ok()
            .and_then(|index| entries.get(index))
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("referenced PST block {:X} is missing", block.search_key()),
                )
            })?;
        if !block.is_internal() {
            continue;
        }

        file.seek(SeekFrom::Start(entry.block().index().index()))?;
        let mut block_type = [0];
        file.read_exact(&mut block_type)?;
        let children: Vec<UnicodeBlockId> = match block_type[0] {
            0x01 => match UnicodeDataTree::read(file, NdbCryptMethod::None, &entry)? {
                DataTree::Intermediate(tree) => {
                    tree.entries().iter().map(|entry| entry.block()).collect()
                }
                DataTree::Leaf(_) => unreachable!(),
            },
            0x02 => match UnicodeSubNodeTree::read(file, &entry)? {
                SubNodeTree::Intermediate(tree) => {
                    tree.entries().iter().map(|entry| entry.block()).collect()
                }
                SubNodeTree::Leaf(tree) => tree
                    .entries()
                    .iter()
                    .flat_map(|entry| [Some(entry.block()), entry.sub_node()])
                    .flatten()
                    .collect(),
            },
            value => return Err(NdbError::InvalidInternalBlockType(value).into()),
        };
        for child in children {
            record_block_reference(child, &mut references, &mut pending)?;
        }
    }

    for entry in &mut *entries {
        if let Some(references) = references.get(&entry.key()) {
            entry.set_ref_count(
                references
                    .checked_add(1)
                    .ok_or(PstError::IntegerConversion)?,
            );
        }
    }
    entries.retain(|entry| references.contains_key(&entry.key()));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_unicode_changes(
    path: &Path,
    mut file: File,
    mut header: UnicodeHeader,
    mut bbt_entries: Vec<UnicodeBlockBTreeEntry>,
    mut nbt_entries: Vec<UnicodeNodeBTreeEntry>,
    blocks: Vec<NewBlock>,
    next_bid: u64,
    mut next_offset: u64,
    reserved_nodes: &[NodeId],
) -> io::Result<()> {
    bbt_entries.extend(blocks.iter().map(|block| {
        UnicodeBlockBTreeEntry::new(
            UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
            block.size,
        )
    }));
    nbt_entries.sort_by_key(BTreeEntry::key);

    header.root_mut().set_amap_status(AmapStatus::Invalid);
    header.update_unique();
    file.seek(SeekFrom::Start(0))?;
    header.write(&mut file)?;
    file.sync_data()?;
    for block in blocks {
        write_new_block(&mut file, block)?;
    }
    file.sync_data()?;
    compact_unicode_block_tree(&mut file, &mut bbt_entries, &nbt_entries)?;

    let mut pages = Vec::new();
    let mut next_page = header.next_page();
    let bbt_ref = plan_block_btree(&bbt_entries, &mut pages, &mut next_page, &mut next_offset)?;
    let nbt_ref = plan_node_btree(&nbt_entries, &mut pages, &mut next_page, &mut next_offset)?;
    let new_eof = next_offset;
    let amap_last =
        AMAP_FIRST_OFFSET + ((new_eof - AMAP_FIRST_OFFSET - 1) / AMAP_DATA_SIZE) * AMAP_DATA_SIZE;

    for page in pages {
        write_btree_page(&mut file, page)?;
    }
    file.set_len(new_eof)?;
    file.sync_data()?;

    header.set_next_block(UnicodeBlockId::new(false, next_bid)?);
    header.set_next_page(next_page);
    for node in reserved_nodes {
        header.reserve_node(*node)?;
    }
    header.root_mut().set_btrees(nbt_ref, bbt_ref);
    header
        .root_mut()
        .set_file_eof_index(UnicodeByteIndex::new(new_eof));
    header
        .root_mut()
        .set_amap_last_index(UnicodeByteIndex::new(amap_last));
    header.update_unique();
    file.seek(SeekFrom::Start(0))?;
    header.write(&mut file)?;
    file.sync_all()?;
    drop(file);
    finish_unicode_pst_metadata(path)
}

fn create_unicode_folder(path: &Path, parent: NodeId, name: &str) -> io::Result<NodeId> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let header = UnicodeHeader::read(&mut file)?;
    if header.crypt_method() != NdbCryptMethod::None {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "folder creation only supports unencrypted Unicode PSTs",
        ));
    }
    let old_root = header.root().clone();
    let bbt_entries = collect_block_btree_entries(&mut file, *old_root.block_btree())?;
    let mut nbt_entries = collect_node_btree_entries(&mut file, *old_root.node_btree())?;
    let pst = Rc::new(UnicodePstFile::open(path)?);
    let store = UnicodeStore::read(pst)?;
    let store_record_key = *store.properties().record_key()?.record_key();
    let parent_folder = store.open_folder(&store.properties().make_entry_id(parent)?)?;
    let parent_has_subfolders = parent_folder.properties().has_sub_folders()?;
    let parent_entry = nbt_entries
        .iter()
        .find(|entry| entry.node() == parent)
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "parent folder node not found"))?;
    let mut parent_properties = writable_folder_properties(parent_folder.as_ref());
    if !parent_has_subfolders {
        set_folder_boolean_property(&mut parent_properties, 0x360A, true);
    }
    let mut parent_subnodes =
        collect_subnode_entries(&mut file, *old_root.block_btree(), parent_entry.sub_node())?;
    let parent_row_hierarchy = (!parent_has_subfolders)
        .then(|| -> io::Result<_> {
            let grandparent = parent_entry.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "parent folder has no parent")
            })?;
            let node = NodeId::new(NodeIdType::HierarchyTable, grandparent.index())?;
            let entry = nbt_entries
                .iter()
                .find(|entry| entry.node() == node)
                .copied()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "parent hierarchy node not found")
                })?;
            let grandparent = store.open_folder(&store.properties().make_entry_id(grandparent)?)?;
            let table = grandparent.hierarchy_table().cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "parent hierarchy table not found")
            })?;
            let subnodes =
                collect_subnode_entries(&mut file, *old_root.block_btree(), entry.sub_node())?;
            let rows_node = table_rows_node(table.as_ref(), &subnodes)?;
            Ok((node, entry, table, subnodes, rows_node))
        })
        .transpose()?;

    let folder_index = nbt_entries
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::NormalFolder)))
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?
        .max(header.next_node_index(NodeIdType::NormalFolder)?);
    let folder = NodeId::new(NodeIdType::NormalFolder, folder_index)?;
    let hierarchy = NodeId::new(NodeIdType::HierarchyTable, parent.index())?;
    let hierarchy_entry = nbt_entries
        .iter()
        .find(|entry| entry.node() == hierarchy)
        .copied();
    let hierarchy_template = nbt_entries
        .iter()
        .find(|entry| entry.node() == NodeId::from(0x60D))
        .map(NodeBTreeEntry::data);
    let contents_template = nbt_entries
        .iter()
        .find(|entry| entry.node() == NodeId::from(0x60E))
        .map(NodeBTreeEntry::data);
    let associated_contents_template = nbt_entries
        .iter()
        .find(|entry| entry.node() == NodeId::from(0x60F))
        .map(NodeBTreeEntry::data);

    let mut next_bid = header.next_block().index().max(
        bbt_entries
            .iter()
            .map(|entry| entry.block().block().index())
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut next_offset = old_root.file_eof_index().index();
    let mut blocks = Vec::new();

    let (hierarchy_data_bid, hierarchy_subnode_bid) =
        if let (Some(hierarchy_entry), Some(hierarchy_table)) =
            (hierarchy_entry, parent_folder.hierarchy_table())
        {
            let mut hierarchy_subnodes = collect_subnode_entries(
                &mut file,
                *old_root.block_btree(),
                hierarchy_entry.sub_node(),
            )?;
            let rows_node = table_rows_node(hierarchy_table.as_ref(), &hierarchy_subnodes)?;
            let (pages, rows, _) = rebuild_hierarchy_table(
                hierarchy_table.as_ref(),
                store_record_key,
                folder,
                name,
                false,
                rows_node,
                header.unique_value(),
            )?;
            let data = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, pages)?;
            let rows = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, rows)?;
            hierarchy_subnodes.retain(|entry| entry.node() != rows_node);
            hierarchy_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None));
            let subnodes = push_subnode_tree(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                hierarchy_subnodes,
            )?;
            (data, Some(subnodes))
        } else {
            let data = push_block(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                false,
                NewBlockData::Data(table_context_data(&[folder])?),
            )?;
            (data, None)
        };

    let (parent_data, parent_subnode) = if parent_has_subfolders {
        (parent_entry.data(), parent_entry.sub_node())
    } else {
        let next_property_subnode = parent_subnodes
            .iter()
            .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::Internal)))
            .map(|entry| entry.node().index())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(PstError::IntegerConversion)?;
        let (parent_pages, external_properties) =
            property_context_pages(parent_properties, next_property_subnode)?;
        let parent_data =
            push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, parent_pages)?;
        for (node, data) in external_properties {
            let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
            parent_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
        }
        let parent_subnode = if parent_subnodes.is_empty() {
            None
        } else {
            Some(push_subnode_tree(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                parent_subnodes,
            )?)
        };
        (parent_data, parent_subnode)
    };

    let folder_data_bid = push_block(
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        false,
        NewBlockData::Data(folder_data(name, 0, false)?),
    )?;
    let folder_hierarchy = NodeId::new(NodeIdType::HierarchyTable, folder.index())?;
    let folder_hierarchy_bid = match hierarchy_template {
        Some(template) => template,
        None => push_block(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            false,
            NewBlockData::Data(table_context_data(&[])?),
        )?,
    };
    let folder_contents = NodeId::new(NodeIdType::ContentsTable, folder.index())?;
    let folder_contents_bid = match contents_template {
        Some(template) => template,
        None => push_block(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            false,
            NewBlockData::Data(table_context_data(&[])?),
        )?,
    };
    let folder_associated_contents =
        NodeId::new(NodeIdType::AssociatedContentsTable, folder.index())?;
    let folder_associated_contents_bid = match associated_contents_template {
        Some(template) => template,
        None => push_block(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            false,
            NewBlockData::Data(table_context_data(&[])?),
        )?,
    };

    let parent_row_hierarchy_entry = if let Some((node, entry, table, mut subnodes, rows_node)) =
        parent_row_hierarchy
    {
        let (pages, rows, _) = rebuild_table(
            table.as_ref(),
            vec![],
            Some(rows_node),
            header.unique_value(),
            Some((parent, 0x360A, PropertyValue::Boolean(true))),
            true,
        )?;
        let data = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, pages)?;
        let rows = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, rows)?;
        subnodes.retain(|entry| entry.node() != rows_node);
        subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None));
        let subnodes = push_subnode_tree(&mut blocks, &mut next_bid, &mut next_offset, subnodes)?;
        Some((
            node,
            UnicodeNodeBTreeEntry::new(node, data, Some(subnodes), entry.parent()),
        ))
    } else {
        None
    };

    nbt_entries.retain(|entry| entry.node() != parent && entry.node() != hierarchy);
    if let Some((node, entry)) = parent_row_hierarchy_entry {
        nbt_entries.retain(|candidate| candidate.node() != node);
        nbt_entries.push(entry);
    }
    nbt_entries.extend([
        UnicodeNodeBTreeEntry::new(parent, parent_data, parent_subnode, parent_entry.parent()),
        UnicodeNodeBTreeEntry::new(
            hierarchy,
            hierarchy_data_bid,
            hierarchy_subnode_bid,
            hierarchy_entry.and_then(|entry| entry.parent()),
        ),
        UnicodeNodeBTreeEntry::new(folder, folder_data_bid, None, Some(parent)),
        UnicodeNodeBTreeEntry::new(folder_hierarchy, folder_hierarchy_bid, None, None),
        UnicodeNodeBTreeEntry::new(folder_contents, folder_contents_bid, None, None),
        UnicodeNodeBTreeEntry::new(
            folder_associated_contents,
            folder_associated_contents_bid,
            None,
            None,
        ),
    ]);

    append_search_updates(
        &store,
        &mut nbt_entries,
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        [
            SearchUpdateData::FolderAdded {
                parent,
                folder,
                reserved1: 0,
                reserved2: 0,
            },
            SearchUpdateData::FolderModified {
                folder: parent,
                reserved: 0,
            },
        ],
    )?;

    commit_unicode_changes(
        path,
        file,
        header,
        bbt_entries,
        nbt_entries,
        blocks,
        next_bid,
        next_offset,
        &[
            folder,
            folder_hierarchy,
            folder_contents,
            folder_associated_contents,
        ],
    )?;
    Ok(folder)
}

fn ensure_unicode_folder_path(path: &Path, folder_path: &[&str]) -> io::Result<NodeId> {
    validate_folder_path(folder_path)?;
    let pst = Rc::new(UnicodePstFile::open(path)?);
    let mut store = UnicodeStore::read(pst)?;
    let mut parent = store.properties().ipm_sub_tree_entry_id()?.node_id();
    for name in folder_path {
        if let Some(folder) = find_child_folder(&store, parent, name)? {
            parent = folder;
            continue;
        }
        // ponytail: one COW commit per missing segment; batch only if deep imports need it.
        parent = create_unicode_folder(path, parent, name)?;
        store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path)?))?;
    }
    Ok(parent)
}

fn append_unicode_pst(
    path: &Path,
    inputs: &[UnicodePstBatchMessage<'_>],
    folder_path: Option<&[&str]>,
) -> io::Result<()> {
    if inputs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bulk append requires at least one message",
        ));
    }
    let requested_folder = folder_path
        .map(|folder_path| ensure_unicode_folder_path(path, folder_path))
        .transpose()?;
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut header = UnicodeHeader::read(&mut file)?;
    if header.crypt_method() != NdbCryptMethod::None {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "append only supports unencrypted Unicode PSTs",
        ));
    }
    let old_root = header.root().clone();
    let mut bbt_entries = collect_block_btree_entries(&mut file, *old_root.block_btree())?;
    let mut nbt_entries = collect_node_btree_entries(&mut file, *old_root.node_btree())?;

    let pst = Rc::new(UnicodePstFile::open(path)?);
    let store = UnicodeStore::read(pst)
        .map_err(|err| io::Error::new(err.kind(), format!("read message store: {err}")))?;
    let store_record_key = *store
        .properties()
        .record_key()
        .map_err(|err| io::Error::new(err.kind(), format!("read store record key: {err}")))?
        .record_key();
    let folder =
        requested_folder
            .unwrap_or(default_receive_folder(&store, &nbt_entries).map_err(|err| {
                io::Error::new(err.kind(), format!("find receive folder: {err}"))
            })?);
    let target_folder = store
        .open_folder(&store.properties().make_entry_id(folder)?)
        .map_err(|err| io::Error::new(err.kind(), format!("open target folder: {err}")))?;
    let contents_table = target_folder.contents_table().cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "target-folder contents table not found",
        )
    })?;
    let mut folder_properties = writable_folder_properties(target_folder.as_ref());

    let message_index = nbt_entries
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::NormalMessage)))
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0x3F)
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?
        .max(header.next_node_index(NodeIdType::NormalMessage)?);
    let messages = (0..inputs.len())
        .map(|offset| {
            let offset = u32::try_from(offset).map_err(|_| PstError::IntegerConversion)?;
            let index = message_index
                .checked_add(offset)
                .ok_or(PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(NodeIdType::NormalMessage, index)?)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let message_sizes = inputs
        .iter()
        .zip(&messages)
        .enumerate()
        .map(|(position, (input, message))| {
            message_size(
                &input.message,
                input.attachments,
                *message,
                header.unique_value(),
            )
            .map_err(|err| {
                let file_size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                io::Error::new(
                    err.kind(),
                    format!(
                        "append message {} of {} ({}) at PST size {file_size}: {err}",
                        position + 1,
                        inputs.len(),
                        input.message.message_id,
                    ),
                )
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let contents = NodeId::new(NodeIdType::ContentsTable, folder.index())?;
    let contents_entry = nbt_entries
        .iter()
        .find(|entry| entry.node() == contents)
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "target contents node not found"))?;
    let folder_entry = nbt_entries
        .iter()
        .find(|entry| entry.node() == folder)
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "target folder node not found"))?;
    let parent_hierarchy = folder_entry
        .parent()
        .map(|parent| -> io::Result<_> {
            let node = NodeId::new(NodeIdType::HierarchyTable, parent.index())?;
            let entry = nbt_entries
                .iter()
                .find(|entry| entry.node() == node)
                .copied()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "parent hierarchy node not found")
                })?;
            let parent = store.open_folder(&store.properties().make_entry_id(parent)?)?;
            let table = parent.hierarchy_table().cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "parent hierarchy table not found")
            })?;
            let subnodes =
                collect_subnode_entries(&mut file, *old_root.block_btree(), entry.sub_node())?;
            let rows_node = table_rows_node(table.as_ref(), &subnodes)?;
            Ok((node, entry, table, subnodes, rows_node))
        })
        .transpose()?;
    let mut folder_subnodes =
        collect_subnode_entries(&mut file, *old_root.block_btree(), folder_entry.sub_node())?;
    let mut contents_subnodes = collect_subnode_entries(
        &mut file,
        *old_root.block_btree(),
        contents_entry.sub_node(),
    )?;
    let rows_node = table_rows_node(contents_table.as_ref(), &contents_subnodes)?;

    let mut next_bid = header.next_block().index().max(
        bbt_entries
            .iter()
            .map(|entry| entry.block().block().index())
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut next_offset = old_root.file_eof_index().index();
    header.root_mut().set_amap_status(AmapStatus::Invalid);
    header.update_unique();
    file.seek(SeekFrom::Start(0))?;
    header.write(&mut file)?;
    file.sync_data()?;

    let mut message_entries = Vec::with_capacity(inputs.len());
    for (position, ((input, message), message_size)) in
        inputs.iter().zip(&messages).zip(&message_sizes).enumerate()
    {
        let mut blocks = Vec::new();
        let result = (|| {
            let (message_pages, message_properties) = message_data(
                &input.message,
                input.attachments,
                *message,
                header.unique_value(),
                *message_size,
            )?;
            let message_bid =
                push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, message_pages)?;
            let recipient_table = NID_RECIPIENT_TABLE;
            let (recipient_bid, recipient_subnodes) = push_recipient_table(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                input.message.recipients,
                header.unique_value(),
            )?;
            let mut message_subnodes = vec![UnicodeLeafSubNodeTreeEntry::new(
                recipient_table,
                recipient_bid,
                recipient_subnodes,
            )];
            for (node, data) in message_properties {
                let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
                message_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
            }
            push_attachments(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                input.attachments,
                header.unique_value(),
                &mut message_subnodes,
            )?;
            let message_subnode_bid = push_subnode_tree(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                message_subnodes,
            )?;
            write_pending_blocks(&mut file, &mut bbt_entries, &mut blocks)?;
            Ok::<_, io::Error>(UnicodeNodeBTreeEntry::new(
                *message,
                message_bid,
                Some(message_subnode_bid),
                Some(folder),
            ))
        })();
        message_entries.push(result.map_err(|err| {
            let file_size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            io::Error::new(
                err.kind(),
                format!(
                    "append message {} of {} ({}) at PST size {file_size}: {err}",
                    position + 1,
                    inputs.len(),
                    input.message.message_id,
                ),
            )
        })?);
    }
    file.sync_data()?;

    let next_contents_subnode = contents_subnodes
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::Internal)))
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0)
        .max(rows_node.index())
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?;
    let ((contents_heap, row_chunks, row_ids), external_contents) = rebuild_contents_table(
        contents_table.as_ref(),
        inputs,
        store_record_key,
        folder,
        &messages,
        &message_sizes,
        rows_node,
        next_contents_subnode,
        header.unique_value(),
    )
    .map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("rebuild receive-folder contents: {err}"),
        )
    })?;
    let mut blocks = Vec::new();
    let contents_data_bid =
        push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, contents_heap)?;
    let rows_bid = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, row_chunks)?;
    contents_subnodes.retain(|entry| entry.node() != rows_node);
    contents_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows_bid, None));
    for (node, data) in external_contents {
        let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
        contents_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
    }
    let contents_subnode_bid = push_subnode_tree(
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        contents_subnodes,
    )?;

    let content_count = i32::try_from(row_ids.len()).map_err(|_| PstError::IntegerConversion)?;
    set_folder_integer_property(&mut folder_properties, 0x3602, content_count);
    let next_property_subnode = folder_subnodes
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::Internal)))
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?;
    let (folder_pages, external_properties) =
        property_context_pages(folder_properties, next_property_subnode)?;
    let folder_data_bid =
        push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, folder_pages)?;
    for (node, data) in external_properties {
        let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
        folder_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
    }
    let folder_subnode_bid = if folder_subnodes.is_empty() {
        None
    } else {
        Some(push_subnode_tree(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            folder_subnodes,
        )?)
    };

    let parent_hierarchy_entry = if let Some((node, entry, table, mut subnodes, rows_node)) =
        parent_hierarchy
    {
        let (pages, rows, _) = rebuild_table(
            table.as_ref(),
            vec![],
            Some(rows_node),
            header.unique_value(),
            Some((folder, 0x3602, PropertyValue::Integer32(content_count))),
            true,
        )?;
        let data = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, pages)?;
        let rows = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, rows)?;
        subnodes.retain(|entry| entry.node() != rows_node);
        subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None));
        let subnodes = push_subnode_tree(&mut blocks, &mut next_bid, &mut next_offset, subnodes)?;
        Some((
            node,
            UnicodeNodeBTreeEntry::new(node, data, Some(subnodes), entry.parent()),
        ))
    } else {
        None
    };

    nbt_entries.retain(|entry| entry.node() != contents && entry.node() != folder);
    if let Some((node, entry)) = parent_hierarchy_entry {
        nbt_entries.retain(|candidate| candidate.node() != node);
        nbt_entries.push(entry);
    }
    nbt_entries.extend([
        UnicodeNodeBTreeEntry::new(
            contents,
            contents_data_bid,
            Some(contents_subnode_bid),
            contents_entry.parent(),
        ),
        UnicodeNodeBTreeEntry::new(
            folder,
            folder_data_bid,
            folder_subnode_bid,
            folder_entry.parent(),
        ),
    ]);
    nbt_entries.extend(message_entries);

    append_search_updates(
        &store,
        &mut nbt_entries,
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        messages
            .iter()
            .map(|message| SearchUpdateData::MessageAdded {
                parent: folder,
                message: *message,
            }),
    )?;

    commit_unicode_changes(
        path,
        file,
        header,
        bbt_entries,
        nbt_entries,
        blocks,
        next_bid,
        next_offset,
        &messages,
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod create_tests {
    use super::*;
    use crate::messaging::attachment::{Attachment, AttachmentData, UnicodeAttachment};
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_RECIPIENTS: &[UnicodePstRecipient<'static>] = &[
        UnicodePstRecipient {
            name: "to recipient",
            email: "to@example.com",
            recipient_type: UnicodePstRecipientType::To,
        },
        UnicodePstRecipient {
            name: "cc recipient",
            email: "cc@example.com",
            recipient_type: UnicodePstRecipientType::Cc,
        },
        UnicodePstRecipient {
            name: "bcc recipient",
            email: "bcc@example.com",
            recipient_type: UnicodePstRecipientType::Bcc,
        },
    ];

    fn last_inbox_message(path: &Path) -> Rc<dyn Message> {
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        store
            .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
            .unwrap()
    }

    fn folder_at_path(store: &Rc<UnicodeStore>, folder_path: &[&str]) -> Rc<dyn Folder> {
        let mut folder = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        for name in folder_path {
            folder = find_child_folder(store, folder, name).unwrap().unwrap();
        }
        store
            .open_folder(&store.properties().make_entry_id(folder).unwrap())
            .unwrap()
    }

    #[test]
    fn creates_a_reopenable_unicode_pst_with_one_message() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-create-{}-{stamp}.pst",
            std::process::id()
        ));

        let input = UnicodePstMessage {
            subject: "Memex PST export",
            sender_name: "Memex Sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "Hello from Memex PST export.",
            html_body: None,
            message_id: "<memex-pst-export@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        let pst = UnicodePstFile::create(&path, &input).unwrap();
        assert_eq!(pst.header().version(), NdbVersion::Unicode);
        assert_eq!(pst.header().crypt_method(), NdbCryptMethod::None);
        drop(pst);

        let reopened = UnicodePstFile::open(&path).unwrap();
        assert_eq!(reopened.header().version(), NdbVersion::Unicode);
        let store = UnicodeStore::read(Rc::new(reopened)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().display_name().unwrap(), "Inbox");
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        let contents = inbox.contents_table().unwrap();
        let message_row = contents.rows_matrix().next().unwrap();
        let message_id = NodeId::from(u32::from(message_row.id()));
        let message = store
            .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
            .unwrap();
        match message.properties().get(0x0037).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "Memex PST export"),
            value => panic!("unexpected subject value: {value:?}"),
        }
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "Hello from Memex PST export.")
            }
            value => panic!("unexpected body value: {value:?}"),
        }

        let recipients = message.recipient_table().unwrap();
        let recipient_rows = recipients
            .rows_matrix()
            .map(|row| {
                let values = row.columns(recipients.context()).unwrap();
                let read = |id| {
                    let (column, value) = recipients
                        .context()
                        .columns()
                        .iter()
                        .zip(&values)
                        .find(|(column, _)| column.prop_id() == id)
                        .unwrap();
                    recipients
                        .read_column(value.as_ref().unwrap(), column.prop_type())
                        .unwrap()
                };
                let recipient_type = match read(0x0C15) {
                    PropertyValue::Integer32(value) => value,
                    value => panic!("unexpected recipient type: {value:?}"),
                };
                let smtp = match read(0x39FE) {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected recipient value: {value:?}"),
                };
                (recipient_type, smtp)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recipient_rows,
            [
                (1, "to@example.com".to_string()),
                (2, "cc@example.com".to_string()),
                (3, "bcc@example.com".to_string()),
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn externalizes_large_recipient_display_values_in_contents_table() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-large-display-to-{}-{stamp}.pst",
            std::process::id()
        ));
        let names = (0..1_500)
            .map(|index| format!("수신자 {index:04}"))
            .collect::<Vec<_>>();
        let emails = (0..1_500)
            .map(|index| format!("recipient-{index}@example.com"))
            .collect::<Vec<_>>();
        let recipients = names
            .iter()
            .zip(&emails)
            .map(|(name, email)| UnicodePstRecipient {
                name,
                email,
                recipient_type: UnicodePstRecipientType::To,
            })
            .collect::<Vec<_>>();
        let display_to = recipient_display(&recipients, UnicodePstRecipientType::To);
        assert!(utf16_bytes(&display_to).len() > MAX_HEAP_ALLOCATION_SIZE);
        let input = UnicodePstMessage {
            subject: "large recipient display",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: &recipients,
            body: "body",
            html_body: None,
            message_id: "<large-display-to@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };

        drop(UnicodePstFile::create(&path, &input).unwrap());
        drop(UnicodePstFile::append_many(&path, &[input, input]).unwrap());
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3);
        let contents = inbox.contents_table().unwrap();
        for row in contents.rows_matrix() {
            let values = row.columns(contents.context()).unwrap();
            let (column, value) = contents
                .context()
                .columns()
                .iter()
                .zip(&values)
                .find(|(column, _)| column.prop_id() == 0x0E04)
                .unwrap();
            assert!(matches!(value, Some(TableRowColumnValue::Node(_))));
            match contents
                .read_column(value.as_ref().unwrap(), column.prop_type())
                .unwrap()
            {
                PropertyValue::Unicode(value) => assert_eq!(value.to_string(), display_to),
                value => panic!("unexpected display-to value: {value:?}"),
            }
        }
        let message = last_inbox_message(&path);
        assert_eq!(
            message.recipient_table().unwrap().rows_matrix().count(),
            recipients.len()
        );
        drop((message, inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fills_nonfinal_data_tree_blocks() {
        let mut blocks = Vec::new();
        let mut next_bid = 1;
        let mut next_offset = AMAP_FIRST_OFFSET;
        let root = push_data_tree_chunks(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            [vec![1; 100], vec![2; 50]],
        )
        .unwrap();

        assert!(root.is_internal());
        assert_eq!(blocks[0].size as usize, MAX_DATA_BLOCK_SIZE);
        assert_eq!(blocks[1].size, 50);
        let NewBlockData::DataTree(_, _, total_size) = blocks.last().unwrap().data else {
            panic!("expected XBLOCK root");
        };
        assert_eq!(total_size as usize, MAX_DATA_BLOCK_SIZE + 50);
    }

    #[test]
    fn writes_local_totals_for_multilevel_data_trees() {
        let mut blocks = Vec::new();
        let mut next_bid = 1;
        let mut next_offset = AMAP_FIRST_OFFSET;
        let root = push_data_tree_chunks(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            (0..1_022).map(|_| vec![0x5A]),
        )
        .unwrap();

        let trees = blocks
            .iter()
            .filter_map(|block| match &block.data {
                NewBlockData::DataTree(entries, level, total_size) => {
                    Some((block.id, *level, entries.len(), *total_size))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let full_group_size = 1_021 * u32::try_from(MAX_DATA_BLOCK_SIZE).unwrap();
        assert_eq!(trees.len(), 3);
        assert_eq!(
            (trees[0].1, trees[0].2, trees[0].3),
            (1, 1_021, full_group_size)
        );
        assert_eq!((trees[1].1, trees[1].2, trees[1].3), (1, 1, 1));
        assert_eq!(
            (trees[2].1, trees[2].2, trees[2].3),
            (2, 2, full_group_size + 1)
        );
        assert_eq!(root, trees[2].0);
    }

    #[test]
    fn aligns_heap_page_maps_to_two_bytes() {
        let data = finish_heap(
            vec![Vec::new(), vec![0x5A]],
            HeapNodeType::Properties,
            heap_id(1).unwrap(),
        )
        .unwrap();
        let mut cursor = Cursor::new(&data);
        let page_map_offset = HeapNodeHeader::read(&mut cursor).unwrap().page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        HeapNodePageMap::read(&mut cursor).unwrap();
        assert_eq!(cursor.position() as usize, data.len());

        let mut heap = HeapBuilder::new();
        heap.alloc(vec![0x5A]).unwrap();
        let pages = heap
            .finish(HeapNodeType::Properties, HeapId::default())
            .unwrap();
        let mut cursor = Cursor::new(&pages[0]);
        let page_map_offset = HeapNodeHeader::read(&mut cursor).unwrap().page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        HeapNodePageMap::read(&mut cursor).unwrap();
        assert_eq!(cursor.position() as usize, pages[0].len());
    }

    #[test]
    fn fills_nonfinal_heap_pages_with_a_padding_allocation() {
        let mut heap = HeapBuilder::new();
        for _ in 0..100 {
            heap.alloc(vec![0x5A; 100]).unwrap();
        }
        let pages = heap
            .finish(HeapNodeType::Properties, HeapId::default())
            .unwrap();

        assert!(pages.len() > 1);
        assert_eq!(pages[0].len(), MAX_DATA_BLOCK_SIZE);
        let mut cursor = Cursor::new(&pages[0]);
        let page_map_offset = HeapNodeHeader::read(&mut cursor).unwrap().page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        let page_map = HeapNodePageMap::read(&mut cursor).unwrap();
        let last = page_map.allocations().last().unwrap();
        assert_eq!(
            usize::from(last.offset() + last.size()),
            usize::from(page_map_offset)
        );
        assert_eq!(cursor.position() as usize, pages[0].len());
    }

    #[test]
    fn writes_html_with_plain_text_fallback() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-html-{}-{stamp}.pst",
            std::process::id()
        ));
        let html = "<p>HTML 본문</p>".repeat(1_000);
        let input = UnicodePstMessage {
            subject: "HTML message",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "plain-text fallback",
            html_body: Some(&html),
            message_id: "<html@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };

        drop(UnicodePstFile::create(&path, &input).unwrap());
        drop(UnicodePstFile::append(&path, &input).unwrap());
        let message = last_inbox_message(&path);
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), input.body),
            value => panic!("unexpected plain-text body: {value:?}"),
        }
        match message.properties().get(0x1013).unwrap() {
            PropertyValue::Binary(value) => assert_eq!(value.buffer(), html.as_bytes()),
            value => panic!("unexpected HTML body: {value:?}"),
        }
        drop(message);

        let empty_html = UnicodePstMessage {
            html_body: Some(""),
            message_id: "<empty-html@example.com>",
            ..input
        };
        drop(UnicodePstFile::append(&path, &empty_html).unwrap());
        let message = last_inbox_message(&path);
        match message.properties().get(0x1013).unwrap() {
            PropertyValue::Binary(value) => assert!(value.buffer().is_empty()),
            value => panic!("unexpected empty HTML body: {value:?}"),
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn appends_a_message_without_replacing_existing_messages() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-append-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "first body",
            html_body: None,
            message_id: "<first@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        let second = UnicodePstMessage {
            subject: "second",
            body: "second body",
            message_id: "<second@example.com>",
            ..first
        };
        let third = UnicodePstMessage {
            subject: "third",
            body: "third body",
            message_id: "<third@example.com>",
            ..first
        };

        let created = UnicodePstFile::create(&path, &first).unwrap();
        let old_bbt = *created.header().root().block_btree();
        let old_nbt = *created.header().root().node_btree();
        drop(created);
        let appended = UnicodePstFile::append(&path, &second).unwrap();
        assert_ne!(
            appended.header().root().block_btree().index().index(),
            old_bbt.index().index()
        );
        assert_ne!(
            appended.header().root().node_btree().index().index(),
            old_nbt.index().index()
        );
        let second_bbt = *appended.header().root().block_btree();
        let second_nbt = *appended.header().root().node_btree();
        drop(appended);
        let appended = UnicodePstFile::append(&path, &third).unwrap();
        assert_ne!(
            appended.header().root().block_btree().index().index(),
            second_bbt.index().index()
        );
        assert_ne!(
            appended.header().root().node_btree().index().index(),
            second_nbt.index().index()
        );

        let store = UnicodeStore::read(Rc::new(appended)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3);
        let subjects: Vec<String> = inbox
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                let message_id = NodeId::from(u32::from(row.id()));
                let message = store
                    .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
                    .unwrap();
                assert_eq!(
                    message.recipient_table().unwrap().rows_matrix().count(),
                    TEST_RECIPIENTS.len()
                );
                match message.properties().get(0x0037).unwrap() {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected subject value: {value:?}"),
                }
            })
            .collect();
        assert_eq!(subjects, ["first", "second", "third"]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_appends_messages_and_attachments_in_one_commit() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<bulk-first@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        let second = UnicodePstMessage {
            subject: "second",
            message_id: "<bulk-second@example.com>",
            ..first
        };
        let third = UnicodePstMessage {
            subject: "third",
            message_id: "<bulk-third@example.com>",
            ..first
        };
        let attachment = UnicodePstAttachment {
            filename: "bulk.txt",
            mime_type: "text/plain",
            content_id: None,
            data: b"bulk attachment",
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Bulk", "2026"],
                &[
                    UnicodePstBatchMessage {
                        message: second,
                        attachments: &[attachment],
                    },
                    UnicodePstBatchMessage {
                        message: third,
                        attachments: &[],
                    },
                ],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let folder = folder_at_path(&store, &["Bulk", "2026"]);
        assert_eq!(folder.properties().content_count().unwrap(), 2);
        let messages = folder
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                store
                    .open_message(
                        &store
                            .properties()
                            .make_entry_id(NodeId::from(u32::from(row.id())))
                            .unwrap(),
                        None,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .count(),
            1
        );
        assert!(messages[1].attachment_table().is_none());
        drop((messages, folder, store));
        assert_eq!(
            UnicodePstFile::append_many(&path, &[])
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_append_error_reports_position_message_id_and_pst_size() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-error-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<bulk-error-first@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        let invalid_attachment = [UnicodePstAttachment {
            filename: "invalid.png",
            mime_type: "image/png",
            content_id: Some("<>"),
            data: &[],
        }];
        let error = UnicodePstFile::append_many_with_attachments(
            &path,
            &[
                UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        message_id: "<bulk-error-ok@example.com>",
                        ..first
                    },
                    attachments: &[],
                },
                UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        message_id: "<bulk-error-invalid@example.com>",
                        ..first
                    },
                    attachments: &invalid_attachment,
                },
            ],
        )
        .err()
        .unwrap();
        let error = error.to_string();
        assert!(error.contains("message 2 of 2"), "{error}");
        assert!(
            error.contains("<bulk-error-invalid@example.com>"),
            "{error}"
        );
        assert!(error.contains("PST size"), "{error}");
        let message = last_inbox_message(&path);
        match message.properties().get(0x0037).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "first"),
            value => panic!("unexpected subject value: {value:?}"),
        }
        drop(message);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_appends_three_thousand_messages_without_cow_explosion() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-3000-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<bulk-3000-first@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());

        let subjects = (0..3_000)
            .map(|index| format!("bulk message {index}"))
            .collect::<Vec<_>>();
        let message_ids = (0..3_000)
            .map(|index| format!("<bulk-{index}@example.com>"))
            .collect::<Vec<_>>();
        let messages = subjects
            .iter()
            .zip(&message_ids)
            .map(|(subject, message_id)| UnicodePstMessage {
                subject,
                message_id,
                ..first
            })
            .collect::<Vec<_>>();
        let attachment_data = vec![0x5A; 320 * 1_024];
        let attachments = [UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &attachment_data,
        }];
        let batch = messages
            .iter()
            .enumerate()
            .map(|(index, message)| UnicodePstBatchMessage {
                message: *message,
                attachments: if index % 300 == 0 { &attachments } else { &[] },
            })
            .collect::<Vec<_>>();
        drop(UnicodePstFile::append_many_with_attachments(&path, &batch).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3_001);
        assert_eq!(inbox.contents_table().unwrap().rows_matrix().count(), 3_001);
        assert!(std::fs::metadata(&path).unwrap().len() < 64 * 1024 * 1024);
        drop((inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_and_appends_messages_in_nested_folders() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-folders-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first in Projects/2026",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<folder-first@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(
            UnicodePstFile::create_in_folder_with_attachments(
                &path,
                &["Projects", "2026"],
                &first,
                &[UnicodePstAttachment {
                    filename: "folder.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: b"folder attachment",
                }],
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["projects", "2026"],
                &UnicodePstMessage {
                    subject: "second in Projects/2026",
                    message_id: "<folder-second@example.com>",
                    ..first
                },
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["Archive", "2025"],
                &UnicodePstMessage {
                    subject: "in Archive/2025",
                    message_id: "<folder-archive@example.com>",
                    ..first
                },
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append(
                &path,
                &UnicodePstMessage {
                    subject: "still in Inbox",
                    message_id: "<folder-inbox@example.com>",
                    ..first
                },
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let ipm = folder_at_path(&store, &[]);
        assert_eq!(ipm.hierarchy_table().unwrap().rows_matrix().count(), 4);
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        let projects = folder_at_path(&store, &["Projects"]);
        assert!(projects.properties().has_sub_folders().unwrap());
        assert_eq!(projects.hierarchy_table().unwrap().rows_matrix().count(), 1);
        let projects_2026 = folder_at_path(&store, &["Projects", "2026"]);
        assert_eq!(projects_2026.properties().content_count().unwrap(), 2);
        assert!(projects_2026.associated_table().is_some());
        let subjects = projects_2026
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                let message = store
                    .open_message(
                        &store
                            .properties()
                            .make_entry_id(NodeId::from(u32::from(row.id())))
                            .unwrap(),
                        None,
                    )
                    .unwrap();
                match message.properties().get(0x0037).unwrap() {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected subject value: {value:?}"),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            subjects,
            ["first in Projects/2026", "second in Projects/2026"]
        );
        let first_message_id = NodeId::from(u32::from(
            projects_2026
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let first_message = store
            .open_message(
                &store.properties().make_entry_id(first_message_id).unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(
            first_message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .count(),
            1
        );
        let archive_2025 = folder_at_path(&store, &["Archive", "2025"]);
        assert_eq!(archive_2025.properties().content_count().unwrap(), 1);
        assert!(archive_2025.associated_table().is_some());
        drop((
            first_message,
            archive_2025,
            projects_2026,
            projects,
            inbox,
            ipm,
            store,
        ));

        assert_eq!(
            UnicodePstFile::append_in_folder(&path, &[], &first)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_and_appends_large_bodies_and_inline_attachments() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-large-{}-{stamp}.pst",
            std::process::id()
        ));
        let body = "본문".repeat(2_500);
        let attachment_bytes = vec![0xA5; 9_000];
        let input = UnicodePstMessage {
            subject: "large",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: &body,
            html_body: Some("<img src=\"cid:large-image@example.com\">"),
            message_id: "<large@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        let attachment = UnicodePstAttachment {
            filename: "large.png",
            mime_type: "image/png",
            content_id: Some("<large-image@example.com>"),
            data: &attachment_bytes,
        };

        drop(UnicodePstFile::create_with_attachments(&path, &input, &[attachment]).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let entry_id = store.properties().make_entry_id(message_id).unwrap();
        let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
        assert_eq!(message.properties().message_flags().unwrap(), 0x11);
        assert!(matches!(
            message.properties().get(0x0E1B),
            Some(PropertyValue::Boolean(true))
        ));
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), body),
            value => panic!("unexpected body value: {value:?}"),
        }
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        assert_eq!(attachment.properties().attachment_size().unwrap(), 9_124);
        match attachment.properties().get(0x3707).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "large.png"),
            value => panic!("unexpected attachment filename: {value:?}"),
        }
        match attachment.properties().get(0x370E).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "image/png"),
            value => panic!("unexpected attachment MIME type: {value:?}"),
        }
        match attachment.properties().get(0x3712).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "large-image@example.com")
            }
            value => panic!("unexpected attachment Content-ID: {value:?}"),
        }
        assert!(matches!(
            attachment.properties().get(0x3714),
            Some(PropertyValue::Integer32(0x0000_0004))
        ));
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert_eq!(value.buffer(), attachment_bytes),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        drop(attachment);
        drop(inbox);
        drop(store);
        std::fs::remove_file(&path).unwrap();

        let small_body = "small";
        let first = UnicodePstMessage {
            subject: "first",
            body: small_body,
            message_id: "<first@example.com>",
            ..input
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        drop(
            UnicodePstFile::append_with_attachments(
                &path,
                &input,
                &[UnicodePstAttachment {
                    filename: "large.png",
                    mime_type: "image/png",
                    content_id: Some("large-image@example.com"),
                    data: &attachment_bytes,
                }],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        let message = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_id).unwrap(),
            None,
        )
        .unwrap();
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), body),
            value => panic!("unexpected body value: {value:?}"),
        }
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        match attachment.properties().get(0x3712).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "large-image@example.com")
            }
            value => panic!("unexpected attachment Content-ID: {value:?}"),
        }
        assert!(matches!(
            attachment.properties().get(0x3714),
            Some(PropertyValue::Integer32(0x0000_0004))
        ));
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert_eq!(value.buffer(), attachment_bytes),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        drop(attachment);
        drop(inbox);
        drop(store);
        std::fs::remove_file(path).unwrap();

        assert_eq!(
            attachment_data(
                &UnicodePstAttachment {
                    filename: "invalid.png",
                    mime_type: "image/png",
                    content_id: Some("<>"),
                    data: &[],
                },
                None,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn reserves_allocation_metadata_through_64_gib() {
        const LIMIT: u64 = 64 * 1024 * 1024 * 1024;

        for (first, interval) in [
            (AMAP_FIRST_OFFSET, AMAP_DATA_SIZE),
            (PMAP_FIRST_OFFSET, PMAP_DATA_SIZE),
            (FMAP_FIRST_OFFSET, FMAP_DATA_SIZE),
            (FPMAP_FIRST_OFFSET, FPMAP_DATA_SIZE),
        ] {
            let mut metadata_offset = first;
            while metadata_offset < LIMIT {
                let mut next_offset = metadata_offset;
                let allocation =
                    reserve_file_range(&mut next_offset, PAGE_SIZE as u64, PAGE_SIZE as u64);
                assert!(
                    allocation > metadata_offset,
                    "allocated over metadata page at 0x{metadata_offset:X}"
                );
                metadata_offset += interval;
            }
        }
    }

    #[test]
    fn supports_multilevel_btrees_and_multiple_amaps() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-multilevel-{}-{stamp}.pst",
            std::process::id()
        ));
        let large_attachment = vec![0x5A; 320_000];
        let first = UnicodePstMessage {
            subject: "message-0",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<message-0@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &first,
                &[UnicodePstAttachment {
                    filename: "large.bin",
                    mime_type: "application/octet-stream",
                    content_id: None,
                    data: &large_attachment,
                }],
            )
            .unwrap(),
        );
        for index in 1..20 {
            let subject = format!("message-{index}");
            let message_id = format!("<message-{index}@example.com>");
            drop(
                UnicodePstFile::append(
                    &path,
                    &UnicodePstMessage {
                        subject: &subject,
                        message_id: &message_id,
                        ..first
                    },
                )
                .unwrap(),
            );
        }

        let pst = UnicodePstFile::open(&path).unwrap();
        assert!(pst.header().root().amap_last_index().index() > AMAP_FIRST_OFFSET);
        let mut reader = pst.reader().lock().unwrap();
        assert!(matches!(
            UnicodeBlockBTree::read(&mut *reader, *pst.header().root().block_btree()).unwrap(),
            RootBTreePage::Intermediate(..)
        ));
        assert!(matches!(
            UnicodeNodeBTree::read(&mut *reader, *pst.header().root().node_btree()).unwrap(),
            RootBTreePage::Intermediate(..)
        ));
        drop(reader);

        let store = UnicodeStore::read(Rc::new(pst)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 20);
        assert_eq!(inbox.contents_table().unwrap().rows_matrix().count(), 20);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn appends_to_an_outlook_created_unicode_pst() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-existing-{}-{stamp}.pst",
            std::process::id()
        ));
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Empty.pst"),
            &path,
        )
        .unwrap();

        // Outlook's fixture uses permute encoding. Decode its data blocks so the same Outlook
        // structure exercises the writer's intentionally unencrypted-only append path.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut header = UnicodeHeader::read(&mut file).unwrap();
        assert_eq!(header.crypt_method(), NdbCryptMethod::Permute);
        let entries = collect_block_btree_entries(&mut file, *header.root().block_btree()).unwrap();
        for entry in entries
            .iter()
            .filter(|entry| !entry.block().block().is_internal())
        {
            file.seek(SeekFrom::Start(entry.block().index().index()))
                .unwrap();
            let block =
                UnicodeDataBlock::read(&mut file, entry.size(), NdbCryptMethod::Permute).unwrap();
            file.seek(SeekFrom::Start(entry.block().index().index()))
                .unwrap();
            UnicodeDataBlock::new(
                NdbCryptMethod::None,
                block.data().to_vec(),
                *block.trailer(),
            )
            .unwrap()
            .write(&mut file)
            .unwrap();
        }
        header.set_crypt_method(NdbCryptMethod::None);
        file.seek(SeekFrom::Start(0)).unwrap();
        header.write(&mut file).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let input = UnicodePstMessage {
            subject: "appended to Outlook PST",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "existing PST body",
            html_body: None,
            message_id: "<existing-pst@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(UnicodePstFile::append(&path, &input).unwrap());
        drop(UnicodePstFile::append(&path, &input).unwrap());
        drop(UnicodePstFile::append_in_folder(&path, &["Imported", "2026"], &input).unwrap());

        let pst = Rc::new(UnicodePstFile::open(&path).unwrap());
        let next_message = pst
            .header()
            .next_node_index(NodeIdType::NormalMessage)
            .unwrap();
        let mut reader = File::open(&path).unwrap();
        let nbt_entries =
            collect_node_btree_entries(&mut reader, *pst.header().root().node_btree()).unwrap();
        let store = UnicodeStore::read(pst).unwrap();
        let inbox_id = default_receive_folder(&store, &nbt_entries).unwrap();
        let inbox = store
            .open_folder(&store.properties().make_entry_id(inbox_id).unwrap())
            .unwrap();
        assert_eq!(inbox.properties().content_count().unwrap(), 2);
        let contents = inbox.contents_table().unwrap();
        assert!(contents.context().columns().len() > 2);
        let message_ids = contents
            .rows_matrix()
            .map(|row| NodeId::from(u32::from(row.id())))
            .collect::<Vec<_>>();
        assert_eq!(message_ids.len(), 2);
        assert!(message_ids.iter().all(|id| id.index() < next_message));
        for message_id in message_ids {
            let message = store
                .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
                .unwrap();
            match message.properties().get(0x0037).unwrap() {
                PropertyValue::Unicode(value) => assert_eq!(value.to_string(), input.subject),
                value => panic!("unexpected subject value: {value:?}"),
            }
        }
        let imported = folder_at_path(&store, &["Imported"]);
        assert!(
            imported
                .hierarchy_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        let imported_2026 = folder_at_path(&store, &["Imported", "2026"]);
        assert_eq!(imported_2026.properties().content_count().unwrap(), 1);
        assert!(
            imported_2026
                .contents_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        assert!(
            imported_2026
                .associated_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        drop((imported_2026, imported, inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn externalizes_large_folder_properties() {
        let value = vec![0x5A; MAX_HEAP_ALLOCATION_SIZE + 1];
        let (pages, external) = property_context_pages(
            [(
                0x300B,
                PropertyValue::Binary(BinaryValue::new(value.clone())),
            )],
            0x300,
        )
        .unwrap();
        assert!(!pages.is_empty());
        assert_eq!(external.len(), 1);
        assert_eq!(
            external[0].0,
            NodeId::new(NodeIdType::Internal, 0x300).unwrap()
        );
        assert_eq!(external[0].1, value);
    }

    #[test]
    fn writes_an_empty_attachment() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-empty-attachment-{}-{stamp}.pst",
            std::process::id()
        ));
        let message = UnicodePstMessage {
            subject: "empty attachment",
            sender_name: "sender",
            sender_email: "sender@example.com",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: "<empty-attachment@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &message,
                &[UnicodePstAttachment {
                    filename: "empty.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: &[],
                }],
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_with_attachments(
                &path,
                &message,
                &[UnicodePstAttachment {
                    filename: "empty.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: &[],
                }],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        let message = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_id).unwrap(),
            None,
        )
        .unwrap();
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        assert_eq!(attachment.properties().attachment_size().unwrap(), 74);
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert!(value.buffer().is_empty()),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        std::fs::remove_file(path).unwrap();
    }
}

struct AllocationMapPageInfo<Pst>
where
    Pst: PstFile,
    <Pst as PstFile>::AllocationMapPage: AllocationMapPageReadWrite<Pst>,
{
    amap_page: <Pst as PstFile>::AllocationMapPage,
    free_space: u64,
}

impl<Pst> AllocationMapPageInfo<Pst>
where
    Pst: PstFile,
    <Pst as PstFile>::AllocationMapPage: AllocationMapPageReadWrite<Pst>,
{
    fn max_free_slots(&self) -> u8 {
        u8::try_from(self.amap_page.find_free_bits(0xFF).len()).unwrap_or(0xFF)
    }
}

type PstFileReadWriteBTree<Pst, BTree> = RootBTreePage<
    Pst,
    <BTree as RootBTree>::Entry,
    <BTree as RootBTree>::IntermediatePage,
    <BTree as RootBTree>::LeafPage,
>;

type PstFileReadWriteNodeBTree<Pst> = PstFileReadWriteBTree<Pst, <Pst as PstFile>::NodeBTree>;

type PstFileReadWriteBlockBTree<Pst> = PstFileReadWriteBTree<Pst, <Pst as PstFile>::BlockBTree>;

impl<Pst> PstFileInner<Pst>
where
    Pst: PstFile + PstFileLock<Pst>,
    <Pst as PstFile>::BlockId: BlockId<Index = <Pst as PstFile>::BTreeKey>
        + From<<<Pst as PstFile>::ByteIndex as ByteIndex>::Index>
        + Debug,
    <Pst as PstFile>::PageId: From<<<Pst as PstFile>::ByteIndex as ByteIndex>::Index> + Debug,
    <Pst as PstFile>::ByteIndex: ByteIndex<Index: TryFrom<u64>> + Debug,
    <Pst as PstFile>::BlockRef: Debug,
    <Pst as PstFile>::PageRef: Debug,
    <Pst as PstFile>::Root: RootReadWrite<Pst>,
    <Pst as PstFile>::Header: HeaderReadWrite<Pst>,
    <Pst as PstFile>::DensityListPage: DensityListPageReadWrite<Pst>,
    <Pst as PstFile>::PageTrailer: PageTrailerReadWrite,
    <Pst as PstFile>::BTreeKey: BTreePageKeyReadWrite,
    <Pst as PstFile>::NodeBTreeEntry: NodeBTreeEntryReadWrite,
    <Pst as PstFile>::NodeBTree: NodeBTreeReadWrite<Pst, <Pst as PstFile>::NodeBTreeEntry>,
    <<Pst as PstFile>::NodeBTree as RootBTree>::IntermediatePage:
        RootBTreeIntermediatePageReadWrite<
            Pst,
            <Pst as PstFile>::NodeBTreeEntry,
            <<Pst as PstFile>::NodeBTree as RootBTree>::LeafPage,
        >,
    <<<Pst as PstFile>::NodeBTree as RootBTree>::IntermediatePage as BTreePage>::Entry:
        BTreePageEntryReadWrite,
    <<Pst as PstFile>::NodeBTree as RootBTree>::LeafPage: RootBTreeLeafPageReadWrite<Pst>,
    <Pst as PstFile>::BlockBTreeEntry: BlockBTreeEntryReadWrite,
    <Pst as PstFile>::BlockBTree: BlockBTreeReadWrite<Pst, <Pst as PstFile>::BlockBTreeEntry>,
    <<Pst as PstFile>::BlockBTree as RootBTree>::IntermediatePage:
        RootBTreeIntermediatePageReadWrite<
            Pst,
            <Pst as PstFile>::BlockBTreeEntry,
            <<Pst as PstFile>::BlockBTree as RootBTree>::LeafPage,
        >,
    <<<Pst as PstFile>::BlockBTree as RootBTree>::IntermediatePage as BTreePage>::Entry:
        BTreePageEntryReadWrite,
    <<Pst as PstFile>::BlockBTree as RootBTree>::LeafPage: RootBTreeLeafPageReadWrite<Pst>,
    <Pst as PstFile>::BlockTrailer: BlockTrailerReadWrite,
    <Pst as PstFile>::AllocationMapPage: AllocationMapPageReadWrite<Pst>,
    <Pst as PstFile>::AllocationPageMapPage: AllocationPageMapPageReadWrite<Pst>,
    <Pst as PstFile>::FreeMapPage: FreeMapPageReadWrite<Pst>,
    <Pst as PstFile>::FreePageMapPage: FreePageMapPageReadWrite<Pst>,
    <Pst as PstFile>::DensityListPage: DensityListPageReadWrite<Pst>,
    <Pst as PstFile>::DataTreeBlock: IntermediateTreeBlockReadWrite,
    <Pst as PstFile>::DataTreeEntry:
        IntermediateTreeEntryReadWrite + From<<Pst as PstFile>::BlockId>,
    <Pst as PstFile>::DataBlock: BlockReadWrite + Clone,
    <Pst as PstFile>::SubNodeTreeBlockHeader: SubNodeTreeBlockHeaderReadWrite,
    <Pst as PstFile>::SubNodeTreeBlock: IntermediateTreeBlockReadWrite,
    <<Pst as PstFile>::SubNodeTreeBlock as IntermediateTreeBlock>::Entry:
        IntermediateTreeEntryReadWrite,
    <Pst as PstFile>::SubNodeBlock: IntermediateTreeBlockReadWrite,
    <<Pst as PstFile>::SubNodeBlock as IntermediateTreeBlock>::Entry:
        IntermediateTreeEntryReadWrite,
{
    fn read_from(mut reader: Box<dyn PstReader>) -> io::Result<Self> {
        let header = <<Pst as PstFile>::Header as HeaderReadWrite<Pst>>::read(&mut reader)?;
        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::read(&mut reader);
        Ok(Self {
            reader: Mutex::new(Box::new(reader)),
            writer: Err(PstError::OpenedReadOnly),
            header,
            density_list,
            node_cache: Default::default(),
            block_cache: Default::default(),
        })
    }

    fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let reader = Box::new(File::open(&path)?);
        let writer = OpenOptions::new()
            .write(true)
            .open(&path)
            .map(BufWriter::new)
            .map(Mutex::new)
            .map_err(|_| PstError::NoWriteAccess(path.as_ref().display().to_string()));
        Ok(Self {
            writer,
            ..Self::read_from(reader)?
        })
    }

    /// Begin a transaction by rebuilding the allocation map if needed and initializing the density
    /// list, then set [`AmapStatus::Invalid`] in the header till the transaction is finished.
    ///
    /// See also [Transactional Semantics](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/bc5a92df-7fc1-4dc2-9c7c-5677237dd73a).
    fn start_write(&mut self) -> io::Result<()> {
        self.rebuild_allocation_map()?;
        self.ensure_density_list()?;

        let header = {
            self.header.update_unique();

            let root = self.header.root_mut();
            root.set_amap_status(AmapStatus::Invalid);
            self.header.clone()
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()
    }

    /// Complete a transaction by writing the header and density list to the file, and setting
    /// [`AmapStatus::Valid2`].
    ///
    /// See also [Transactional Semantics](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/bc5a92df-7fc1-4dc2-9c7c-5677237dd73a).
    #[instrument(skip_all)]
    fn finish_write(&mut self) -> io::Result<()> {
        // Reset AmapStatus::Valid2 to complete the transaction and then rewrite the updated
        // density list.
        let header = {
            self.header.update_unique();
            let root = self.header.root_mut();
            root.set_amap_status(AmapStatus::Valid2);
            self.header.clone()
        };

        self.update_density_list_page_id()?;
        let density_list = {
            self.density_list.as_ref().ok().and_then(|dl| {
                <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                    dl.backfill_complete(),
                    dl.current_page(),
                    dl.entries(),
                    *dl.trailer(),
                )
                .ok()
            })
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()?;

        if let Some(density_list) = density_list {
            density_list.write(writer)?;
            writer.flush()?;
        }

        Ok(())
    }

    /// [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced)
    fn rebuild_allocation_map(&mut self) -> io::Result<()> {
        let root = self.header.root();
        if AmapStatus::Invalid != root.amap_is_valid() {
            return Ok(());
        }

        let num_amap_pages = root.file_eof_index().index().into() - AMAP_FIRST_OFFSET;
        let num_amap_pages = num_amap_pages.div_ceil(AMAP_DATA_SIZE);

        let mut amap_pages: Vec<_> = (0..num_amap_pages)
            .map(|amap_index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        amap_index * AMAP_DATA_SIZE + AMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::AllocationMap,
                    0,
                    block_id,
                    0,
                );

                let mut map_bits = [0; mem::size_of::<MapBits>()];
                let reserved = map_page_count(amap_index);

                let free_space = AMAP_DATA_SIZE - reserved * PAGE_SIZE as u64;

                let reserved = &[0xFF; 4][..reserved as usize];
                map_bits[..reserved.len()].copy_from_slice(reserved);

                let amap_page =
                    <<Pst as PstFile>::AllocationMapPage as AllocationMapPageReadWrite<Pst>>::new(
                        map_bits, trailer,
                    )?;
                Ok(AllocationMapPageInfo::<Pst> {
                    amap_page,
                    free_space,
                })
            })
            .collect::<PstResult<Vec<_>>>()?;

        {
            let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
            let reader = &mut *reader;

            let node_btree =
                <Pst::NodeBTree as RootBTreeReadWrite>::read(reader, *root.node_btree())?;

            Self::mark_node_btree_allocations(
                reader,
                root.node_btree().index(),
                &node_btree,
                &mut amap_pages,
            )?;

            let block_btree =
                <Pst::BlockBTree as RootBTreeReadWrite>::read(reader, *root.block_btree())?;

            Self::mark_block_btree_allocations(
                reader,
                root.block_btree().index(),
                &block_btree,
                &mut amap_pages,
            )?;
        }

        let free_bytes =
            <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                amap_pages.iter().map(|page| page.free_space).sum(),
            )
            .map_err(|_| PstError::IntegerConversion)?;
        let free_bytes = <<Pst as PstFile>::ByteIndex as ByteIndexReadWrite>::new(free_bytes);

        let mut first_fmap = [0; FMAP_FIRST_SIZE as usize];
        for (entry, free_space) in first_fmap
            .iter_mut()
            .zip(amap_pages.iter().map(|page| page.max_free_slots()))
        {
            *entry = free_space;
        }

        let pmap_pages: Vec<_> = (0..num_amap_pages.div_ceil(PMAP_PAGE_COUNT))
            .map(|index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * PMAP_DATA_SIZE + PMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::AllocationPageMap,
                    0,
                    block_id,
                    0,
                );

                let map_bits = [0xFF; mem::size_of::<MapBits>()];

                let pmap_page =
                    <<Pst as PstFile>::AllocationPageMapPage as AllocationPageMapPageReadWrite<
                        Pst,
                    >>::new(map_bits, trailer)?;
                Ok(pmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        let fmap_pages: Vec<_> = (0..(num_amap_pages.max(FMAP_FIRST_SIZE) - FMAP_FIRST_SIZE)
            .div_ceil(FMAP_PAGE_COUNT))
            .map(|index| {
                let amap_index =
                    FMAP_FIRST_SIZE as usize + (index as usize * mem::size_of::<MapBits>());
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * FMAP_DATA_SIZE + FMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::FreeMap,
                    0,
                    block_id,
                    0,
                );

                let mut map_bits = [0; mem::size_of::<MapBits>()];
                for (entry, free_space) in map_bits.iter_mut().zip(
                    amap_pages
                        .iter()
                        .skip(amap_index)
                        .map(|page| page.max_free_slots()),
                ) {
                    *entry = free_space;
                }

                let fmap_page = <<Pst as PstFile>::FreeMapPage as FreeMapPageReadWrite<Pst>>::new(
                    map_bits, trailer,
                )?;
                Ok(fmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        let fpmap_pages: Vec<_> = (0..(num_amap_pages.max(FPMAP_FIRST_SIZE) - FPMAP_FIRST_SIZE)
            .div_ceil(FPMAP_PAGE_COUNT))
            .map(|index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * FPMAP_DATA_SIZE + FPMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::FreePageMap,
                    0,
                    block_id,
                    0,
                );

                let map_bits = [0xFF; mem::size_of::<MapBits>()];

                let fpmap_page = <<Pst as PstFile>::FreePageMapPage as FreePageMapPageReadWrite<
                    Pst,
                >>::new(map_bits, trailer)?;
                Ok(fpmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        {
            let mut writer = self
                .writer
                .as_ref()?
                .lock()
                .map_err(|_| PstError::LockError)?;
            let writer = &mut *writer;

            for page in amap_pages.into_iter().map(|info| info.amap_page) {
                writer.seek(SeekFrom::Start(page.trailer().block_id().into_u64()))?;
                <Pst::AllocationMapPage as AllocationMapPageReadWrite<Pst>>::write(&page, writer)?;
            }

            for page in pmap_pages.into_iter() {
                writer.seek(SeekFrom::Start(page.trailer().block_id().into_u64()))?;
                <Pst::AllocationPageMapPage as AllocationPageMapPageReadWrite<Pst>>::write(
                    &page, writer,
                )?;
            }

            for page in fmap_pages.into_iter() {
                writer.seek(SeekFrom::Start(page.trailer().block_id().into_u64()))?;
                <Pst::FreeMapPage as FreeMapPageReadWrite<Pst>>::write(&page, writer)?;
            }

            for page in fpmap_pages.into_iter() {
                writer.seek(SeekFrom::Start(page.trailer().block_id().into_u64()))?;
                <Pst::FreePageMapPage as FreePageMapPageReadWrite<Pst>>::write(&page, writer)?;
            }

            writer.flush()?;
        }

        let header = {
            <<Pst as PstFile>::Header as HeaderReadWrite<Pst>>::first_free_map(&mut self.header)
                .copy_from_slice(&first_fmap);
            self.header.update_unique();

            let root = self.header.root_mut();
            let amap_last_index =
                AMAP_FIRST_OFFSET + num_amap_pages.saturating_sub(1) * AMAP_DATA_SIZE;
            let amap_last_index = <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<
                u64,
            >>::try_from(amap_last_index)
            .map_err(|_| PstError::IntegerConversion)?;
            root.set_amap_last_index(<<Pst as PstFile>::ByteIndex as ByteIndexReadWrite>::new(
                amap_last_index,
            ));
            root.reset_free_size(free_bytes)?;
            root.set_amap_status(AmapStatus::Valid2);

            self.header.clone()
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()
    }

    /// Recursively mark all of the pages in the [`Node BTree`](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/7d759bcb-7864-480c-8746-f6af913ab085).
    /// as allocated. This does not include any blocks referenced in the nodes or the sub-trees in
    /// those blocks, blocks will be marked by [`Self::mark_block_btree_allocations`].
    ///
    /// See also [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced).
    #[instrument(skip_all)]
    fn mark_node_btree_allocations<R: PstReader>(
        reader: &mut R,
        page_index: Pst::ByteIndex,
        node_btree: &PstFileReadWriteNodeBTree<Pst>,
        amap_pages: &mut Vec<AllocationMapPageInfo<Pst>>,
    ) -> io::Result<()> {
        Self::mark_page_allocation(page_index.index().into(), amap_pages)?;

        if let RootBTreePage::Intermediate(page, ..) = node_btree {
            let level = page.level();
            for entry in page.entries() {
                let block = entry.block();
                let node_btree = <Pst::NodeBTree as RootBTreeReadWrite>::read(reader, block)?;
                match &node_btree {
                    RootBTreePage::Intermediate(page, ..) if page.level() + 1 != level => {
                        error!(
                            name: "PstUnexpectedBTreeIntermediatePage",
                            block = ?block.block(),
                            index = ?block.index(),
                            parent = level,
                            child = page.level(),
                            "Possible NBT page cycle detected, expected child == parent - 1"
                        );
                        return Err(PstError::InvalidBTreePage(block.index().index().into()).into());
                    }
                    RootBTreePage::Leaf(_) if level != 1 => {
                        error!(
                            name: "PstUnexpectedBTreeLeafPage",
                            block = ?block.block(),
                            index = ?block.index(),
                            parent = level,
                            child = page.level(),
                            "Corrupted NBT intermediate page detected, unexpected child leaf page"
                        );
                        return Err(PstError::InvalidBTreePage(block.index().index().into()).into());
                    }
                    _ => (),
                }
                Self::mark_node_btree_allocations(reader, block.index(), &node_btree, amap_pages)?;
            }
        }

        Ok(())
    }

    /// Recursively mark all of the pages and blocks in the [`Block BTree`](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/7d759bcb-7864-480c-8746-f6af913ab085).
    ///
    /// See also [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced).
    #[instrument(skip_all)]
    fn mark_block_btree_allocations<R: PstReader>(
        reader: &mut R,
        page_index: Pst::ByteIndex,
        block_btree: &PstFileReadWriteBlockBTree<Pst>,
        amap_pages: &mut Vec<AllocationMapPageInfo<Pst>>,
    ) -> io::Result<()> {
        Self::mark_page_allocation(page_index.index().into(), amap_pages)?;

        match block_btree {
            RootBTreePage::Intermediate(page, ..) => {
                let level = page.level();
                for entry in page.entries() {
                    let block = entry.block();
                    let block_btree = <Pst::BlockBTree as RootBTreeReadWrite>::read(reader, block)?;
                    match &block_btree {
                        RootBTreePage::Intermediate(page, ..) if page.level() + 1 != level => {
                            error!(
                                name: "PstUnexpectedBTreeIntermediatePage",
                                block = ?block.block(),
                                index = ?block.index(),
                                parent = level,
                                child = page.level(),
                                "Possible BBT page cycle detected, expected child == parent - 1"
                            );
                            return Err(
                                PstError::InvalidBTreePage(block.index().index().into()).into()
                            );
                        }
                        RootBTreePage::Leaf(_) if level != 1 => {
                            error!(
                                name: "PstUnexpectedBTreeLeafPage",
                                block = ?block.block(),
                                index = ?block.index(),
                                parent = level,
                                child = page.level(),
                                "Corrupted BBT intermediate page detected, unexpected child leaf page"
                            );
                            return Err(
                                PstError::InvalidBTreePage(block.index().index().into()).into()
                            );
                        }
                        _ => (),
                    }
                    Self::mark_block_btree_allocations(
                        reader,
                        block.index(),
                        &block_btree,
                        amap_pages,
                    )?;
                }
            }
            RootBTreePage::Leaf(page) => {
                for entry in page.entries() {
                    Self::mark_block_allocation(
                        entry.block().index().index().into(),
                        entry.size(),
                        amap_pages,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Mark a page at the given file offset as allocated.
    fn mark_page_allocation(
        index: u64,
        amap_pages: &mut [AllocationMapPageInfo<Pst>],
    ) -> io::Result<()> {
        let index = index - AMAP_FIRST_OFFSET;
        let amap_index =
            usize::try_from(index / AMAP_DATA_SIZE).map_err(|_| PstError::IntegerConversion)?;
        let entry = amap_pages
            .get_mut(amap_index)
            .ok_or(PstError::AllocationMapPageNotFound(amap_index))?;
        entry.free_space -= PAGE_SIZE as u64;

        let bytes = entry.amap_page.map_bits_mut();

        let bit_index = usize::try_from((index % AMAP_DATA_SIZE) / 64)
            .map_err(|_| PstError::IntegerConversion)?;
        let byte_index = bit_index / 8;
        let bit_index = bit_index % 8;

        if bit_index == 0 {
            bytes[byte_index] = 0xFF;
        } else {
            let mask = 0x80_u8 >> bit_index;
            let mask = mask | (mask - 1);
            bytes[byte_index] |= mask;
            bytes[byte_index + 1] |= !mask;
        }

        Ok(())
    }

    /// Mark a block at the given file offset and size as allocated.
    fn mark_block_allocation(
        index: u64,
        size: u16,
        amap_pages: &mut [AllocationMapPageInfo<Pst>],
    ) -> io::Result<()> {
        let index = index - AMAP_FIRST_OFFSET;
        let amap_index =
            usize::try_from(index / AMAP_DATA_SIZE).map_err(|_| PstError::IntegerConversion)?;
        let entry = amap_pages
            .get_mut(amap_index)
            .ok_or(PstError::AllocationMapPageNotFound(amap_index))?;
        let size = u64::from(block_size(
            size + <<Pst as PstFile>::BlockTrailer as BlockTrailerReadWrite>::SIZE,
        ));
        entry.free_space -= size;

        let bytes = entry.amap_page.map_bits_mut();

        let bit_start = usize::try_from((index % AMAP_DATA_SIZE) / 64)
            .map_err(|_| PstError::IntegerConversion)?;
        let bit_end =
            bit_start + usize::try_from(size / 64).map_err(|_| PstError::IntegerConversion)?;
        let byte_start = bit_start / 8;
        let bit_start = bit_start % 8;
        let byte_end = bit_end / 8;
        let bit_end = bit_end % 8;

        if byte_start == byte_end {
            // The allocation fits in a single byte
            if bit_end > bit_start {
                let mask_start = 0x80_u8 >> bit_start;
                let mask_start = mask_start | (mask_start - 1);
                let mask_end = 0x80_u8 >> bit_end;
                let mask_end = !(mask_end | (mask_end - 1));
                let mask = mask_start & mask_end;
                bytes[byte_start] |= mask;
            }
            return Ok(());
        }

        let byte_start = if bit_start == 0 {
            byte_start
        } else {
            let mask_start = 0x80_u8 >> bit_start;
            let mask_start = mask_start | (mask_start - 1);
            bytes[byte_start] |= mask_start;
            byte_start + 1
        };

        if bit_end != 0 {
            let mask_end = 0x80_u8 >> bit_end;
            let mask_end = !(mask_end | (mask_end - 1));
            bytes[byte_end] |= mask_end;
        };

        if byte_end > byte_start {
            for byte in bytes[byte_start..byte_end].iter_mut() {
                *byte = 0xFF;
            }
        }

        Ok(())
    }

    /// Initialize the density list at the beginning of a transaction if it is missing, corrupt, or
    /// the page ID doesn't match the next page ID in the header.
    fn ensure_density_list(&mut self) -> PstResult<()> {
        if let Ok(density_list) = self.density_list.as_ref() {
            if density_list.trailer().block_id() == self.header.next_page() {
                return Ok(());
            }
        }

        let current_page = u32::try_from(
            (self.header.root().amap_last_index().index().into() - AMAP_FIRST_OFFSET)
                / AMAP_DATA_SIZE,
        )
        .map_err(|_| PstError::IntegerConversion)?;
        let block_id = self.header.next_page();
        let signature = PageType::DensityList
            .signature(ndb::page::DENSITY_LIST_FILE_OFFSET, block_id.into_u64());
        let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
            PageType::DensityList,
            signature,
            block_id,
            0,
        );
        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                false,
                current_page,
                &[],
                trailer,
            )?;

        self.density_list = Ok(density_list);
        Ok(())
    }

    /// Similar to [`Self::ensure_density_list`], but instead of resetting the density list, it
    /// assumes that it's already initialized and only updates the page ID if it doesn't match.
    fn update_density_list_page_id(&mut self) -> PstResult<()> {
        let Ok(density_list) = self.density_list.as_ref() else {
            return Ok(());
        };

        let next_page = self.header.next_page();
        if density_list.trailer().block_id() == next_page {
            return Ok(());
        }

        let signature = PageType::DensityList
            .signature(ndb::page::DENSITY_LIST_FILE_OFFSET, next_page.into_u64());
        let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
            PageType::DensityList,
            signature,
            next_page,
            0,
        );

        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                density_list.backfill_complete(),
                density_list.current_page(),
                density_list.entries(),
                trailer,
            )?;

        self.density_list = Ok(density_list);
        Ok(())
    }

    fn read_node(&self, node: NodeId) -> io::Result<<Pst as PstFile>::NodeBTreeEntry> {
        let node_btree = *self.header.root().node_btree();
        let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
        let reader = &mut *reader;
        let node_btree =
            <<Pst as PstFile>::NodeBTree as RootBTreeReadWrite>::read(reader, node_btree)?;
        let mut page_cache = self.node_cache.borrow_mut();
        let node_id: <Pst as PstFile>::BTreeKey = u32::from(node).into();
        let node = node_btree.find_entry(reader, node_id, &mut page_cache)?;
        Ok(node)
    }

    fn read_block(&self, block: <Pst as PstFile>::BlockId) -> io::Result<Vec<u8>> {
        let encoding = self.header.crypt_method();
        let block_btree = *self.header.root().block_btree();
        let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
        let reader = &mut *reader;
        let block_btree =
            <<Pst as PstFile>::BlockBTree as RootBTreeReadWrite>::read(reader, block_btree)?;
        let mut page_cache = self.block_cache.borrow_mut();
        let block = block_btree.find_entry(reader, block.search_key(), &mut page_cache)?;
        let block = DataTree::<Pst>::read(reader, encoding, &block)?;
        let mut block_cache = Default::default();
        let mut data = vec![];
        let _ = block
            .reader(
                reader,
                encoding,
                &block_btree,
                &mut page_cache,
                &mut block_cache,
            )?
            .read_to_end(&mut data)?;
        Ok(data)
    }
}

pub fn open_store(path: impl AsRef<Path>) -> io::Result<Rc<dyn Store>> {
    Ok(if let Ok(pst_file) = UnicodePstFile::open(path.as_ref()) {
        UnicodeStore::read(Rc::new(pst_file))?
    } else {
        let pst_file = AnsiPstFile::open(path.as_ref())?;
        AnsiStore::read(Rc::new(pst_file))?
    })
}
