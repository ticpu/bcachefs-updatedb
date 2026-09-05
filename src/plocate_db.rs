//! Writing and reading plocate databases, byte-compatible with plocate-build.

use crate::turbopfor::PostingListBuilder;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zstd::bulk::Compressor;
use zstd::dict::EncoderDictionary;

pub const MAGIC: [u8; 8] = *b"\0plocate";
pub const HEADER_SIZE: usize = 112;
pub const TRIGRAM_SIZE: usize = 16;
pub const NUM_TRIGRAMS: usize = 16777216;
pub const ZSTD_LEVEL: i32 = 6;

const NUM_OVERFLOW_SLOTS: u32 = 16;
const BLOCKS_TO_KEEP: u64 = 1000;
const DICTIONARY_SIZE: usize = 1024;
const MAX_DICTIONARY_SIZE: u64 = 1048576;

#[repr(C)]
#[derive(Clone, Default)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: u32,
    pub hashtable_size: u32,
    pub extra_ht_slots: u32,
    pub num_docids: u32,
    pub hash_table_offset_bytes: u64,
    pub filename_index_offset_bytes: u64,
    pub max_version: u32,
    pub zstd_dictionary_length_bytes: u32,
    pub zstd_dictionary_offset_bytes: u64,
    pub directory_data_length_bytes: u64,
    pub directory_data_offset_bytes: u64,
    pub next_zstd_dictionary_length_bytes: u64,
    pub next_zstd_dictionary_offset_bytes: u64,
    pub conf_block_length_bytes: u64,
    pub conf_block_offset_bytes: u64,
    pub check_visibility: bool,
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(
        buf[off..off + 4]
            .try_into()
            .unwrap(), // slice of four, checked by the caller's length test
    )
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(
        buf[off..off + 8]
            .try_into()
            .unwrap(), // slice of eight, checked by the caller's length test
    )
}

