use std::io::{Cursor, Seek, SeekFrom, Write};

use byteorder::{LittleEndian, WriteBytesExt};
use rayon::prelude::*;

use crate::laszip::chunk_table::{update_chunk_table_offset, ChunkTable, ChunkTableEntry};
use crate::laszip::details::record_compressor_from_laz_items;
use crate::LazVlr;

/// LasZip compressor that compresses using multiple threads
///
/// This supports both **variable-size** and **fixed-size** chunks.
/// The method you need to call in order to compress data depends on which
/// type of *sized* chunks you want to write.
///
/// Its the [`LazVlr`] that controls which type of chunks you want to write.
///
/// You must call [`done`] when you have compressed all the points you wanted.
///
/// # Fixed-Size
///
/// Use [`compress_many`]
///
/// This works by forming complete chunks of points with the points
/// data passed when [`compress_many`] is called. These complete chunks are
/// compressed & written right away and points that are 'leftovers' are kept until
/// the next call to [`compress_many`] or [`done`].
///
/// # Variable-Size
///
/// Use [`compress_chunks`]
///
///
/// [`compress_many`]: Self::compress_many
/// [`compress_chunks`]: Self::compress_chunks
/// [`done`]: Self::done
#[cfg(feature = "parallel")]
pub struct ParLasZipCompressor<W> {
    vlr: LazVlr,
    /// Table of chunks written so far
    chunk_table: ChunkTable,
    /// offset from beginning of the file to where the
    /// offset to chunk table will be written
    table_offset: i64,
    // Stores uncompressed points from the last call to compress_many
    // that did not allow to make a full chunk of the requested vlr.chunk_size
    // They are prepended to the points data passed to the compress_many fn.
    // The rest is compressed when done is called, forming the last chunk
    rest: Vec<u8>,
    dest: W,
    compressor: ParChunkCompressor,
}

#[cfg(feature = "parallel")]
impl<W: Write + Seek + Send> ParLasZipCompressor<W> {
    /// Creates a new ParLasZipCompressor
    pub fn new(dest: W, vlr: LazVlr) -> crate::Result<Self> {
        let mut rest = Vec::<u8>::new();
        if !vlr.uses_variable_size_chunks() {
            rest.reserve(vlr.num_bytes_in_decompressed_chunk() as usize);
        }
        Ok(Self {
            vlr,
            chunk_table: ChunkTable::default(),
            table_offset: -1,
            rest,
            dest,
            compressor: ParChunkCompressor::default(),
        })
    }

    /// Reserves and prepares the offset to chunk table that will be
    /// updated when [done] is called.
    ///
    /// This method will automatically be called on the first point(s) being compressed,
    /// but for some scenarios, manually calling this might be useful.
    ///
    /// [done]: Self::done
    pub fn reserve_offset_to_chunk_table(&mut self) -> std::io::Result<()> {
        self.table_offset = self.dest.seek(SeekFrom::Current(0))? as i64;
        self.dest.write_i64::<LittleEndian>(self.table_offset)
    }

