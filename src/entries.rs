use crate::cluster::ClustersReader;
use crate::disk::DiskPartition;
use crate::FileAttributes;
use byteorder::{ByteOrder, LE};
use std::cmp::min;
use std::fmt::{Display, Formatter};
use std::io::Read;
use thiserror::Error;

/// A struct to read directory entries.
pub(crate) struct EntriesReader<P: DiskPartition> {
    cluster_reader: ClustersReader<P>,
    entry_index: usize,
}

impl<P: DiskPartition> EntriesReader<P> {
    pub fn new(cluster_reader: ClustersReader<P>) -> Self {
        Self {
            cluster_reader,
            entry_index: 0,
        }
    }

    pub fn read(&mut self) -> Result<RawEntry, ReaderError> {
        // Get current cluster and entry index.
        let cluster = self.cluster_reader.cluster();
        let index = self.entry_index;

        // Read directory entry.
        let mut entry = [0u8; 32];

        if let Err(e) = self.cluster_reader.read_exact(&mut entry) {
            return Err(ReaderError::ReadFailed(index, cluster, e));
        }

        // Update entry index.
        if self.cluster_reader.cluster() != cluster {
            self.entry_index = 0;
        } else {
            self.entry_index += 1;
        }

        Ok(RawEntry {
            index,
            cluster,
            data: entry,
        })
    }
}

/// Represents a raw directory entry.
pub(crate) struct RawEntry {
    index: usize,
    cluster: usize,
    data: [u8; 32],
}

impl RawEntry {
    pub fn ty(&self) -> EntryType {
        EntryType(self.data[0])
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn cluster(&self) -> usize {
        self.cluster
    }

    pub fn data(&self) -> &[u8; 32] {
        &self.data
    }
}

/// Represents a File Directory Entry.
pub(crate) struct FileEntry {
    pub name: String,
    pub attributes: FileAttributes,
    pub stream: StreamEntry,
}

impl FileEntry {
    pub fn load<P>(raw: &RawEntry, reader: &mut EntriesReader<P>) -> Result<Self, FileEntryError>
    where
        P: DiskPartition,
    {
        // Load fields.
        let data = &raw.data;
        let secondary_count = data[1] as usize;
        let attributes = FileAttributes(LE::read_u16(&data[4..]));

        if secondary_count < 1 {
            return Err(FileEntryError::NoStreamExtension(raw.index, raw.cluster));
        } else if secondary_count < 2 {
            return Err(FileEntryError::NoFileName(raw.index, raw.cluster));
        }

        // Read stream extension.
        let stream = match reader.read() {
            Ok(v) => v,
            Err(e) => return Err(FileEntryError::ReadStreamFailed(e)),
        };

        // Check if the entry is a stream extension.
        let ty = stream.ty();

        if !ty.is_critical_secondary(0) {
            return Err(FileEntryError::NotStreamExtension(
                stream.index,
                stream.cluster,
            ));
        }

        // Load stream extension.
        let stream = StreamEntry::load(stream, attributes)?;

        // Read file names.
        let name_count = secondary_count - 1;
        let mut names: Vec<RawEntry> = Vec::with_capacity(name_count);

        for i in 0..name_count {
            // Read file name.
            let entry = match reader.read() {
                Ok(v) => v,
                Err(e) => return Err(FileEntryError::ReadFileNameFailed(i, e)),
            };

            // Check if the entry is a file name.
            let ty = entry.ty();

            if !ty.is_critical_secondary(1) {
                return Err(FileEntryError::NotFileName(entry.index, entry.cluster));
            }

            names.push(entry);
        }

        // TODO: Use div_ceil when https://github.com/rust-lang/rust/issues/88581 stabilized.
        if names.len() != (stream.name_length + 15 - 1) / 15 {
            return Err(FileEntryError::WrongFileNames(raw.index, raw.cluster));
        }

        // Construct a complete file name.
        let mut need = stream.name_length * 2;
        let mut name = String::with_capacity(15 * names.len());

        for entry in names {
            let data = entry.data;

            // Load GeneralSecondaryFlags.
            let general_secondary_flags = SecondaryFlags(data[1]);

            if general_secondary_flags.allocation_possible() {
                return Err(FileEntryError::InvalidFileName(entry.index, entry.cluster));
            }

            // Load FileName.
            let raw_name = &data[2..(2 + min(30, need))];

            need -= raw_name.len();

            // Convert FileName from little-endian to native endian.
            let mut file_name = [0u16; 15];
            let file_name = &mut file_name[..(raw_name.len() / 2)];

            LE::read_u16_into(raw_name, file_name);

            match String::from_utf16(file_name) {
                Ok(v) => name.push_str(&v),
                Err(_) => return Err(FileEntryError::InvalidFileName(entry.index, entry.cluster)),
            }
        }

        Ok(Self {
            name,
            attributes,
            stream,
        })
    }
}

/// Represents a Stream Extension Directory Entry.
pub(crate) struct StreamEntry {
    no_fat_chain: bool,
    name_length: usize,
    valid_data_length: u64,
    alloc: ClusterAllocation,
}

impl StreamEntry {
    fn load(raw: RawEntry, attrs: FileAttributes) -> Result<Self, FileEntryError> {
        // Load GeneralSecondaryFlags.
        let data = &raw.data;
        let general_secondary_flags = SecondaryFlags(data[1]);

        if !general_secondary_flags.allocation_possible() {
            return Err(FileEntryError::InvalidStreamExtension(
                raw.index,
                raw.cluster,
            ));
        }

        // Load NameLength.
        let name_length = data[3] as usize;

        if name_length < 1 {
            return Err(FileEntryError::InvalidStreamExtension(
                raw.index,
                raw.cluster,
            ));
        }

        // Load ValidDataLength and cluster allocation.
        let valid_data_length = LE::read_u64(&data[8..]);
        let alloc = match ClusterAllocation::load(&raw) {
            Ok(v) => v,
            Err(_) => {
                return Err(FileEntryError::InvalidStreamExtension(
                    raw.index,
                    raw.cluster,
                ));
            }
        };

        if attrs.is_directory() {
            if valid_data_length != alloc.data_length {
                return Err(FileEntryError::InvalidStreamExtension(
                    raw.index,
                    raw.cluster,
                ));
            }
        } else if valid_data_length > alloc.data_length {
            return Err(FileEntryError::InvalidStreamExtension(
                raw.index,
                raw.cluster,
            ));
        }

        Ok(StreamEntry {
            no_fat_chain: general_secondary_flags.no_fat_chain(),
            name_length,
            valid_data_length,
            alloc,
        })
    }

