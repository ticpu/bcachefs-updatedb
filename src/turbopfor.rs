//! TurboPFor posting-list encoding, byte-compatible with plocate's
//! turbopfor-encode.h.

const BLOCK_SIZE: usize = 128;

/// P4NENC_BOUND(128), plus room for the writers that run past the end.
const SCRATCH_LEN: usize = BLOCK_SIZE.div_ceil(128) + (BLOCK_SIZE + 32) * 4 + 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockType {
    For = 0,
    PforVb = 1,
    PforBitmap = 2,
    Constant = 3,
}

fn div_round_up(val: u32, div: u32) -> u32 {
    val.div_ceil(div)
}

fn bytes_for_packed_bits(num: u32, bit_width: u32) -> usize {
    div_round_up(num * bit_width, 8) as usize
}

fn mask_for_bits(bit_width: u32) -> u32 {
    if bit_width == 32 {
        0xffff_ffff
    } else {
        (1u32 << bit_width) - 1
    }
}

fn num_bits(x: u32) -> u32 {
    if x == 0 { 0 } else { 32 - x.leading_zeros() }
}

fn write_le32(out: &mut [u8], pos: usize, val: u32) {
    out[pos..pos + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_le16(out: &mut [u8], pos: usize, val: u16) {
    out[pos..pos + 2].copy_from_slice(&val.to_le_bytes());
}

/// Corresponds to read_baseval.
fn write_baseval(val: u32, out: &mut Vec<u8>) {
    if val < 128 {
        out.push(val as u8);
    } else if val < 0x4000 {
        out.push(((val >> 8) | 0x80) as u8);
        out.push((val & 0xff) as u8);
    } else if val < 0x200000 {
        out.push(((val >> 16) | 0xc0) as u8);
        out.push((val & 0xff) as u8);
        out.push(((val >> 8) & 0xff) as u8);
    } else if val < 0x10000000 {
        out.push(((val >> 24) | 0xe0) as u8);
        out.push(((val >> 16) & 0xff) as u8);
        out.push(((val >> 8) & 0xff) as u8);
        out.push((val & 0xff) as u8);
    } else {
        panic!("docid {val} does not fit the plocate base value encoding");
    }
}

/// Writes a varbyte-encoded exception.
fn write_vb(val: u32, out: &mut [u8], pos: usize) -> usize {
    if val <= 176 {
        out[pos] = val as u8;
        pos + 1
    } else if val <= 16560 {
        let val = val - 177;
        out[pos] = ((val >> 8) + 177) as u8;
        out[pos + 1] = (val & 0xff) as u8;
        pos + 2
    } else if val <= 540848 {
        let val = val - 16561;
        out[pos] = ((val >> 16) + 241) as u8;
        write_le16(out, pos + 1, (val & 0xffff) as u16);
        pos + 3
    } else if val <= 16777215 {
        out[pos] = 249;
        write_le32(out, pos + 1, val);
        pos + 4
    } else {
        out[pos] = 250;
        write_le32(out, pos + 1, val);
        pos + 5
    }
}

struct BitWriter {
    pos: usize,
    bits: u32,
    bits_used: u32,
    cur_val: u32,
}

impl BitWriter {
    fn new(pos: usize, bits: u32) -> Self {
        BitWriter {
            pos,
            bits,
            bits_used: 0,
            cur_val: 0,
        }
    }

    fn write(&mut self, out: &mut [u8], val: u32) {
        self.cur_val |= val.wrapping_shl(self.bits_used);
        write_le32(out, self.pos, self.cur_val);

        self.bits_used += self.bits;
        // The C accumulator is a 32-bit register, where a shift of 32 is a
        // no-op; the emitted bytes differ if this one shifts the value out.
        self.cur_val >>= (self.bits_used / 8 * 8) & 31;
        self.pos += (self.bits_used / 8) as usize;
        self.bits_used %= 8;
    }
}

struct InterleavedBitWriter<const NUM_STREAMS: usize> {
    pos: usize,
    bits: u32,
    bits_used: u32,
    cur_val: u64,
}

impl<const NUM_STREAMS: usize> InterleavedBitWriter<NUM_STREAMS> {
    const STRIDE: usize = NUM_STREAMS * 4;

    fn new(pos: usize, bits: u32) -> Self {
        InterleavedBitWriter {
            pos,
            bits,
            bits_used: 0,
            cur_val: 0,
        }
    }

    fn write(&mut self, out: &mut [u8], val: u32) {
        self.cur_val |= u64::from(val) << self.bits_used;
        if self.bits_used + self.bits >= 32 {
            write_le32(out, self.pos, self.cur_val as u32);
            self.pos += Self::STRIDE;
            self.cur_val >>= 32;
            self.bits_used = self
                .bits_used
                .wrapping_sub(32);
        }
        write_le32(out, self.pos, self.cur_val as u32);
        self.bits_used = self
            .bits_used
            .wrapping_add(self.bits);
    }
}

/// Bitpacks a set of values, interleaved over four streams for full blocks.
fn encode_bitmap(
    input: &[u32],
    bit_width: u32,
    interleaved: bool,
    out: &mut [u8],
    pos: usize,
) -> usize {
    let mask = mask_for_bits(bit_width);
    if interleaved {
        let mut bs = [
            InterleavedBitWriter::<4>::new(pos, bit_width),
            InterleavedBitWriter::<4>::new(pos + 4, bit_width),
            InterleavedBitWriter::<4>::new(pos + 8, bit_width),
            InterleavedBitWriter::<4>::new(pos + 12, bit_width),
        ];
        for chunk in input
            .as_chunks::<4>()
            .0
        {
            for (w, &val) in bs
                .iter_mut()
                .zip(chunk)
            {
                w.write(out, val & mask);
            }
        }
    } else {
        let mut bs = BitWriter::new(pos, bit_width);
        for &val in input {
            bs.write(out, val & mask);
        }
    }
    pos + bytes_for_packed_bits(input.len() as u32, bit_width)
}

fn encode_for(
    input: &[u32],
    bit_width: u32,
    interleaved: bool,
    out: &mut [u8],
    pos: usize,
) -> usize {
    encode_bitmap(input, bit_width, interleaved, out, pos)
}

fn encode_pfor_bitmap(
    input: &[u32],
    bit_width: u32,
    exception_bit_width: u32,
    interleaved: bool,
    out: &mut [u8],
    pos: usize,
) -> usize {
    out[pos] = exception_bit_width as u8;
    let mut pos = pos + 1;

    let mut bs = BitWriter::new(pos, 1);
    for &val in input {
        bs.write(out, u32::from((val >> bit_width) != 0));
    }
    pos += bytes_for_packed_bits(input.len() as u32, 1);

    let mut bs = BitWriter::new(pos, exception_bit_width);
    let mut num_exceptions = 0u32;
    for &val in input {
        if (val >> bit_width) != 0 {
            bs.write(out, val >> bit_width);
            num_exceptions += 1;
        }
    }
    pos += bytes_for_packed_bits(num_exceptions, exception_bit_width);

    encode_bitmap(input, bit_width, interleaved, out, pos)
}

fn encode_pfor_vb(
    input: &[u32],
    bit_width: u32,
    interleaved: bool,
    out: &mut [u8],
    pos: usize,
) -> usize {
    let num_exceptions = input
        .iter()
        .filter(|&&val| (val >> bit_width) != 0)
        .count();
    out[pos] = num_exceptions as u8;
    let mut pos = pos + 1;

    pos = encode_bitmap(input, bit_width, interleaved, out, pos);

    for &val in input {
        let val = val >> bit_width;
        if val != 0 {
            pos = write_vb(val, out, pos);
        }
    }

    for (i, &val) in input
        .iter()
        .enumerate()
    {
        if (val >> bit_width) != 0 {
            out[pos] = i as u8;
            pos += 1;
        }
    }

    pos
}

/// Returns the block type, its bit width and, for PFOR_BITMAP, the exception
/// bit width.
fn decide_block_type(input: &[u32]) -> (BlockType, u32, u32) {
    let num = input.len() as u32;
    if input
        .iter()
        .all(|&val| val == input[0])
    {
        return (BlockType::Constant, num_bits(input[0]), 0);
    }

    let mut histogram = [0u32; 33];
    let mut max_bits = 0;
    for &val in input {
        let bits = num_bits(val);
        histogram[bits as usize] += 1;
        max_bits = max_bits.max(bits);
    }

    let mut best_cost = bytes_for_packed_bits(num, max_bits);
    let mut best_bit_width = max_bits;

    let bitmap_cost = bytes_for_packed_bits(num, 1);
    let mut num_exceptions = 0u32;
    for exception_bit_width in 1..=max_bits {
        let test_bit_width = max_bits - exception_bit_width;
        num_exceptions += histogram[(test_bit_width + 1) as usize];

        let cost = 1
            + bitmap_cost
            + bytes_for_packed_bits(num, test_bit_width)
            + bytes_for_packed_bits(num_exceptions, exception_bit_width);
        if cost < best_cost {
            best_cost = cost;
            best_bit_width = test_bit_width;
        }
    }

    for i in (1..=max_bits).rev() {
        histogram[(i - 1) as usize] += histogram[i as usize];
    }

    let mut best_is_varbyte = false;
    for test_bit_width in 0..max_bits {
        let mut cost = 1 + bytes_for_packed_bits(num, test_bit_width);
        if cost >= best_cost {
            break;
        }
        if test_bit_width < max_bits {
            cost += 2 * histogram[(test_bit_width + 1) as usize] as usize;
            if test_bit_width + 7 <= max_bits {
                cost += histogram[(test_bit_width + 7) as usize] as usize;
                if test_bit_width + 14 <= max_bits {
                    cost += histogram[(test_bit_width + 14) as usize] as usize;
                    if test_bit_width + 19 <= max_bits {
                        cost += histogram[(test_bit_width + 19) as usize] as usize;
                        if test_bit_width + 24 <= max_bits {
                            cost += histogram[(test_bit_width + 24) as usize] as usize;
                        }
                    }
                }
            }
        }
        if cost < best_cost {
            best_cost = cost;
            best_bit_width = test_bit_width;
            best_is_varbyte = true;
        }
    }

    if best_is_varbyte {
        (BlockType::PforVb, best_bit_width, 0)
    } else if best_bit_width == max_bits {
        (BlockType::For, max_bits, 0)
    } else {
        (
            BlockType::PforBitmap,
            best_bit_width,
            max_bits - best_bit_width,
        )
    }
}

/// Packs one delta-minus-1-encoded block. May write four bytes past the end of
/// the returned range.
fn encode_pfor_single_block(input: &[u32], interleaved: bool, out: &mut [u8], pos: usize) -> usize {
    debug_assert!(!input.is_empty());
    debug_assert!(!interleaved || input.len() == BLOCK_SIZE);

    let (block_type, bit_width, exception_bit_width) = decide_block_type(input);
    out[pos] = ((block_type as u8) << 6) | bit_width as u8;
    let pos = pos + 1;

    match block_type {
        BlockType::Constant => {
            write_le32(out, pos, input[0]);
            pos + div_round_up(num_bits(input[0]), 8) as usize
        }
        BlockType::For => encode_for(input, bit_width, interleaved, out, pos),
        BlockType::PforBitmap => {
            encode_pfor_bitmap(input, bit_width, exception_bit_width, interleaved, out, pos)
        }
        BlockType::PforVb => encode_pfor_vb(input, bit_width, interleaved, out, pos),
    }
}

fn append_block(input: &[u32], interleaved: bool, encoded: &mut Vec<u8>) {
    let mut buf = [0u8; SCRATCH_LEN];
    let end = encode_pfor_single_block(input, interleaved, &mut buf, 0);
    encoded.extend_from_slice(&buf[..end]);
}

pub struct PostingListBuilder {
    pub encoded: Vec<u8>,
    pending_deltas: Vec<u32>,
    num_docids: u32,
    last_docid: u32,
}

impl Default for PostingListBuilder {
    fn default() -> Self {
        PostingListBuilder {
            encoded: Vec::new(),
            pending_deltas: Vec::new(),
            num_docids: 0,
            last_docid: u32::MAX,
        }
    }
}

impl PostingListBuilder {
    pub fn add_first_docid(&mut self, docid: u32) {
        write_baseval(docid, &mut self.encoded);
        self.num_docids += 1;
        self.last_docid = docid;
    }

    pub fn add_docid(&mut self, docid: u32) {
        if docid == self.last_docid {
            return;
        }
        self.pending_deltas
            .push(
                docid
                    .wrapping_sub(self.last_docid)
                    .wrapping_sub(1),
            );
        self.last_docid = docid;
        if self
            .pending_deltas
            .len()
            == BLOCK_SIZE
        {
            append_block(&self.pending_deltas, true, &mut self.encoded);
            self.pending_deltas
                .clear();
            self.num_docids += BLOCK_SIZE as u32;
        }
    }

    pub fn finish(&mut self) {
        if self
            .pending_deltas
            .is_empty()
        {
            return;
        }
        append_block(&self.pending_deltas, false, &mut self.encoded);
        self.num_docids += self
            .pending_deltas
            .len() as u32;
        self.pending_deltas
            .clear();
    }

    pub fn num_docids(&self) -> u32 {
        self.num_docids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(input: &[u32], interleaved: bool) -> Vec<u8> {
        let mut out = Vec::new();
        append_block(input, interleaved, &mut out);
        out
    }

    fn baseval(val: u32) -> Vec<u8> {
        let mut out = Vec::new();
        write_baseval(val, &mut out);
        out
    }

    fn varbyte(val: u32) -> Vec<u8> {
        let mut buf = [0u8; 16];
        let end = write_vb(val, &mut buf, 0);
        buf[..end].to_vec()
    }

    #[test]
    fn baseval_one_byte_below_128() {
        assert_eq!(baseval(0), [0x00]);
        assert_eq!(baseval(127), [0x7f]);
    }

    #[test]
    fn baseval_two_bytes_below_0x4000() {
        assert_eq!(baseval(128), [0x80, 0x80]);
        assert_eq!(baseval(0x3fff), [0xbf, 0xff]);
    }

    #[test]
    fn baseval_three_bytes_below_0x200000() {
        assert_eq!(baseval(0x4000), [0xc0, 0x00, 0x40]);
        assert_eq!(baseval(0x1fffff), [0xdf, 0xff, 0xff]);
    }

    #[test]
    fn baseval_four_bytes_below_0x10000000() {
        assert_eq!(baseval(0x200000), [0xe0, 0x20, 0x00, 0x00]);
        assert_eq!(baseval(0xfffffff), [0xef, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn varbyte_single_byte_up_to_176() {
        assert_eq!(varbyte(0), [0]);
        assert_eq!(varbyte(176), [176]);
    }

    #[test]
    fn varbyte_two_bytes_up_to_16560() {
        assert_eq!(varbyte(177), [177, 0]);
        assert_eq!(varbyte(16560), [240, 255]);
    }

    #[test]
    fn varbyte_three_bytes_up_to_540848() {
        assert_eq!(varbyte(16561), [241, 0, 0]);
        assert_eq!(varbyte(540848), [248, 0xff, 0xff]);
    }

    #[test]
    fn varbyte_four_bytes_up_to_16777215() {
        assert_eq!(varbyte(540849), [249, 0xb1, 0x40, 0x08]);
        assert_eq!(varbyte(16777215), [249, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn varbyte_five_bytes_above_16777215() {
        assert_eq!(varbyte(16777216), [250, 0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn constant_block_keeps_only_the_significant_bytes() {
        assert_eq!(encode(&[5, 5, 5, 5], false), [0xc3, 0x05]);
    }

    #[test]
    fn constant_block_of_zeroes_is_the_type_byte_alone() {
        assert_eq!(encode(&[0, 0, 0], false), [0xc0]);
    }

    #[test]
    fn for_block_tail_packs_four_two_bit_values() {
        assert_eq!(encode(&[1, 2, 3, 1], false), [0x02, 0x79]);
    }

    #[test]
    fn for_block_full_interleaves_four_streams() {
        let input: Vec<u32> = (0..128)
            .map(|i| i % 4)
            .collect();
        let mut expect = vec![0x02];
        for _ in 0..2 {
            expect.extend_from_slice(&[0x00; 4]);
            expect.extend_from_slice(&[0x55; 4]);
            expect.extend_from_slice(&[0xaa; 4]);
            expect.extend_from_slice(&[0xff; 4]);
        }
        assert_eq!(encode(&input, true), expect);
    }

    #[test]
    fn for_block_tail_packs_the_same_values_sequentially() {
        let input: Vec<u32> = (0..128)
            .map(|i| i % 4)
            .collect();
        let mut expect = vec![0x02];
        expect.extend_from_slice(&[0xe4; 32]);
        assert_eq!(encode(&input, false), expect);
    }

    #[test]
    fn pfor_bitmap_block_with_thirty_two_wide_exceptions() {
        let mut input = vec![0u32; 96];
        input.extend(std::iter::repeat_n(255u32, 32));
        let mut expect = vec![0x80, 0x08];
        expect.extend_from_slice(&[0x00; 12]);
        expect.extend_from_slice(&[0xff; 4]);
        expect.extend_from_slice(&[0xff; 32]);
        assert_eq!(encode(&input, false), expect);
    }

    #[test]
    fn pfor_vb_block_with_one_exception() {
        let mut input = vec![0u32; 31];
        input.push(255);
        assert_eq!(encode(&input, false), [0x40, 0x01, 0xb1, 0x4e, 0x1f]);
    }

    #[test]
    fn posting_list_tail_block_of_two_deltas() {
        let mut pl = PostingListBuilder::default();
        pl.add_first_docid(5);
        pl.add_docid(6);
        pl.add_docid(6);
        pl.add_docid(7);
        pl.finish();
        assert_eq!(pl.encoded, [0x05, 0xc0]);
        assert_eq!(pl.num_docids(), 3);
    }

    #[test]
    fn posting_list_full_block_leaves_no_tail() {
        let mut pl = PostingListBuilder::default();
        pl.add_first_docid(0);
        for docid in 1..=128 {
            pl.add_docid(docid);
        }
        pl.finish();
        assert_eq!(pl.encoded, [0x00, 0xc0]);
        assert_eq!(pl.num_docids(), 129);
    }
}
