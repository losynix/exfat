use self::cluster::ClustersReader;
use self::directory::{Directory, Item};
use self::disk::DiskPartition;
use self::entries::{ClusterAllocation, EntriesReader, EntryType, FileEntry};
use self::fat::Fat;
use self::file::File;
use self::param::Params;
use byteorder::{ByteOrder, LE};
use std::error::Error;
use std::sync::Arc;
use thiserror::Error;

pub mod cluster;
pub mod directory;
pub mod disk;
pub mod entries;
pub mod fat;
pub mod file;
pub mod image;
pub mod param;

/// Represents a root directory in exFAT.
///
/// This implementation follows the official specs
/// https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification.
pub struct Root<P: DiskPartition> {
    volume_label: Option<String>,
    items: Vec<Item<P>>,
}

impl<P: DiskPartition> Root<P> {
    pub fn open(partition: P) -> Result<Self, OpenError> {
        // Read boot sector.
        let mut boot = [0u8; 512];

        if let Err(e) = partition.read_exact(0, &mut boot) {
            return Err(OpenError::ReadMainBootFailed(e));
        }

        // Check type.
        if &boot[3..11] != b"EXFAT   " || !boot[11..64].iter().all(|&b| b == 0) {
            return Err(OpenError::NotExFat);
        }

        // Load fields.
        let params = Params {
            fat_offset: LE::read_u32(&boot[80..]) as u64,
            fat_length: LE::read_u32(&boot[84..]) as u64,
            cluster_heap_offset: LE::read_u32(&boot[88..]) as u64,
            cluster_count: LE::read_u32(&boot[92..]) as usize,
            first_cluster_of_root_directory: LE::read_u32(&boot[96..]) as usize,
            volume_flags: LE::read_u16(&boot[106..]).into(),
            bytes_per_sector: {
                let v = boot[108];

                if (9..=12).contains(&v) {
                    1u64 << v
                } else {
                    return Err(OpenError::InvalidBytesPerSectorShift);
                }
            },
            sectors_per_cluster: {
                let v = boot[109];

                // No need to check if subtraction is underflow because we already checked for the
                // valid value on the above.
                if v <= (25 - boot[108]) {
                    1u64 << v
                } else {
                    return Err(OpenError::InvalidSectorsPerClusterShift);
                }
            },
            number_of_fats: {
                let v = boot[110];

                if v == 1 || v == 2 {
                    v
                } else {
                    return Err(OpenError::InvalidNumberOfFats);
                }
            },
        };

        // Read FAT region.
        let active_fat = params.volume_flags.active_fat();
        let fat = if active_fat == 0 || params.number_of_fats == 2 {
            match Fat::load(&params, &partition, active_fat) {
                Ok(v) => v,
                Err(e) => return Err(OpenError::ReadFatRegionFailed(e)),
            }
        } else {
            return Err(OpenError::InvalidNumberOfFats);
        };

        // Create a entries reader for the root directory.
        let root_cluster = params.first_cluster_of_root_directory;
        let exfat = Arc::new(ExFat {
            partition,
            params,
            fat,
        });

        let mut reader = match ClustersReader::new(exfat.clone(), root_cluster, None, None) {
            Ok(v) => EntriesReader::new(v),
            Err(e) => return Err(OpenError::CreateClustersReaderFailed(e)),
        };

        // Load root directory.
        let mut allocation_bitmaps: [Option<ClusterAllocation>; 2] = [None, None];
        let mut upcase_table: Option<()> = None;
        let mut volume_label: Option<String> = None;
        let mut items: Vec<Item<P>> = Vec::new();

        loop {
            // Read primary entry.
            let entry = match reader.read() {
                Ok(v) => v,
                Err(e) => return Err(OpenError::ReadEntryFailed(e)),
            };

            // Check entry type.
            let ty = entry.ty();

            if !ty.is_regular() {
                break;
            } else if ty.type_category() != EntryType::PRIMARY {
                return Err(OpenError::NotPrimaryEntry(entry.index(), entry.cluster()));
            }

            // Parse primary entry.
            match (ty.type_importance(), ty.type_code()) {
                (EntryType::CRITICAL, 1) => {
                    // Get next index.
                    let index = if allocation_bitmaps[1].is_some() {
                        return Err(OpenError::TooManyAllocationBitmap);
                    } else if allocation_bitmaps[0].is_some() {
                        1
                    } else {
                        0
                    };

                    // Load fields.
                    let data = entry.data();
                    let bitmap_flags = data[1] as usize;

                    if (bitmap_flags & 1) != index {
                        return Err(OpenError::WrongAllocationBitmap);
                    }

                    allocation_bitmaps[index] = match ClusterAllocation::load(&entry) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            return Err(OpenError::ReadClusterAllocationFailed(
                                entry.index(),
                                entry.cluster(),
                                e,
                            ));
                        }
                    };
                }
                (EntryType::CRITICAL, 2) => {
                    // Check if more than one up-case table.
                    if upcase_table.is_some() {
                        return Err(OpenError::MultipleUpcaseTable);
                    }

                    // Load fields.
                    if let Err(e) = ClusterAllocation::load(&entry) {
                        return Err(OpenError::ReadClusterAllocationFailed(
                            entry.index(),
                            entry.cluster(),
                            e,
                        ));
                    }

                    upcase_table = Some(());
                }
                (EntryType::CRITICAL, 3) => {
                    // Check if more than one volume label.
                    if volume_label.is_some() {
                        return Err(OpenError::MultipleVolumeLabel);
                    }

                    // Load fields.
                    let data = entry.data();
                    let character_count = data[1] as usize;

                    if character_count > 11 {
                        return Err(OpenError::InvalidVolumeLabel);
                    }

                    let raw_label = &data[2..(2 + character_count * 2)];

                    // Convert the label from little endian to native endian.
                    let mut label = [0u16; 11];
                    let label = &mut label[..character_count];

                    LE::read_u16_into(raw_label, label);

                    volume_label = Some(String::from_utf16_lossy(label));
                }
                (EntryType::CRITICAL, 5) => {
                    // Load the entry.
                    let file = match FileEntry::load(&entry, &mut reader) {
                        Ok(v) => v,
                        Err(e) => return Err(OpenError::LoadFileEntryFailed(e)),
                    };

                    let name = file.name;
                    let attrs = file.attributes;
                    let stream = file.stream;

                    // Add to the list.
                    items.push(if attrs.is_directory() {
                        Item::Directory(Directory::new(exfat.clone(), name, stream))
                    } else {
                        match File::new(exfat.clone(), name, stream) {
                            Ok(v) => Item::File(v),
                            Err(e) => {
                                return Err(OpenError::CreateFileObjectFailed(
                                    entry.index(),
                                    entry.cluster(),
                                    e,
                                ));
                            }
                        }
                    });
                }
                _ => return Err(OpenError::UnknownEntry(entry.index(), entry.cluster())),
            }
        }

        // Check allocation bitmap count.
        if exfat.params.number_of_fats == 2 {
            if allocation_bitmaps[1].is_none() {
                return Err(OpenError::NoAllocationBitmap);
            }
        } else if allocation_bitmaps[0].is_none() {
            return Err(OpenError::NoAllocationBitmap);
        }

        // Check Up-case Table.
        if upcase_table.is_none() {
            return Err(OpenError::NoUpcaseTable);
        }

        Ok(Self {
            volume_label,
            items,
        })
    }

    pub fn volume_label(&self) -> Option<&str> {
        self.volume_label.as_deref()
    }
}

