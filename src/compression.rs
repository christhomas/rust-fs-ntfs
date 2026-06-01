//! NTFS LZNT1 decompression (read path for compressed `$DATA`).
//!
//! LZNT1 is the classic per-file NTFS compression codec. A compressed
//! attribute stores its data in **compression units** (typically 16
//! clusters = 64 KiB). Each unit is an independent LZNT1 stream; this
//! module decodes that stream. The read-path plumbing (mapping a unit's
//! data runs to raw clusters, then calling [`decompress_unit`]) lives in
//! the caller — this module is the pure codec so it can be unit-tested
//! without any I/O.
//!
//! ## Stream layout (clean-room, from the documented LZNT1 format — no
//! GPL source consulted)
//!
//! A stream is a sequence of **chunks**. Each chunk starts with a 16-bit
//! little-endian header:
//!
//! ```text
//!   bit 15      compressed flag (1 = compressed token stream, 0 = raw)
//!   bits 12..14 signature, always 0b011 (= 3)
//!   bits 0..11  (chunk_data_len - 1)   -- bytes of chunk body after header
//! ```
//!
//! A header of `0x0000` (or running out of input) ends the stream.
//!
//! A **raw** chunk's body is copied verbatim. A **compressed** chunk's
//! body is a series of flag groups: one flag byte then up to 8 tokens,
//! one per bit (LSB first). A `0` bit is a literal byte; a `1` bit is a
//! 16-bit little-endian back-reference. The split of that 16-bit value
//! into (length, displacement) depends on how many bytes have already
//! been emitted in the current chunk: the displacement field is exactly
//! wide enough to address the bytes produced so far.

/// Maximum bytes a single chunk decompresses to.
const CHUNK_SIZE: usize = 4096;

/// Decompress one LZNT1 compression unit.
///
/// `input` is the unit's raw bytes (the allocated, compressed clusters of
/// the unit, in order). `max_len` caps the output — pass the number of
/// valid bytes this unit contributes to the file (e.g. the unit size, or
/// the remaining `data_size` for the final unit). Decoding stops at the
/// end-of-stream marker or once `max_len` bytes are produced.
pub fn decompress_unit(input: &[u8], max_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(max_len.min(64 * 1024));
    let mut pos = 0usize;

    while pos + 2 <= input.len() && out.len() < max_len {
        let header = u16::from_le_bytes([input[pos], input[pos + 1]]);
        if header == 0 {
            break; // end-of-stream marker
        }
        pos += 2;

        let chunk_len = (header & 0x0FFF) as usize + 1;
        let compressed = header & 0x8000 != 0;
        let chunk_end = pos
            .checked_add(chunk_len)
            .filter(|&e| e <= input.len())
            .ok_or_else(|| format!("LZNT1: chunk body ({chunk_len}) overruns input"))?;

        if compressed {
            decompress_chunk(&input[pos..chunk_end], &mut out)?;
        } else {
            out.extend_from_slice(&input[pos..chunk_end]);
        }
        pos = chunk_end;
    }

    out.truncate(max_len);
    Ok(out)
}

/// Decode one compressed chunk body (the flag-group token stream) onto
/// the end of `out`. Back-references address only bytes within this
/// chunk, so `chunk_start` anchors the per-chunk position used to size
/// the displacement field.
fn decompress_chunk(body: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    let chunk_start = out.len();
    let mut i = 0usize;

    while i < body.len() {
        let flags = body[i];
        i += 1;
        for bit in 0..8 {
            if i >= body.len() {
                break;
            }
            if out.len() - chunk_start >= CHUNK_SIZE {
                return Ok(()); // chunk full (4096 bytes); ignore trailing pad
            }
            if flags & (1 << bit) == 0 {
                // Literal byte.
                out.push(body[i]);
                i += 1;
            } else {
                // Back-reference: 16-bit LE, split by current position.
                if i + 2 > body.len() {
                    return Err("LZNT1: truncated back-reference token".to_string());
                }
                let token = u16::from_le_bytes([body[i], body[i + 1]]);
                i += 2;

                let cur = out.len() - chunk_start; // bytes already in this chunk
                let (length, displacement) = split_token(token, cur)?;
                if displacement > cur {
                    return Err(format!(
                        "LZNT1: back-reference displacement {displacement} exceeds \
                         {cur} bytes decoded in chunk"
                    ));
                }
                // Copy byte-by-byte: displacement may be < length (RLE-style
                // overlapping copy), so we can't use copy_within.
                let start = out.len() - displacement;
                for k in 0..length {
                    let b = out[start + k];
                    out.push(b);
                }
            }
        }
    }
    Ok(())
}