    /// Compresses many points using multiple threads.
    ///
    /// # Important
    ///
    /// This **must** be called **only** when writing **fixed-size** chunks.
    /// This will **panic** otherwise.
    ///
    /// # Note
    ///
    /// For this function to actually use multiple threads, the `points`
    /// buffer shall hold more points that the vlr's `chunk_size`.
    pub fn compress_many(&mut self, points: &[u8]) -> std::io::Result<()> {
        assert!(!self.vlr.uses_variable_size_chunks());
        if self.table_offset == -1 {
            self.reserve_offset_to_chunk_table()?;
        }
        let point_size = self.vlr.items_size() as usize;
        debug_assert_eq!(self.rest.len() % point_size, 0);

        let chunk_size_in_bytes = self.vlr.chunk_size() as usize * point_size;
        debug_assert!(self.rest.len() < chunk_size_in_bytes);
        let mut compressible_buf = points;

        if self.rest.len() != 0 {
            // Try to complete our rest buffer to form a complete chunk
            let missing_bytes = chunk_size_in_bytes - self.rest.len();
            let num_bytes_to_copy = missing_bytes.min(compressible_buf.len());
            self.rest
                .extend_from_slice(&compressible_buf[..num_bytes_to_copy]);

            if self.rest.len() < chunk_size_in_bytes {
                // rest + points did not form a complete chunk,
                // no need to go further.
                return Ok(());
            }

            debug_assert_eq!(self.rest.len(), chunk_size_in_bytes);
            // We have a complete chunk, lets compress it now
            let chunk_size = compress_one_chunk(&self.rest, &self.vlr, &mut self.dest)?;
            self.chunk_table.push(ChunkTableEntry {
                point_count: self.vlr.chunk_size() as u64,
                byte_count: chunk_size,
            });
            self.rest.clear();

            compressible_buf = &compressible_buf[missing_bytes..]
        }
        debug_assert_eq!(compressible_buf.len() % point_size, 0);

        // Copy bytes which does not form a complete chunk into our rest.
        let num_excess_bytes = compressible_buf.len() % chunk_size_in_bytes;
        let (compressible_buf, excess_bytes) =
            compressible_buf.split_at(compressible_buf.len() - num_excess_bytes);
        debug_assert_eq!(excess_bytes.len(), num_excess_bytes);
        if !excess_bytes.is_empty() {
            self.rest.extend_from_slice(excess_bytes);
        }

        if !compressible_buf.is_empty() {
            let chunk_table = self
                .compressor
                .compress_points(&mut self.dest, compressible_buf, &self.vlr)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            // let chunk_table = par_compress(&mut self.dest, compressible_buf, &self.vlr)
            //     .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            self.chunk_table.extend(&chunk_table);
        }

        Ok(())
    }

    /// Compresses multiple chunks using multiple threads.
    ///
    /// # Important
    ///
    /// This **must** be called **only** when writing **variable-size** chunks.
    /// This will **panic** otherwise.
    ///
    /// # Note
    ///
    /// For this function to actually use multiple threads, their should be more that one chunk.
    /// buffer shall hold more points that the vlr's `chunk_size`.
    pub fn compress_chunks<Chunks, Item, Iter>(&mut self, chunks: Chunks) -> std::io::Result<()>
    where
        Item: AsRef<[u8]> + Send,
        Iter: IndexedParallelIterator<Item = Item>,
        Chunks: IntoParallelIterator<Item = Item, Iter = Iter> + Copy,
    {
        assert!(self.vlr.uses_variable_size_chunks());
        debug_assert!(self.rest.is_empty());
        if self.table_offset == -1 {
            self.reserve_offset_to_chunk_table()?;
        }

        let chunk_table = self
            .compressor
            .compress_chunks(&mut self.dest, chunks, &self.vlr)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        // let chunk_table = par_compress_chunks(&mut self.dest, chunks, &self.vlr)
        //     .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        self.chunk_table.extend(&chunk_table);
        Ok(())
    }

    /// Tells the compressor that no more points will be compressed
    ///
    /// - Compresses & writes the rest of the points to form the last chunk
    /// - Writes the chunk table
    /// - update the offset to the chunk_table
    pub fn done(&mut self) -> crate::Result<()> {
        if self.rest.len() != 0 {
            debug_assert!(self.rest.len() <= self.vlr.num_bytes_in_decompressed_chunk() as usize);
            let last_chunk_size = compress_one_chunk(&self.rest, &self.vlr, &mut self.dest)?;
            self.chunk_table.push(ChunkTableEntry {
                point_count: self.vlr.chunk_size() as u64,
                byte_count: last_chunk_size,
            });
        }

        if self.table_offset == -1 && self.chunk_table.is_empty() {
            // No call to compress_many was made
            self.reserve_offset_to_chunk_table()?;
        }
        update_chunk_table_offset(&mut self.dest, SeekFrom::Start(self.table_offset as u64))?;
        self.chunk_table.write_to(&mut self.dest, &self.vlr)?;
        // write_chunk_table(&mut self.dest, &self.chunk_table)?;
        Ok(())
    }

    pub fn vlr(&self) -> &LazVlr {
        &self.vlr
    }

    pub fn into_inner(self) -> W {
        self.dest
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.dest
    }

    pub fn get(&self) -> &W {
        &self.dest
    }
}

impl<W: Write + Seek + Send> crate::LazCompressor for ParLasZipCompressor<W> {
    fn compress_many(&mut self, points: &[u8]) -> crate::Result<()> {
        self.compress_many(points)?;
        Ok(())
    }

    fn done(&mut self) -> crate::Result<()> {
        self.done()?;
        Ok(())
    }
}