    pub fn no_fat_chain(&self) -> bool {
        self.no_fat_chain
    }

    pub fn valid_data_length(&self) -> u64 {
        self.valid_data_length
    }

    pub fn allocation(&self) -> &ClusterAllocation {
        &self.alloc
    }
}

/// Encapsulate EntryType field of the directory entry.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(crate) struct EntryType(u8);

impl EntryType {
    pub const PRIMARY: u8 = 0;
    pub const SECONDARY: u8 = 1;
    pub const CRITICAL: u8 = 0;

    pub fn is_regular(self) -> bool {
        self.0 >= 0x81
    }

    pub fn type_code(self) -> u8 {
        self.0 & 0x1f
    }

    pub fn type_importance(self) -> u8 {
        (self.0 & 0x20) >> 5
    }

    pub fn type_category(self) -> u8 {
        (self.0 & 0x40) >> 6
    }

    pub fn is_critical_secondary(self, code: u8) -> bool {
        self.is_regular()
            && self.type_importance() == Self::CRITICAL
            && self.type_category() == Self::SECONDARY
            && self.type_code() == code
    }
}

impl Display for EntryType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.is_regular() {
            if self.type_importance() == Self::CRITICAL {
                f.write_str("critical ")?;
            } else {
                f.write_str("benign ")?;
            }

            if self.type_category() == Self::PRIMARY {
                f.write_str("primary ")?;
            } else {
                f.write_str("secondary ")?;
            }

            write!(f, "{}", self.type_code())
        } else {
            write!(f, "{:#04x}", self.0)
        }
    }
}

/// Represents GeneralSecondaryFlags in the Generic Secondary DirectoryEntry Template.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(crate) struct SecondaryFlags(u8);

impl SecondaryFlags {
    pub fn allocation_possible(self) -> bool {
        (self.0 & 1) != 0
    }

    pub fn no_fat_chain(self) -> bool {
        (self.0 & 2) != 0
    }
}

/// Represents FirstCluster and DataLength fields in the Directory Entry.
#[derive(Debug, Clone)]
pub struct ClusterAllocation {
    first_cluster: usize,
    data_length: u64,
}

impl ClusterAllocation {
    pub(crate) fn load(entry: &RawEntry) -> Result<Self, ClusterAllocationError> {
        // Load fields.
        let data = &entry.data;
        let first_cluster = LE::read_u32(&data[20..]) as usize;
        let data_length = LE::read_u64(&data[24..]);

        // Check values.
        if first_cluster == 0 {
            if data_length != 0 {
                return Err(ClusterAllocationError::InvalidDataLength);
            }
        } else if first_cluster < 2 {
            return Err(ClusterAllocationError::InvalidFirstCluster);
        }

        Ok(Self {
            first_cluster,
            data_length,
        })
    }

    pub(crate) fn first_cluster(&self) -> usize {
        self.first_cluster
    }

    pub(crate) fn data_length(&self) -> u64 {
        self.data_length
    }
}

impl Display for ClusterAllocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.first_cluster, self.data_length)
    }
}

/// Represents an error for [`read()`][EntriesReader::read()].
#[derive(Debug, Error)]
pub enum ReaderError {
    #[error("cannot read entry #{0} on cluster #{1}")]
    ReadFailed(usize, usize, #[source] std::io::Error),
}

/// Represents an error for [`load()`][FileEntry::load()].
#[derive(Debug, Error)]
pub enum FileEntryError {
    #[error("no stream extension is followed the entry #{0} on cluster #{1}")]
    NoStreamExtension(usize, usize),

    #[error("no file name is followed the entry #{0} on cluster #{1}")]
    NoFileName(usize, usize),

    #[error("cannot read stream extension")]
    ReadStreamFailed(#[source] ReaderError),

    #[error("entry #{0} on cluster #{1} is not a stream extension")]
    NotStreamExtension(usize, usize),

    #[error("entry #{0} on cluster #{1} is not a valid stream extension")]
    InvalidStreamExtension(usize, usize),

    #[error("cannot read file name #{0}")]
    ReadFileNameFailed(usize, #[source] ReaderError),

    #[error("entry #{0} on cluster #{1} is not a file name")]
    NotFileName(usize, usize),

    #[error("entry #{0} on cluster #{1} has wrong number of file names")]
    WrongFileNames(usize, usize),

    #[error("entry #{0} on cluster #{1} is not a valid file name")]
    InvalidFileName(usize, usize),
}

/// Represents an error for [`load()`][ClusterAllocation::load()].
#[derive(Debug, Error)]
pub enum ClusterAllocationError {
    #[error("invalid FirstCluster")]
    InvalidFirstCluster,

    #[error("invalid DataLength")]
    InvalidDataLength,
}
