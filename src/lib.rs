use byteorder::{BigEndian, ReadBytesExt};
use nbt::decode::TagDecodeError;
use nbt::decode::{read_gzip_compound_tag, read_zlib_compound_tag};
use nbt::CompoundTag;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::{fs, io};

/// Amount of chunks in region.
const REGION_CHUNKS: usize = 1024;
/// Length of chunks metadata in region.
const REGION_CHUNKS_METADATA_LENGTH: usize = 2 * REGION_CHUNKS;
/// Region header length in bytes.
const REGION_HEADER_BYTES_LENGTH: u64 = 8 * REGION_CHUNKS as u64;
/// Region sector length in bytes.
const REGION_SECTOR_BYTES_LENGTH: u16 = 4096;

/// Possible errors while loading the chunk.
#[derive(Debug)]
pub enum ChunkLoadError {
    /// Region at specified coordinates not found.
    RegionNotFound { region_x: i32, region_z: i32 },
    /// Chunk at specified coordinates inside region not found.
    ChunkNotFound { chunk_x: u8, chunk_z: u8 },
    /// Chunk length overlaps declared maximum.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted.
    LengthExceedsMaximum {
        /// Chunk length.
        length: u32,
        /// Chunk maximum expected length.
        maximum_length: u32,
    },
    /// Currently are only 2 types of compression: Gzip and Zlib.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted or was introduced new compression type.
    UnsupportedCompressionScheme {
        /// Compression scheme type id.
        compression_scheme: u8,
    },
    /// I/O Error which happened while were reading chunk data from region file.
    ReadError { io_error: io::Error },
    /// Error while decoding binary data to NBT tag.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted or a developer error in the NBT library.
    TagDecodeError { tag_decode_error: TagDecodeError },
}

impl From<io::Error> for ChunkLoadError {
    fn from(io_error: io::Error) -> Self {
        ChunkLoadError::ReadError { io_error }
    }
}

impl From<TagDecodeError> for ChunkLoadError {
    fn from(tag_decode_error: TagDecodeError) -> Self {
        ChunkLoadError::TagDecodeError { tag_decode_error }
    }
}

pub struct AnvilChunkProvider<P> {
    /// Folder where region files located.
    folder: P,
}

impl<P: AsRef<Path>> AnvilChunkProvider<P> {
    pub fn new(folder: P) -> Self {
        AnvilChunkProvider { folder }
    }

    pub fn load_chunk(&self, chunk_x: i32, chunk_z: i32) -> Result<CompoundTag, ChunkLoadError> {
        let region_x = chunk_x >> 5;
        let region_z = chunk_z >> 5;

        let region_chunk_x = (chunk_x & 31) as u8;
        let region_chunk_z = (chunk_z & 31) as u8;

        let region_name = format!("r.{}.{}.mca", region_x, region_z);
        let region_path = self.folder.as_ref().join(region_name);

        if !region_path.exists() {
            return Err(ChunkLoadError::RegionNotFound { region_x, region_z });
        }

        // TODO: Cache region files.
        let mut region = AnvilRegion::new(region_path)?;

        region.read_chunk(region_chunk_x, region_chunk_z)
    }

    pub fn save_chunk(&self, chunk_compound_tag: CompoundTag) -> Result<(), io::Error> {
        let folder_ref = self.folder.as_ref();

        if !folder_ref.exists() {
            fs::create_dir(folder_ref)?;
        }

        Ok(())
    }
}

/// Region represents a 32x32 group of chunks.
struct AnvilRegion {
    /// File in which region are stored.
    file: File,
    /// Array of chunks metadata.
    chunks_metadata: [AnvilChunkMetadata; REGION_CHUNKS],
}

/// Chunk metadata are stored in header.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
struct AnvilChunkMetadata {
    /// Sector index from which starts chunk data.
    sector_index: u32,
    /// Amount of sectors used to store chunk.
    sectors: u8,
    /// Last time chunk was modified.
    last_modified_timestamp: u32,
}