/// Compresses all points in parallel
///
/// Just like [`compress_buffer`] but the compression is done in multiple threads
///
/// # Note
///
/// Point order [is conserved](https://github.com/rayon-rs/rayon/issues/551)
///
/// [`compress_buffer`]: crate::compress_buffer
#[cfg(feature = "parallel")]
pub fn par_compress_buffer<W: Write + Seek>(
    dst: &mut W,
    uncompressed_points: &[u8],
    laz_vlr: &LazVlr,
) -> crate::Result<()> {
    let start_pos = dst.seek(SeekFrom::Current(0))?;
    // Reserve the bytes for the chunk table offset that will be updated later
    dst.write_i64::<LittleEndian>(start_pos as i64)?;

    let chunk_table = par_compress(dst, uncompressed_points, laz_vlr)?;

    update_chunk_table_offset(dst, SeekFrom::Start(start_pos))?;
    chunk_table.write_to(dst, laz_vlr)?;
    Ok(())
}

/// Compresses the points contained in `uncompressed_points` writing the result in the `dst`
/// and returns the size of each chunk
///
/// Does not write nor update the offset to the chunk table
/// And does not write the chunk table
///
/// Returns the size of each compressed chunk of point written
#[cfg(feature = "parallel")]
pub fn par_compress<W: Write>(
    dst: &mut W,
    uncompressed_points: &[u8],
    laz_vlr: &LazVlr,
) -> crate::Result<ChunkTable> {
    debug_assert!(!laz_vlr.uses_variable_size_chunks());
    debug_assert_eq!(uncompressed_points.len() % laz_vlr.items_size() as usize, 0);

    let point_size = laz_vlr.items_size() as usize;
    let points_per_chunk = laz_vlr.chunk_size() as usize;
    let chunk_size_in_bytes = points_per_chunk * point_size;

    let all_slices = uncompressed_points.par_chunks(chunk_size_in_bytes);
    par_compress_chunks(dst, all_slices, laz_vlr)
}

#[derive(Copy, Clone)]
struct RawPoints<'a> {
    slc: &'a [u8],
    chunk_size_in_bytes: usize,
}

impl<'a> IntoParallelIterator for RawPoints<'a> {
    type Iter = rayon::slice::Chunks<'a, u8>;
    type Item = <rayon::slice::Chunks<'a, u8> as ParallelIterator>::Item;

    fn into_par_iter(self) -> Self::Iter {
        self.slc.par_chunks(self.chunk_size_in_bytes)
    }
}

#[derive(Default)]
struct Lol {
    input_size: usize,
    buffer: Vec<u8>,
}

#[derive(Default)]
struct ParChunkCompressor {
    chunk_buffers: Vec<Lol>,
}

impl ParChunkCompressor {
    fn compress_points<W: Write>(
        &mut self,
        dst: &mut W,
        uncompressed_points: &[u8],
        laz_vlr: &LazVlr,
    ) -> crate::Result<ChunkTable> {
        debug_assert!(!laz_vlr.uses_variable_size_chunks());
        debug_assert_eq!(uncompressed_points.len() % laz_vlr.items_size() as usize, 0);

        let point_size = laz_vlr.items_size() as usize;
        let points_per_chunk = laz_vlr.chunk_size() as usize;
        let chunk_size_in_bytes = points_per_chunk * point_size;

        let r = RawPoints {
            slc: uncompressed_points,
            chunk_size_in_bytes,
        };
        self.compress_chunks(dst, r, laz_vlr)
    }

    fn compress_chunks<W, Chunks, Item, Iter>(
        &mut self,
        dst: &mut W,
        chunks: Chunks,
        laz_vlr: &LazVlr,
    ) -> crate::Result<ChunkTable>
    where
        W: Write,
        Item: AsRef<[u8]> + Send,
        Iter: IndexedParallelIterator<Item = Item>,
        Chunks: IntoParallelIterator<Item = Item, Iter = Iter> + Copy,
    {
        let num_chunks = chunks.into_par_iter().count();
        if num_chunks > self.chunk_buffers.len() {
            self.chunk_buffers.resize_with(num_chunks, Lol::default);
        }

        let chunk_buffers = &mut self.chunk_buffers[..num_chunks];

        chunk_buffers
            .into_par_iter()
            .zip(chunks.into_par_iter())
            .map(|(lol, data)| {
                let slc = data.as_ref();
                lol.buffer.clear();
                let mut output = Cursor::new(&mut lol.buffer);
                compress_one_chunk(slc, &laz_vlr, &mut output)?;
                lol.input_size = slc.len();
                Ok(())
            })
            .collect::<crate::Result<()>>()?;

        let point_size = laz_vlr.items_size() as usize;
        let mut chunk_table = ChunkTable::with_capacity(chunk_buffers.len());
        for lol in chunk_buffers {
            let input_size = lol.input_size;
            let compressed_data = &lol.buffer;
            let point_count = if laz_vlr.uses_variable_size_chunks() {
                (input_size / point_size) as u64
            } else {
                laz_vlr.chunk_size() as u64
            };
            let entry = ChunkTableEntry {
                point_count,
                byte_count: compressed_data.len() as u64,
            };
            chunk_table.push(entry);
            dst.write_all(&compressed_data)?;
        }
        Ok(chunk_table)
    }
}