/// Split a back-reference token into `(length, displacement)` for the
/// current within-chunk position `cur` (the number of bytes already
/// emitted in this chunk; always ≥ 1 when a back-reference appears).
///
/// The displacement field is sized so it can address every byte produced
/// so far: `displacement_bits = ceil(log2(cur))`, and the remaining bits
/// hold `length - 3`. So early in the chunk displacements are small and
/// lengths large; as the chunk fills the split shifts the other way.
fn split_token(token: u16, cur: usize) -> Result<(usize, usize), String> {
    if cur == 0 {
        return Err("LZNT1: back-reference at start of chunk".to_string());
    }
    let mut span = cur - 1;
    let mut length_bits: u32 = 12;
    while span >= 16 {
        span >>= 1;
        length_bits -= 1;
    }
    let length = (token as usize & ((1usize << length_bits) - 1)) + 3;
    let displacement = (token as usize >> length_bits) + 1;
    Ok((length, displacement))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `data` as a single LZNT1 chunk using **literals only** (a
    /// trivially-correct encoder: every byte is a literal, flag bytes are
    /// all-zero). Produces a valid compressed stream a conformant decoder
    /// must round-trip. Used to exercise the chunk/flag-group framing
    /// without hand-rolling back-references.
    fn encode_literal_chunk(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= CHUNK_SIZE);
        // Chunk body = ceil(n/8) flag bytes (all 0) interleaved: one flag
        // byte then up to 8 literals.
        let mut body = Vec::new();
        for group in data.chunks(8) {
            body.push(0u8); // all-literal flags
            body.extend_from_slice(group);
        }
        let mut out = Vec::new();
        let header: u16 = 0x8000 | 0x3000 | ((body.len() - 1) as u16 & 0x0FFF);
        out.extend_from_slice(&header.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn round_trips_all_literals() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 7 % 256) as u8).collect();
        let stream = encode_literal_chunk(&data);
        let got = decompress_unit(&stream, data.len()).expect("decompress");
        assert_eq!(got, data);
    }

    #[test]
    fn raw_uncompressed_chunk_is_copied_verbatim() {
        // Header with bit 15 clear => raw chunk; body copied as-is.
        let data = b"raw chunk bytes, not compressed";
        let mut stream = Vec::new();
        let header: u16 = 0x3000 | ((data.len() - 1) as u16 & 0x0FFF); // no 0x8000
        stream.extend_from_slice(&header.to_le_bytes());
        stream.extend_from_slice(data);
        let got = decompress_unit(&stream, data.len()).expect("decompress");
        assert_eq!(&got, data);
    }

    #[test]
    fn back_reference_repeats_prior_bytes() {
        // Build a chunk: literal 'A', then a back-reference (disp=1,
        // length=3) to produce "AAAA". At cur=1, length_bits=12, so the
        // token = ((disp-1) << 12) | (length-3) = (0<<12)|0 = 0x0000.
        // Flag group: bit0=0 (literal 'A'), bit1=1 (back-ref).
        let mut body = Vec::new();
        body.push(0b0000_0010u8); // bit0 literal, bit1 back-ref
        body.push(b'A');
        body.extend_from_slice(&0x0000u16.to_le_bytes()); // disp=1,len=3
        let mut stream = Vec::new();
        let header: u16 = 0x8000 | 0x3000 | ((body.len() - 1) as u16 & 0x0FFF);
        stream.extend_from_slice(&header.to_le_bytes());
        stream.extend_from_slice(&body);

        let got = decompress_unit(&stream, 4).expect("decompress");
        assert_eq!(&got, b"AAAA", "disp=1 len=3 back-ref after 'A' => AAAA");
    }

    #[test]
    fn split_token_boundaries() {
        // cur in 1..=16 => 4 displacement bits, 12 length bits.
        let (len, disp) = split_token(0x0000, 1).unwrap();
        assert_eq!((len, disp), (3, 1));
        let (len, disp) = split_token(0xFFFF, 16).unwrap();
        assert_eq!((len, disp), ((0x0FFF) + 3, 0x0F + 1)); // len 4098, disp 16
                                                           // cur 17 => one shift: 11 length bits, 5 displacement bits.
        let (_len, disp) = split_token(0xFFFF, 17).unwrap();
        assert_eq!(disp, 0x1F + 1); // 5-bit displacement field, max 32
    }

    #[test]
    fn stops_at_zero_header() {
        let data = b"hello";
        let mut stream = encode_literal_chunk(data);
        stream.extend_from_slice(&[0u8, 0u8]); // end marker
        stream.extend_from_slice(b"garbage past the end");
        let got = decompress_unit(&stream, 4096).expect("decompress");
        assert_eq!(&got, data);
    }

    #[test]
    fn truncates_to_max_len() {
        let data: Vec<u8> = (0..100u8).collect();
        let stream = encode_literal_chunk(&data);
        let got = decompress_unit(&stream, 10).expect("decompress");
        assert_eq!(got.len(), 10);
        assert_eq!(&got, &data[..10]);
    }

    #[test]
    fn rejects_overrunning_chunk_header() {
        // Header claims a 4096-byte body but input is short.
        let header: u16 = 0x8000 | 0x3000 | 0x0FFF;
        let mut stream = Vec::new();
        stream.extend_from_slice(&header.to_le_bytes());
        stream.extend_from_slice(b"short");
        assert!(decompress_unit(&stream, 4096).is_err());
    }
}