impl AnvilChunkMetadata {
    fn new(sector_index: u32, sectors: u8, last_modified_timestamp: u32) -> Self {
        AnvilChunkMetadata {
            sector_index,
            sectors,
            last_modified_timestamp,
        }
    }

    fn is_empty(&self) -> bool {
        self.sectors == 0
    }
}

impl AnvilRegion {
    fn new<P: AsRef<Path>>(path: P) -> Result<Self, io::Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(path)?;

        // If necessary, expand the file length to the length of the header.
        if REGION_HEADER_BYTES_LENGTH > file.metadata()?.len() {
            file.set_len(REGION_HEADER_BYTES_LENGTH)?;
        }

        let chunks_metadata = Self::read_header(&mut file)?;

        let region = AnvilRegion {
            file,
            chunks_metadata,
        };

        Ok(region)
    }

    fn read_header(file: &mut File) -> Result<[AnvilChunkMetadata; REGION_CHUNKS], io::Error> {
        let mut chunks_metadata = [Default::default(); REGION_CHUNKS];
        let mut values = [0u32; REGION_CHUNKS_METADATA_LENGTH];

        for index in 0..REGION_CHUNKS_METADATA_LENGTH {
            values[index] = file.read_u32::<BigEndian>()?;
        }

        for index in 0..REGION_CHUNKS {
            let last_modified_timestamp = values[REGION_CHUNKS + index];
            let offset = values[index];

            let sector_index = offset >> 8;
            let sectors = (offset & 0xFF) as u8;

            let metadata = AnvilChunkMetadata {
                sector_index,
                sectors,
                last_modified_timestamp,
            };

            chunks_metadata[index] = metadata;
        }

        return Ok(chunks_metadata);
    }

    fn read_chunk(&mut self, chunk_x: u8, chunk_z: u8) -> Result<CompoundTag, ChunkLoadError> {
        let metadata = self.get_metadata(chunk_x, chunk_z);

        if metadata.is_empty() {
            return Err(ChunkLoadError::ChunkNotFound { chunk_x, chunk_z });
        }

        let seek_offset = metadata.sector_index as u64 * REGION_SECTOR_BYTES_LENGTH as u64;
        let maximum_length = metadata.sectors as u32 * REGION_SECTOR_BYTES_LENGTH as u32;

        self.file.seek(SeekFrom::Start(seek_offset))?;
        let length = self.file.read_u32::<BigEndian>()?;

        if length > maximum_length {
            return Err(ChunkLoadError::LengthExceedsMaximum {
                length,
                maximum_length,
            });
        }

        let compression_scheme = self.file.read_u8()?;
        let mut compressed_buffer = vec![0u8; (length - 1) as usize];
        self.file.read_exact(&mut compressed_buffer)?;

        let mut cursor = Cursor::new(&compressed_buffer);

        match compression_scheme {
            1 => Ok(read_gzip_compound_tag(&mut cursor)?),
            2 => Ok(read_zlib_compound_tag(&mut cursor)?),
            _ => Err(ChunkLoadError::UnsupportedCompressionScheme { compression_scheme }),
        }
    }

    fn get_metadata(&self, x: u8, z: u8) -> AnvilChunkMetadata {
        assert!(32 > x, "Region chunk x coordinate out of bounds");
        assert!(32 > z, "Region chunk y coordinate out of bounds");

        self.chunks_metadata[(x + z) as usize * 32]
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AnvilChunkMetadata, AnvilChunkProvider, AnvilRegion, ChunkLoadError,
        REGION_HEADER_BYTES_LENGTH,
    };
    use std::io::Read;
    use std::path::Path;
    use tempfile::NamedTempFile;

    #[test]
    fn test_empty_header_write() {
        let mut file = NamedTempFile::new().unwrap();
        let region = AnvilRegion::new(file.path()).unwrap();
        let file_length = region.file.metadata().unwrap().len();

        assert_eq!(file_length, REGION_HEADER_BYTES_LENGTH);
    }

    #[test]
    fn test_empty_region_init() {
        let mut file = NamedTempFile::new().unwrap();
        let region = AnvilRegion::new(file.path()).unwrap();

        let mut vec = Vec::new();
        file.read_to_end(&mut vec).unwrap();

        assert_eq!(vec, include_bytes!("../test/empty_region.mca").to_vec());
    }

    #[test]
    fn test_header_read() {
        let expected_data = vec![
            AnvilChunkMetadata::new(692, 1, 1570215596),
            AnvilChunkMetadata::new(772, 1, 1570215597),
            AnvilChunkMetadata::new(875, 1, 1570215597),
            AnvilChunkMetadata::new(991, 1, 1570215597),
            AnvilChunkMetadata::new(696, 1, 1570215596),
            AnvilChunkMetadata::new(795, 1, 1570215597),
            AnvilChunkMetadata::new(281, 1, 1570215597),
            AnvilChunkMetadata::new(1018, 1, 1570215597),
            AnvilChunkMetadata::new(735, 1, 1570215596),
            AnvilChunkMetadata::new(812, 1, 1570215597),
        ];

        let path = Path::new("test/region.mca");
        assert!(path.exists());

        let region = AnvilRegion::new(path).unwrap();

        for (index, expected_chunk_metadata) in expected_data.iter().enumerate() {
            let chunk_metadata = region.chunks_metadata[256 + index];

            assert_eq!(&chunk_metadata, expected_chunk_metadata);
        }
    }

    #[test]
    fn test_read_chunk_data() {
        let path = Path::new("test/region.mca");
        assert!(path.exists());

        let mut region = AnvilRegion::new(path).unwrap();
        let compound_tag = region.read_chunk(4, 4).unwrap();
        let level_tag = compound_tag.get_compound_tag("Level").unwrap();

        assert_eq!(level_tag.get_i32("xPos").unwrap(), 0);
        assert_eq!(level_tag.get_i32("zPos").unwrap(), -24);
    }

    #[test]
    fn test_read_chunk_empty() {
        let path = Path::new("test/empty_region.mca");
        assert!(path.exists());

        let mut region = AnvilRegion::new(path).unwrap();
        let load_error = region.read_chunk(0, 0).err().unwrap();

        match load_error {
            ChunkLoadError::ChunkNotFound { chunk_x, chunk_z } => {
                assert_eq!(chunk_x, 0);
                assert_eq!(chunk_z, 0);
            }
            _ => panic!("Expected `ChunkNotFound` but got `{:?}`", load_error),
        }
    }

    #[test]
    fn test_load_chunk_no_folder() {
        let chunk_provider = AnvilChunkProvider::new("no-folder");
        let load_error = chunk_provider.load_chunk(4, 4).err().unwrap();

        match load_error {
            ChunkLoadError::RegionNotFound { region_x, region_z } => {
                assert_eq!(region_x, 0);
                assert_eq!(region_z, 0);
            }
            _ => panic!("Expected `RegionNotFound` but got `{:?}", load_error),
        }
    }

    #[test]
    fn test_load_chunk_no_region() {
        let chunk_provider = AnvilChunkProvider::new("test/region");
        let load_error = chunk_provider.load_chunk(100, 100).err().unwrap();

        match load_error {
            ChunkLoadError::RegionNotFound { region_x, region_z } => {
                assert_eq!(region_x, 3);
                assert_eq!(region_z, 3);
            }
            _ => panic!("Expected `RegionNotFound` but got `{:?}", load_error),
        }
    }

    #[test]
    fn test_load_chunk_chunk_not_found() {
        let chunk_provider = AnvilChunkProvider::new("test/region");
        let load_error = chunk_provider.load_chunk(22, 0).err().unwrap();

        match load_error {
            ChunkLoadError::ChunkNotFound { chunk_x, chunk_z } => {
                assert_eq!(chunk_x, 22);
                assert_eq!(chunk_z, 0);
            }
            _ => panic!("Expected `ChunkNotFound` but got `{:?}", load_error),
        }
    }

}
