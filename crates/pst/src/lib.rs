#![doc = include_str!("../README.md")]

use byteorder::WriteBytesExt;
use std::{
    cell::RefMut,
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

/// Plain-message input supported by the format-spike writer.
pub struct UnicodePstMessage<'a> {
    pub subject: &'a str,
    pub sender_name: &'a str,
    pub sender_email: &'a str,
    pub recipient_name: &'a str,
    pub recipient_email: &'a str,
    pub body: &'a str,
    pub message_id: &'a str,
    /// Number of 100-nanosecond intervals since 1601-01-01 UTC.
    pub delivery_time: i64,
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

    /// Create a new Unicode PST containing Inbox and one plain-text message.
    pub fn create(path: impl AsRef<Path>, message: &UnicodePstMessage<'_>) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message)?;
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

const NEW_PST_BBT_OFFSET: u64 = PMAP_FIRST_OFFSET + PAGE_SIZE as u64;
const NEW_PST_NBT_OFFSET: u64 = NEW_PST_BBT_OFFSET + PAGE_SIZE as u64;
const NEW_PST_DATA_OFFSET: u64 = NEW_PST_NBT_OFFSET + PAGE_SIZE as u64;

const NEW_PST_RECORD_KEY: [u8; 16] = [
    0x4D, 0x45, 0x4D, 0x45, 0x58, 0x2D, 0x50, 0x53, 0x54, 0x2D, 0x56, 0x32, 0x33, 0x00, 0x00, 0x01,
];

fn heap_id(allocation_index: usize) -> io::Result<HeapId> {
    Ok(HeapId::new(
        u16::try_from(allocation_index + 1).map_err(|_| PstError::IntegerConversion)?,
        0,
    )?)
}

fn entry_id_bytes(node_id: NodeId) -> io::Result<Vec<u8>> {
    let entry_id = EntryId::new(StoreRecordKey::new(NEW_PST_RECORD_KEY), node_id);
    Vec::try_from(&entry_id)
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
    let page_map_offset = allocations.iter().map(Vec::len).sum::<usize>() + HEAP_HEADER_SIZE;
    let page_map_offset =
        u16::try_from(page_map_offset).map_err(|_| PstError::IntegerConversion)?;
    let header = HeapNodeHeader::new(
        page_map_offset,
        client_signature,
        user_root,
        [HeapFillLevel::Empty; 8],
    );
    header.write(&mut allocations[0])?;

    let mut data = Vec::new();
    let mut offsets = Vec::with_capacity(allocations.len() + 1);
    for allocation in allocations {
        offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
        data.extend_from_slice(&allocation);
    }
    offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);

    data.write_u16::<byteorder::LittleEndian>(
        u16::try_from(offsets.len() - 1).map_err(|_| PstError::IntegerConversion)?,
    )?;
    data.write_u16::<byteorder::LittleEndian>(0)?;
    for offset in offsets {
        data.write_u16::<byteorder::LittleEndian>(offset)?;
    }
    Ok(data)
}

fn property_context_data(
    properties: &[(u16, PropertyType, Option<Vec<u8>>, u32)],
) -> io::Result<Vec<u8>> {
    let mut allocations = vec![vec![], vec![], vec![]];
    let mut records = Vec::with_capacity(properties.len());

    for (prop_id, prop_type, value, small) in properties {
        let record = if let Some(value) = value {
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

fn recipient_table_data(
    row_id: NodeId,
    recipient_name: &str,
    recipient_email: &str,
) -> io::Result<Vec<u8>> {
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0C15, 8, 4, 2),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3001, 12, 4, 3),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x39FE, 16, 4, 4),
    ];
    let context = TableContextInfo::new(
        20,
        20,
        20,
        21,
        heap_id(2)?,
        Some(NodeId::from(u32::from(heap_id(4)?))),
        columns,
    )?;
    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;

    let mut tree_header_bytes = Vec::new();
    HeapTreeHeader::new(4, 4, 0, heap_id(3)?)?.write(&mut tree_header_bytes)?;

    let mut index_bytes = Vec::new();
    TableRowId::new(u32::from(row_id)).write(&mut index_bytes)?;
    index_bytes.write_u32::<byteorder::LittleEndian>(0)?;

    let mut row = Vec::new();
    row.write_u32::<byteorder::LittleEndian>(u32::from(row_id))?;
    row.write_u32::<byteorder::LittleEndian>(0)?;
    row.write_i32::<byteorder::LittleEndian>(1)?;
    row.write_u32::<byteorder::LittleEndian>(u32::from(heap_id(5)?))?;
    row.write_u32::<byteorder::LittleEndian>(u32::from(heap_id(6)?))?;
    row.push(0xF8);

    finish_heap(
        vec![
            vec![],
            context_bytes,
            tree_header_bytes,
            index_bytes,
            row,
            utf16_bytes(recipient_name),
            utf16_bytes(recipient_email),
        ],
        HeapNodeType::Table,
        heap_id(1)?,
    )
}