impl<P: DiskPartition> IntoIterator for Root<P> {
    type Item = Item<P>;
    type IntoIter = std::vec::IntoIter<Item<P>>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

/// Represents FileAttributes in the File Directory Entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct FileAttributes(u16);

impl FileAttributes {
    pub fn is_read_only(self) -> bool {
        (self.0 & 0x0001) != 0
    }

    pub fn is_hidden(self) -> bool {
        (self.0 & 0x0002) != 0
    }

    pub fn is_system(self) -> bool {
        (self.0 & 0x0004) != 0
    }

    pub fn is_directory(self) -> bool {
        (self.0 & 0x0010) != 0
    }

    pub fn is_archive(self) -> bool {
        (self.0 & 0x0020) != 0
    }
}

/// Contains objects for the opened exFAT.
pub(crate) struct ExFat<P: DiskPartition> {
    partition: P,
    params: Params,
    fat: Fat,
}

/// Represents an error for [`Root::open()`].
#[derive(Debug, Error)]
pub enum OpenError {
    #[error("cannot read main boot region")]
    ReadMainBootFailed(#[source] Box<dyn Error + Send + Sync>),

    #[error("image is not exFAT")]
    NotExFat,

    #[error("invalid BytesPerSectorShift")]
    InvalidBytesPerSectorShift,

    #[error("invalid SectorsPerClusterShift")]
    InvalidSectorsPerClusterShift,

    #[error("invalid NumberOfFats")]
    InvalidNumberOfFats,

    #[error("cannot read FAT region")]
    ReadFatRegionFailed(#[source] fat::LoadError),

    #[error("cannot create a clusters reader")]
    CreateClustersReaderFailed(#[source] cluster::NewError),

    #[error("cannot read a directory entry")]
    ReadEntryFailed(#[source] entries::ReaderError),

    #[error("directory entry #{0} on cluster #{1} is not a primary entry")]
    NotPrimaryEntry(usize, usize),

    #[error("more than 2 allocation bitmaps exists in the root directory")]
    TooManyAllocationBitmap,

    #[error("allocation bitmap in the root directory is not for its corresponding FAT")]
    WrongAllocationBitmap,

    #[error("multiple up-case table exists in the root directory")]
    MultipleUpcaseTable,

    #[error("multiple volume label exists in the root directory")]
    MultipleVolumeLabel,

    #[error("invalid volume label")]
    InvalidVolumeLabel,

    #[error("cannot load file entry in the root directory")]
    LoadFileEntryFailed(#[source] entries::FileEntryError),

    #[error("cannot create a file object for directory entry #{0} on cluster #{1}")]
    CreateFileObjectFailed(usize, usize, #[source] file::NewError),

    #[error("cannot read cluster allocation for entry #{0} on cluster #{1}")]
    ReadClusterAllocationFailed(usize, usize, #[source] entries::ClusterAllocationError),

    #[error("unknown directory entry #{0} on cluster #{1}")]
    UnknownEntry(usize, usize),

    #[error("no Allocation Bitmap available for active FAT")]
    NoAllocationBitmap,

    #[error("no Up-case Table available")]
    NoUpcaseTable,
}