impl Header {
    fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..8].copy_from_slice(&self.magic);
        buf[8..12].copy_from_slice(
            &self
                .version
                .to_le_bytes(),
        );
        buf[12..16].copy_from_slice(
            &self
                .hashtable_size
                .to_le_bytes(),
        );
        buf[16..20].copy_from_slice(
            &self
                .extra_ht_slots
                .to_le_bytes(),
        );
        buf[20..24].copy_from_slice(
            &self
                .num_docids
                .to_le_bytes(),
        );
        buf[24..32].copy_from_slice(
            &self
                .hash_table_offset_bytes
                .to_le_bytes(),
        );
        buf[32..40].copy_from_slice(
            &self
                .filename_index_offset_bytes
                .to_le_bytes(),
        );
        buf[40..44].copy_from_slice(
            &self
                .max_version
                .to_le_bytes(),
        );
        buf[44..48].copy_from_slice(
            &self
                .zstd_dictionary_length_bytes
                .to_le_bytes(),
        );
        buf[48..56].copy_from_slice(
            &self
                .zstd_dictionary_offset_bytes
                .to_le_bytes(),
        );
        buf[56..64].copy_from_slice(
            &self
                .directory_data_length_bytes
                .to_le_bytes(),
        );
        buf[64..72].copy_from_slice(
            &self
                .directory_data_offset_bytes
                .to_le_bytes(),
        );
        buf[72..80].copy_from_slice(
            &self
                .next_zstd_dictionary_length_bytes
                .to_le_bytes(),
        );
        buf[80..88].copy_from_slice(
            &self
                .next_zstd_dictionary_offset_bytes
                .to_le_bytes(),
        );
        buf[88..96].copy_from_slice(
            &self
                .conf_block_length_bytes
                .to_le_bytes(),
        );
        buf[96..104].copy_from_slice(
            &self
                .conf_block_offset_bytes
                .to_le_bytes(),
        );
        buf[104] = u8::from(self.check_visibility);
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> io::Result<Header> {
        if buf.len() < HEADER_SIZE {
            return Err(io::Error::other("short read on the database header"));
        }
        if buf[0..8] != MAGIC {
            return Err(io::Error::other("not a plocate database: bad magic"));
        }
        Ok(Header {
            magic: MAGIC,
            version: get_u32(buf, 8),
            hashtable_size: get_u32(buf, 12),
            extra_ht_slots: get_u32(buf, 16),
            num_docids: get_u32(buf, 20),
            hash_table_offset_bytes: get_u64(buf, 24),
            filename_index_offset_bytes: get_u64(buf, 32),
            max_version: get_u32(buf, 40),
            zstd_dictionary_length_bytes: get_u32(buf, 44),
            zstd_dictionary_offset_bytes: get_u64(buf, 48),
            directory_data_length_bytes: get_u64(buf, 56),
            directory_data_offset_bytes: get_u64(buf, 64),
            next_zstd_dictionary_length_bytes: get_u64(buf, 72),
            next_zstd_dictionary_offset_bytes: get_u64(buf, 80),
            conf_block_length_bytes: get_u64(buf, 88),
            conf_block_offset_bytes: get_u64(buf, 96),
            check_visibility: buf[104] != 0,
        })
    }

    pub fn read(db: &Path) -> io::Result<Header> {
        let mut buf = [0u8; HEADER_SIZE];
        let mut file = fs::File::open(db)?;
        file.read_exact(&mut buf)?;
        Header::from_bytes(&buf)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Trigram {
    pub trgm: u32,
    pub num_docids: u32,
    pub offset: u64,
}

impl Trigram {
    fn to_bytes(self) -> [u8; TRIGRAM_SIZE] {
        let mut buf = [0u8; TRIGRAM_SIZE];
        buf[0..4].copy_from_slice(
            &self
                .trgm
                .to_le_bytes(),
        );
        buf[4..8].copy_from_slice(
            &self
                .num_docids
                .to_le_bytes(),
        );
        buf[8..16].copy_from_slice(
            &self
                .offset
                .to_le_bytes(),
        );
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Trigram {
        Trigram {
            trgm: get_u32(buf, 0),
            num_docids: get_u32(buf, 4),
            offset: get_u64(buf, 8),
        }
    }
}

pub fn hash_trigram(trgm: u32, ht_size: u32) -> u32 {
    let mut crc = trgm;
    for _ in 0..32 {
        let bit = crc & 0x8000_0000 != 0;
        crc <<= 1;
        if bit {
            crc ^= 0x1edc_6f41;
        }
    }
    crc % ht_size
}

fn is_prime(x: u32) -> bool {
    if x.is_multiple_of(2) || x.is_multiple_of(3) {
        return false;
    }
    let limit = f64::from(x)
        .sqrt()
        .ceil() as u32;
    let mut factor = 5;
    while factor <= limit {
        if x.is_multiple_of(factor) {
            return false;
        }
        factor += 1;
    }
    true
}

fn next_prime(mut x: u32) -> u32 {
    if x.is_multiple_of(2) {
        x += 1;
    }
    while !is_prime(x) {
        x += 2;
    }
    x
}

fn create_hashtable(
    invindex: &[Option<Box<PostingListBuilder>>],
    all_trigrams: &[u32],
    ht_size: u32,
) -> Option<Vec<Trigram>> {
    let slots = (ht_size + NUM_OVERFLOW_SLOTS + 1) as usize;
    let mut ht = vec![
        Trigram {
            trgm: u32::MAX,
            num_docids: 0,
            offset: 0,
        };
        slots
    ];
    for &trgm in all_trigrams {
        let mut to_insert = Trigram {
            trgm,
            num_docids: invindex[trgm as usize]
                .as_ref()
                .expect("all_trigrams only holds trigrams with a posting list")
                .num_docids(),
            offset: 0,
        };

        let mut bucket = hash_trigram(trgm, ht_size);
        let mut distance = 0u32;
        while ht[bucket as usize].num_docids != 0 {
            let other_distance =
                bucket.wrapping_sub(hash_trigram(ht[bucket as usize].trgm, ht_size));
            if distance > other_distance {
                std::mem::swap(&mut to_insert, &mut ht[bucket as usize]);
                distance = other_distance;
            }
            bucket += 1;
            distance += 1;
            if distance > NUM_OVERFLOW_SLOTS {
                return None;
            }
        }
        ht[bucket as usize] = to_insert;
    }
    Some(ht)
}

/// Reservoir sample of whole blocks, used to train the dictionary the next run
/// picks up.
struct DictionarySampler {
    blocks_to_keep: u64,
    block_num: u64,
    rng: StdRng,
    keep_current_block: bool,
    slot_for_current_block: i64,
    sampled_blocks: Vec<Vec<u8>>,
}

impl DictionarySampler {
    fn new(blocks_to_keep: u64) -> Self {
        DictionarySampler {
            blocks_to_keep,
            block_num: 0,
            rng: StdRng::seed_from_u64(1234),
            keep_current_block: true,
            slot_for_current_block: -1,
            sampled_blocks: Vec::new(),
        }
    }

    fn add_block(&mut self, block: &[u8]) {
        if self.keep_current_block {
            if self.slot_for_current_block == -1 {
                self.sampled_blocks
                    .push(block.to_vec());
            } else {
                self.sampled_blocks[self.slot_for_current_block as usize] = block.to_vec();
            }
        }
        self.block_num += 1;

        if self.block_num < self.blocks_to_keep {
            self.keep_current_block = true;
            self.slot_for_current_block = -1;
        } else {
            let idx = self
                .rng
                .random_range(0..=self.block_num);
            self.keep_current_block = idx < self.blocks_to_keep;
            self.slot_for_current_block = idx as i64;
        }
    }

    fn train(&mut self, size: usize) -> io::Result<Vec<u8>> {
        self.sampled_blocks
            .sort_unstable();
        zstd::dict::from_samples(&self.sampled_blocks, size)
    }
}

struct Out {
    tmp: NamedTempFile,
    buf: Vec<u8>,
    pos: u64,
}

impl Out {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.buf
            .extend_from_slice(data);
        self.pos += data.len() as u64;
        if self
            .buf
            .len()
            >= 1 << 20
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tmp
            .as_file_mut()
            .write_all(&self.buf)?;
        self.buf
            .clear();
        Ok(())
    }

    fn pos(&self) -> u64 {
        self.pos
    }
}

#[derive(Default)]
pub struct BuildStats {
    pub num_files: u64,
    pub num_trigrams: u64,
    pub num_entries: u64,
    pub longest_posting_list: u32,
    pub bytes_for_dictionary: u64,
    pub bytes_for_hashtable: u64,
    pub bytes_for_posting_lists: u64,
    pub bytes_for_filename_index: u64,
    pub bytes_for_filenames: u64,
}

pub struct DatabaseBuilder<'a> {
    out: Out,
    output: PathBuf,
    hdr: Header,
    block_size: usize,
    compressor: Compressor<'a>,
    invindex: Vec<Option<Box<PostingListBuilder>>>,
    filename_blocks: Vec<u64>,
    current_block: Vec<u8>,
    num_files_in_block: usize,
    num_files: u64,
    num_blocks: u64,
    sampler: DictionarySampler,
}