enum NewBlockData {
    Data(Vec<u8>),
    SubNodes(Vec<UnicodeLeafSubNodeTreeEntry>),
}

struct NewBlock {
    node: Option<(NodeId, Option<NodeId>, Option<UnicodeBlockId>)>,
    id: UnicodeBlockId,
    offset: u64,
    size: u16,
    data: NewBlockData,
}

fn create_unicode_pst(path: &Path, input: &UnicodePstMessage<'_>) -> io::Result<()> {
    let ipm_subtree = NodeId::new(NodeIdType::NormalFolder, 0x20)?;
    let inbox = NodeId::new(NodeIdType::NormalFolder, 0x21)?;
    let ipm_hierarchy = NodeId::new(NodeIdType::HierarchyTable, ipm_subtree.index())?;
    let inbox_contents = NodeId::new(NodeIdType::ContentsTable, inbox.index())?;
    let message = NodeId::new(NodeIdType::NormalMessage, 0x40)?;
    let recipient_table = NodeId::new(NodeIdType::RecipientTable, message.index())?;
    let store_data = property_context_data(&[
        (0x0E38, PropertyType::Integer32, None, 0),
        (
            0x0FF9,
            PropertyType::Binary,
            Some(NEW_PST_RECORD_KEY.to_vec()),
            0,
        ),
        (0x3001, PropertyType::Unicode, Some(utf16_bytes("Memex")), 0),
        (
            0x35E0,
            PropertyType::Binary,
            Some(entry_id_bytes(ipm_subtree)?),
            0,
        ),
    ])?;
    let ipm_data = property_context_data(&[
        (
            0x3001,
            PropertyType::Unicode,
            Some(utf16_bytes("Top of Personal Folders")),
            0,
        ),
        (0x3602, PropertyType::Integer32, None, 0),
        (0x3603, PropertyType::Integer32, None, 0),
        (0x360A, PropertyType::Boolean, None, 1),
        (
            0x3613,
            PropertyType::Unicode,
            Some(utf16_bytes("IPF.Note")),
            0,
        ),
    ])?;
    let inbox_data = property_context_data(&[
        (0x3001, PropertyType::Unicode, Some(utf16_bytes("Inbox")), 0),
        (0x3602, PropertyType::Integer32, None, 0),
        (0x3603, PropertyType::Integer32, None, 0),
        (0x360A, PropertyType::Boolean, None, 0),
        (
            0x3613,
            PropertyType::Unicode,
            Some(utf16_bytes("IPF.Note")),
            0,
        ),
    ])?;

    let message_data = property_context_data(&[
        (
            0x001A,
            PropertyType::Unicode,
            Some(utf16_bytes("IPM.Note")),
            0,
        ),
        (
            0x0037,
            PropertyType::Unicode,
            Some(utf16_bytes(input.subject)),
            0,
        ),
        (
            0x0039,
            PropertyType::Time,
            Some(input.delivery_time.to_le_bytes().to_vec()),
            0,
        ),
        (
            0x0C1A,
            PropertyType::Unicode,
            Some(utf16_bytes(input.sender_name)),
            0,
        ),
        (
            0x0C1F,
            PropertyType::Unicode,
            Some(utf16_bytes(input.sender_email)),
            0,
        ),
        (
            0x0E06,
            PropertyType::Time,
            Some(input.delivery_time.to_le_bytes().to_vec()),
            0,
        ),
        (0x0E07, PropertyType::Integer32, None, 1),
        (
            0x1000,
            PropertyType::Unicode,
            Some(utf16_bytes(input.body)),
            0,
        ),
        (
            0x1035,
            PropertyType::Unicode,
            Some(utf16_bytes(input.message_id)),
            0,
        ),
        (0x3FDE, PropertyType::Integer32, None, 65001),
    ])?;
    let recipient_data =
        recipient_table_data(recipient_table, input.recipient_name, input.recipient_email)?;

    let seeds = vec![
        (NID_MESSAGE_STORE, None, store_data),
        (ipm_subtree, Some(NID_ROOT_FOLDER), ipm_data),
        (ipm_hierarchy, None, table_context_data(&[inbox])?),
        (inbox, Some(ipm_subtree), inbox_data),
        (inbox_contents, None, table_context_data(&[message])?),
        (message, Some(inbox), message_data),
        (recipient_table, None, recipient_data),
    ];
    let mut blocks = Vec::with_capacity(seeds.len() + 1);
    let mut next_offset = NEW_PST_DATA_OFFSET;
    for (index, (node_id, parent, data)) in seeds.into_iter().enumerate() {
        let block_id = UnicodeBlockId::new(
            false,
            u64::try_from(index + 1).map_err(|_| PstError::IntegerConversion)?,
        )?;
        let size = u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?;
        let disk_size = u64::from(ndb::block::block_size(size + UnicodeBlockTrailer::SIZE));
        blocks.push(NewBlock {
            node: Some((node_id, parent, None)),
            id: block_id,
            offset: next_offset,
            size,
            data: NewBlockData::Data(data),
        });
        next_offset += disk_size;
    }
    let recipient_block = blocks.last().unwrap().id;
    let sub_node_id = UnicodeBlockId::new(
        true,
        u64::try_from(blocks.len() + 1).map_err(|_| PstError::IntegerConversion)?,
    )?;
    let sub_node_entries = vec![UnicodeLeafSubNodeTreeEntry::new(
        recipient_table,
        recipient_block,
        None,
    )];
    let sub_node_size =
        UnicodeSubNodeTreeBlockHeader::HEADER_SIZE + UnicodeLeafSubNodeTreeEntry::ENTRY_SIZE;
    let disk_size = u64::from(ndb::block::block_size(
        sub_node_size + UnicodeBlockTrailer::SIZE,
    ));
    blocks.push(NewBlock {
        node: None,
        id: sub_node_id,
        offset: next_offset,
        size: sub_node_size,
        data: NewBlockData::SubNodes(sub_node_entries),
    });
    next_offset += disk_size;
    if let Some((block, parent)) = blocks.iter_mut().find_map(|block| match block.node {
        Some((node, parent, _)) if node == message => Some((block, parent)),
        _ => None,
    }) {
        block.node = Some((message, parent, Some(sub_node_id)));
    }
    let new_pst_eof = next_offset;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.set_len(new_pst_eof)?;

    let amap_id = UnicodePageId::from(1);
    let pmap_id = UnicodePageId::from(2);
    let bbt_id = UnicodePageId::from(3);
    let nbt_id = UnicodePageId::from(4);

    let bbt_ref = UnicodePageRef::new(bbt_id, UnicodeByteIndex::new(NEW_PST_BBT_OFFSET));
    let nbt_ref = UnicodePageRef::new(nbt_id, UnicodeByteIndex::new(NEW_PST_NBT_OFFSET));
    let root = UnicodeRoot::new(
        UnicodeByteIndex::new(new_pst_eof),
        UnicodeByteIndex::new(AMAP_FIRST_OFFSET),
        UnicodeByteIndex::new(AMAP_DATA_SIZE - (new_pst_eof - AMAP_FIRST_OFFSET)),
        UnicodeByteIndex::new(0),
        nbt_ref,
        bbt_ref,
        AmapStatus::Valid2,
    );
    let header = UnicodeHeader::new_file(
        root,
        NdbCryptMethod::None,
        UnicodePageId::from(5),
        UnicodeBlockId::new(
            false,
            u64::try_from(blocks.len() + 1).map_err(|_| PstError::IntegerConversion)?,
        )?,
    );
    file.seek(SeekFrom::Start(0))?;
    header.write(&mut file)?;

    let mut amap_bits = [0; mem::size_of::<MapBits>()];
    let occupied_slots = (new_pst_eof - AMAP_FIRST_OFFSET) / 64;
    for slot in 0..occupied_slots {
        amap_bits[(slot / 8) as usize] |= 0x80 >> (slot % 8);
    }
    let amap_trailer = UnicodePageTrailer::new(PageType::AllocationMap, 0, amap_id, 0);
    let amap = <UnicodeMapPage<{ PageType::AllocationMap as u8 }> as MapPageReadWrite<
        UnicodePstFile,
        { PageType::AllocationMap as u8 },
    >>::new(amap_bits, amap_trailer)?;
    file.seek(SeekFrom::Start(AMAP_FIRST_OFFSET))?;
    <UnicodeMapPage<{ PageType::AllocationMap as u8 }> as MapPageReadWrite<
        UnicodePstFile,
        { PageType::AllocationMap as u8 },
    >>::write(&amap, &mut file)?;

    let pmap_trailer = UnicodePageTrailer::new(PageType::AllocationPageMap, 0, pmap_id, 0);
    let pmap = <UnicodeMapPage<{ PageType::AllocationPageMap as u8 }> as MapPageReadWrite<
        UnicodePstFile,
        { PageType::AllocationPageMap as u8 },
    >>::new([0xFF; mem::size_of::<MapBits>()], pmap_trailer)?;
    file.seek(SeekFrom::Start(PMAP_FIRST_OFFSET))?;
    <UnicodeMapPage<{ PageType::AllocationPageMap as u8 }> as MapPageReadWrite<
        UnicodePstFile,
        { PageType::AllocationPageMap as u8 },
    >>::write(&pmap, &mut file)?;

    let bbt_trailer = UnicodePageTrailer::new(
        PageType::BlockBTree,
        PageType::BlockBTree.signature(NEW_PST_BBT_OFFSET, bbt_id.into_u64()),
        bbt_id,
        0,
    );
    let bbt_entries: Vec<_> = blocks
        .iter()
        .map(|block| {
            UnicodeBlockBTreeEntry::new(
                UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                block.size,
            )
        })
        .collect();
    let bbt_page = UnicodeBlockBTreePage::new(0, 20, 24, &bbt_entries, bbt_trailer)?;
    let bbt = UnicodeBlockBTree::Leaf(Box::new(bbt_page));
    bbt.write(&mut file, bbt_ref)?;

    let nbt_trailer = UnicodePageTrailer::new(
        PageType::NodeBTree,
        PageType::NodeBTree.signature(NEW_PST_NBT_OFFSET, nbt_id.into_u64()),
        nbt_id,
        0,
    );
    let mut nbt_entries: Vec<_> = blocks
        .iter()
        .filter_map(|block| {
            block.node.map(|(node_id, parent, sub_node)| {
                UnicodeNodeBTreeEntry::new(node_id, block.id, sub_node, parent)
            })
        })
        .collect();
    nbt_entries.sort_by_key(|entry| entry.key());
    let nbt_page = UnicodeNodeBTreePage::new(0, 15, 32, &nbt_entries, nbt_trailer)?;
    let nbt = UnicodeNodeBTree::Leaf(Box::new(nbt_page));
    nbt.write(&mut file, nbt_ref)?;

    for block in blocks {
        let block_signature =
            block_sig::compute_sig(block.offset as u32, u64::from(block.id) as u32);
        let trailer = UnicodeBlockTrailer::new(block.size, block_signature, 0, block.id)?;
        file.seek(SeekFrom::Start(block.offset))?;
        match block.data {
            NewBlockData::Data(data) => {
                UnicodeDataBlock::new(NdbCryptMethod::None, data, trailer)?.write(&mut file)?;
            }
            NewBlockData::SubNodes(entries) => {
                let header = UnicodeSubNodeTreeBlockHeader::new(
                    0,
                    u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
                );
                let tree = UnicodeLeafSubNodeTreeBlock::new(header, entries, trailer)?;
                SubNodeTree::<UnicodePstFile>::Leaf(Box::new(tree)).write(
                    &mut file,
                    &UnicodeBlockBTreeEntry::new(
                        UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                        block.size,
                    ),
                )?;
            }
        }
    }

    file.sync_all()
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod create_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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
            recipient_name: "Memex Recipient",
            recipient_email: "recipient@example.com",
            body: "Hello from Memex PST export.",
            message_id: "<memex-pst-export@example.com>",
            delivery_time: 133_750_080_000_000_000,
        };
        let pst = UnicodePstFile::create(&path, &input).unwrap();
        assert_eq!(pst.header().version(), NdbVersion::Unicode);
        assert!(pst.header().root().file_eof_index().index() > NEW_PST_DATA_OFFSET);
        drop(pst);

        let reopened = UnicodePstFile::open(&path).unwrap();
        assert_eq!(reopened.header().version(), NdbVersion::Unicode);
        assert!(!reopened
            .read_block(UnicodeBlockId::new(false, 1).unwrap())
            .unwrap()
            .is_empty());
        {
            let mut reader = reopened.reader().lock().unwrap();
            let root = reopened.header().root();
            let nbt = UnicodeNodeBTree::read(&mut *reader, *root.node_btree()).unwrap();
            let bbt = UnicodeBlockBTree::read(&mut *reader, *root.block_btree()).unwrap();
            let node = nbt
                .find_entry(
                    &mut *reader,
                    u64::from(u32::from(NID_MESSAGE_STORE)),
                    &mut Default::default(),
                )
                .unwrap();
            let heap = UnicodeHeapNode::read(
                &mut *reader,
                &bbt,
                &mut Default::default(),
                NdbCryptMethod::None,
                node.data().search_key(),
            )
            .unwrap();
            let heap_header = heap.header().unwrap();
            let tree = UnicodeHeapTree::<PropertyTreeRecordKey, PropertyTreeRecordValue>::new(
                heap,
                heap_header.user_root(),
            );
            let tree_header = tree.header().unwrap();
            assert_eq!(tree_header.key_size(), 2);
            assert_eq!(
                tree.heap().find_entry(tree_header.root()).unwrap().len(),
                32
            );
            assert_eq!(tree.entries().unwrap().len(), 4);
        }
        let store = UnicodeStore::read(Rc::new(reopened)).unwrap();
        assert_eq!(store.properties().display_name().unwrap(), "Memex");
        assert_eq!(
            store
                .properties()
                .ipm_sub_tree_entry_id()
                .unwrap()
                .node_id(),
            NodeId::new(NodeIdType::NormalFolder, 0x20).unwrap()
        );
        let ipm = store
            .open_folder(&store.properties().ipm_sub_tree_entry_id().unwrap())
            .unwrap();
        assert_eq!(
            ipm.properties().display_name().unwrap(),
            "Top of Personal Folders"
        );
        let inbox_row = ipm.hierarchy_table().unwrap().rows_matrix().next().unwrap();
        let inbox_id = NodeId::from(u32::from(inbox_row.id()));
        let inbox = store
            .open_folder(&store.properties().make_entry_id(inbox_id).unwrap())
            .unwrap();
        assert_eq!(inbox.properties().display_name().unwrap(), "Inbox");
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
        let row = recipients.rows_matrix().next().unwrap();
        let values = row.columns(recipients.context()).unwrap();
        let smtp_column = recipients
            .context()
            .columns()
            .iter()
            .zip(values)
            .find(|(column, _)| column.prop_id() == 0x39FE)
            .unwrap();
        let smtp = recipients
            .read_column(smtp_column.1.as_ref().unwrap(), smtp_column.0.prop_type())
            .unwrap();
        match smtp {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "recipient@example.com")
            }
            value => panic!("unexpected recipient value: {value:?}"),
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
            .map(|index| {
                let has_pmap_page = index % 8 == 0;
                let has_fmap_page = has_pmap_page
                    && index >= FMAP_FIRST_SIZE
                    && (index - FMAP_FIRST_SIZE) % FMAP_PAGE_COUNT == 0;
                let has_fpmap_page = has_pmap_page
                    && index >= FPMAP_FIRST_SIZE
                    && (index - FPMAP_FIRST_SIZE) % FPMAP_PAGE_COUNT == 0;

                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * AMAP_DATA_SIZE + AMAP_FIRST_OFFSET,
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
                let mut reserved = 1;
                if has_pmap_page {
                    reserved += 1;
                }
                if has_fmap_page {
                    reserved += 1;
                }
                if has_fpmap_page {
                    reserved += 1;
                }

                let free_space = AMAP_DATA_SIZE - (reserved * PAGE_SIZE) as u64;

                let reserved = &[0xFF; 4][..reserved];
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

        let pmap_pages: Vec<_> = (0..=(num_amap_pages / 8))
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