fn par_compress_chunks<W, Chunks, Item>(
    dst: &mut W,
    chunks: Chunks,
    laz_vlr: &LazVlr,
) -> crate::Result<ChunkTable>
where
    W: Write,
    Item: AsRef<[u8]> + Send,
    Chunks: IntoParallelIterator<Item = Item>,
{
    use std::io::Cursor;

    let chunks = chunks
        .into_par_iter()
        .map(|data| {
            let slc = data.as_ref();
            let mut output = Cursor::new(Vec::<u8>::new());
            compress_one_chunk(slc, laz_vlr, &mut output)?;
            let vec = output.into_inner();
            Ok((slc.len(), vec))
        })
        .collect::<Vec<crate::Result<(usize, Vec<u8>)>>>();

    let mut chunk_table = ChunkTable::with_capacity(chunks.len());
    let point_size = laz_vlr.items_size() as usize;
    for chunk_result in chunks {
        let (input_size, compressed_data) = chunk_result?;
        let point_count = if laz_vlr.uses_variable_size_chunks() {
            (input_size / point_size) as u64
        } else {
            laz_vlr.chunk_size() as u64
        };
        let entry = ChunkTableEntry {
            point_count,
            byte_count: compressed_data.len() as u64,
        };
        chunk_table.push(entry);
        dst.write_all(&compressed_data)?;
    }
    Ok(chunk_table)
}

fn compress_one_chunk<W: Write + Seek + Send>(
    chunk_data: &[u8],
    vlr: &LazVlr,
    mut dest: &mut W,
) -> std::io::Result<u64> {
    let start = dest.seek(SeekFrom::Current(0))?;
    {
        let mut compressor = record_compressor_from_laz_items(&vlr.items(), &mut dest).unwrap();
        compressor.compress_many(chunk_data)?;
        compressor.done()?;
    }
    let end = dest.seek(SeekFrom::Current(0))?;
    Ok(end - start)
}

#[cfg(test)]
mod test {
    use crate::{LazItemRecordBuilder, LazItemType};

    use super::*;

    #[cfg(feature = "parallel")]
    #[test]
    fn test_table_offset_one_point() {
        // Test that if we compress just one point using the Parallel compressor
        // the chunk table offset is correctly reserved
        let vlr = super::LazVlr::from_laz_items(
            LazItemRecordBuilder::new()
                .add_item(LazItemType::Point10)
                .build(),
        );

        let point = vec![0u8; vlr.items_size() as usize];
        let mut compressor =
            ParLasZipCompressor::new(std::io::Cursor::new(Vec::<u8>::new()), vlr).unwrap();
        assert_eq!(compressor.table_offset, -1);
        compressor.compress_many(&point).unwrap();
        assert_eq!(compressor.table_offset, 0);
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn test_table_offset_complete_chunk() {
        // Test that if we compress at least a chunk using the Parallel compressor
        // the chunk table offset is correctly reserved
        let vlr = super::LazVlr::from_laz_items(
            LazItemRecordBuilder::new()
                .add_item(LazItemType::Point10)
                .build(),
        );

        let points = vec![0u8; vlr.num_bytes_in_decompressed_chunk() as usize];
        let mut compressor =
            ParLasZipCompressor::new(std::io::Cursor::new(Vec::<u8>::new()), vlr).unwrap();
        assert_eq!(compressor.table_offset, -1);
        compressor.compress_many(&points).unwrap();
        assert_eq!(compressor.table_offset, 0);
    }
}