impl<'a> DatabaseBuilder<'a> {
    pub fn new(
        output: &Path,
        gid: Option<libc::gid_t>,
        block_size: usize,
        dictionary: &[u8],
        cdict: Option<&'a EncoderDictionary<'a>>,
        check_visibility: bool,
    ) -> io::Result<Self> {
        let dir = output
            .parent()
            .filter(|p| {
                !p.as_os_str()
                    .is_empty()
            })
            .unwrap_or(Path::new("."));
        let tmp = NamedTempFile::new_in(dir)?;
        let fd = tmp
            .as_file()
            .as_raw_fd();
        if unsafe { libc::fchmod(fd, 0o640) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if let Some(gid) = gid
            && unsafe { libc::fchown(fd, u32::MAX, gid) } == -1
        {
            return Err(io::Error::last_os_error());
        }

        let mut hdr = Header {
            magic: MAGIC,
            version: u32::MAX,
            extra_ht_slots: NUM_OVERFLOW_SLOTS,
            max_version: 2,
            hash_table_offset_bytes: u64::MAX,
            filename_index_offset_bytes: u64::MAX,
            zstd_dictionary_length_bytes: u32::MAX,
            zstd_dictionary_offset_bytes: u64::MAX,
            check_visibility,
            ..Header::default()
        };

        let mut out = Out {
            tmp,
            buf: Vec::with_capacity(2 << 20),
            pos: 0,
        };
        out.write(&hdr.to_bytes())?;

        if dictionary.is_empty() {
            hdr.zstd_dictionary_offset_bytes = 0;
            hdr.zstd_dictionary_length_bytes = 0;
        } else {
            hdr.zstd_dictionary_offset_bytes = out.pos();
            out.write(dictionary)?;
            hdr.zstd_dictionary_length_bytes = dictionary.len() as u32;
        }

        let compressor = match cdict {
            Some(cdict) => Compressor::with_prepared_dictionary(cdict)?,
            None => Compressor::new(ZSTD_LEVEL)?,
        };

        let mut invindex = Vec::new();
        invindex.resize_with(NUM_TRIGRAMS, || None);

        Ok(DatabaseBuilder {
            out,
            output: output.to_path_buf(),
            hdr,
            block_size,
            compressor,
            invindex,
            filename_blocks: Vec::new(),
            current_block: Vec::new(),
            num_files_in_block: 0,
            num_files: 0,
            num_blocks: 0,
            sampler: DictionarySampler::new(BLOCKS_TO_KEEP),
        })
    }

    pub fn add_file(&mut self, filename: &[u8]) -> io::Result<()> {
        self.num_files += 1;
        if !self
            .current_block
            .is_empty()
        {
            self.current_block
                .push(b'\0');
        }
        self.current_block
            .extend_from_slice(filename);
        self.num_files_in_block += 1;
        if self.num_files_in_block == self.block_size {
            self.flush_block()?;
        }
        Ok(())
    }

    fn add_docid(&mut self, trgm: u32, docid: u32) {
        match &mut self.invindex[trgm as usize] {
            Some(pl) => pl.add_docid(docid),
            slot => {
                let mut pl = Box::new(PostingListBuilder::default());
                pl.add_first_docid(docid);
                *slot = Some(pl);
            }
        }
    }

    fn flush_block(&mut self) -> io::Result<()> {
        if self
            .current_block
            .is_empty()
        {
            return Ok(());
        }
        let docid = self.num_blocks as u32;
        let mut block = std::mem::take(&mut self.current_block);
        let len = block.len();
        // The trigram scan reads one byte past each trigram, as the C encoder
        // does over the string's terminator.
        block.push(b'\0');

        let mut ptr = 0usize;
        while ptr + 3 < len {
            if block[ptr] == 0 {
                ptr += 1;
                continue;
            } else if block[ptr + 1] == 0 {
                ptr += 2;
                continue;
            } else if block[ptr + 2] == 0 {
                ptr += 3;
                continue;
            }
            loop {
                let trgm = get_u32(&block, ptr);
                ptr += 1;
                self.add_docid(trgm & 0xffffff, docid);
                if trgm <= 0xffffff {
                    ptr += 3;
                    break;
                }
            }
        }

        block.truncate(len);
        self.sampler
            .add_block(&block);
        self.filename_blocks
            .push(
                self.out
                    .pos(),
            );
        let compressed = self
            .compressor
            .compress(&block)?;
        self.out
            .write(&compressed)?;

        block.clear();
        self.current_block = block;
        self.num_files_in_block = 0;
        self.num_blocks += 1;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<BuildStats> {
        self.flush_block()?;

        let mut stats = BuildStats {
            num_files: self.num_files,
            bytes_for_dictionary: u64::from(
                self.hdr
                    .zstd_dictionary_length_bytes,
            ),
            ..BuildStats::default()
        };

        self.hdr
            .num_docids = self
            .filename_blocks
            .len() as u32;
        self.filename_blocks
            .push(
                self.out
                    .pos(),
            );
        stats.bytes_for_filenames = self
            .filename_blocks
            .last()
            .copied()
            .unwrap_or(0)
            - self
                .filename_blocks
                .first()
                .copied()
                .unwrap_or(0);

        self.hdr
            .filename_index_offset_bytes = self
            .out
            .pos();
        stats.bytes_for_filename_index = (self
            .filename_blocks
            .len()
            * 8) as u64;
        let mut index = Vec::with_capacity(
            self.filename_blocks
                .len()
                * 8,
        );
        for offset in &self.filename_blocks {
            index.extend_from_slice(&offset.to_le_bytes());
        }
        self.out
            .write(&index)?;
        drop(index);
        self.filename_blocks = Vec::new();

        let mut all_trigrams = Vec::new();
        for trgm in 0..NUM_TRIGRAMS {
            if let Some(pl) = &mut self.invindex[trgm] {
                pl.finish();
                stats.longest_posting_list = stats
                    .longest_posting_list
                    .max(pl.num_docids());
                stats.num_entries += u64::from(pl.num_docids());
                stats.bytes_for_posting_lists += pl
                    .encoded
                    .len() as u64;
                all_trigrams.push(trgm as u32);
            }
        }
        stats.num_trigrams = all_trigrams.len() as u64;

        let mut ht_size = next_prime(all_trigrams.len() as u32);
        let hashtable = loop {
            match create_hashtable(&self.invindex, &all_trigrams, ht_size) {
                Some(ht) => break ht,
                None => ht_size = next_prime((f64::from(ht_size) * 1.05) as u32),
            }
        };
        drop(all_trigrams);

        let slots = (ht_size + NUM_OVERFLOW_SLOTS + 1) as usize;
        stats.bytes_for_hashtable = (slots * TRIGRAM_SIZE) as u64;
        let mut hashtable = hashtable;
        let mut offset = self
            .out
            .pos()
            + stats.bytes_for_hashtable;
        for slot in &mut hashtable {
            slot.offset = offset;
            if slot.num_docids == 0 {
                continue;
            }
            offset += self.invindex[slot.trgm as usize]
                .as_ref()
                .expect("a hash table slot only names a trigram with a posting list")
                .encoded
                .len() as u64;
        }

        self.hdr
            .hash_table_offset_bytes = self
            .out
            .pos();
        self.hdr
            .hashtable_size = ht_size;
        let mut table = Vec::with_capacity(slots * TRIGRAM_SIZE);
        for slot in &hashtable {
            table.extend_from_slice(&slot.to_bytes());
        }
        self.out
            .write(&table)?;
        drop(table);

        for slot in &hashtable {
            if slot.num_docids == 0 {
                continue;
            }
            let encoded = std::mem::take(
                &mut self.invindex[slot.trgm as usize]
                    .as_mut()
                    .expect("a hash table slot only names a trigram with a posting list")
                    .encoded,
            );
            self.out
                .write(&encoded)?;
        }
        drop(hashtable);
        self.invindex = Vec::new();

        match self
            .sampler
            .train(DICTIONARY_SIZE)
        {
            Ok(next_dictionary) if !next_dictionary.is_empty() => {
                self.hdr
                    .next_zstd_dictionary_offset_bytes = self
                    .out
                    .pos();
                self.hdr
                    .next_zstd_dictionary_length_bytes = next_dictionary.len() as u64;
                self.out
                    .write(&next_dictionary)?;
            }
            Ok(_) => eprintln!("trained an empty zstd dictionary, storing none"),
            Err(e) => eprintln!("training the next zstd dictionary failed: {e}"),
        }

        self.hdr
            .version = 1;
        self.out
            .flush()?;
        let file = self
            .out
            .tmp
            .as_file_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(
            &self
                .hdr
                .to_bytes(),
        )?;
        file.flush()?;
        self.out
            .tmp
            .persist(&self.output)
            .map_err(|e| io::Error::other(format!("renaming the database into place: {e}")))?;

        Ok(stats)
    }
}

/// The next dictionary a previous run of this tool (or of updatedb) left in the
/// database being replaced.
pub fn read_next_dictionary(db: &Path) -> io::Result<Vec<u8>> {
    let hdr = match Header::read(db) {
        Ok(hdr) => hdr,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = hdr.next_zstd_dictionary_length_bytes;
    if len == 0 || len > MAX_DICTIONARY_SIZE {
        return Ok(Vec::new());
    }
    let mut file = fs::File::open(db)?;
    file.seek(SeekFrom::Start(hdr.next_zstd_dictionary_offset_bytes))?;
    let mut dictionary = vec![0u8; len as usize];
    file.read_exact(&mut dictionary)?;
    Ok(dictionary)
}

fn group_name(gid: libc::gid_t) -> Option<String> {
    let gr = unsafe { libc::getgrgid(gid) };
    if gr.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr((*gr).gr_name) };
    Some(
        name.to_string_lossy()
            .into_owned(),
    )
}

fn setgid_group_of_plocate() -> Option<(libc::gid_t, PathBuf)> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("plocate");
        let name = match CString::new(
            candidate
                .as_os_str()
                .as_bytes(),
        ) {
            Ok(name) => name,
            Err(_) => continue,
        };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::stat(name.as_ptr(), st.as_mut_ptr()) } != 0 {
            continue;
        }
        let st = unsafe { st.assume_init() };
        if st.st_mode & libc::S_ISGID != 0 {
            return Some((st.st_gid, candidate));
        }
    }
    None
}

/// The database is only readable through plocate's setgid group, so the group
/// comes from the installed binary unless the caller names one.
pub fn resolve_group(group: Option<&str>) -> io::Result<libc::gid_t> {
    if let Some(group) = group {
        let name = CString::new(group).map_err(|e| io::Error::other(format!("group name: {e}")))?;
        let gr = unsafe { libc::getgrnam(name.as_ptr()) };
        if gr.is_null() {
            return Err(io::Error::other(format!("unknown group {group}")));
        }
        let gid = unsafe { (*gr).gr_gid };
        eprintln!("database group {group} ({gid})");
        return Ok(gid);
    }

    match setgid_group_of_plocate() {
        Some((gid, path)) => {
            let name = group_name(gid).unwrap_or_else(|| gid.to_string());
            eprintln!("database group {name} ({gid}), from {}", path.display());
            Ok(gid)
        }
        None => Err(io::Error::other(
            "no setgid plocate binary on PATH to take the database group from; \
             pass --group NAME, since a root-owned database is unreadable to users",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_the_c_layout() {
        assert_eq!(std::mem::size_of::<Header>(), HEADER_SIZE);
        assert_eq!(std::mem::size_of::<Trigram>(), TRIGRAM_SIZE);
    }

    #[test]
    fn next_prime_skips_composites() {
        assert_eq!(next_prime(0), 1);
        assert_eq!(next_prime(2), 5);
        assert_eq!(next_prime(8), 11);
        assert_eq!(next_prime(1000), 1009);
    }

    #[test]
    fn hash_trigram_is_the_crc_of_db_h() {
        assert_eq!(hash_trigram(0, 1000), 0);
        assert_eq!(hash_trigram(1, u32::MAX), 0x1edc6f41);
    }
}
