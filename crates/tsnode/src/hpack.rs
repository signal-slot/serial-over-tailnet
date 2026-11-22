//! HPACK (RFC 7541), as much as an HTTP/2 client needs: a decoder for the
//! server's header blocks, with the dynamic table and Huffman strings, and an
//! encoder that sends every field as a literal without indexing (so it keeps
//! no table, whatever size the server allows).
//!
//! The decoder returns an error on any malformed block and never panics.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// A decoded header field: name and value.
pub type Field = (Vec<u8>, Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The block ends in the middle of a field.
    Truncated,
    /// An integer does not fit.
    IntegerOverflow,
    /// An index that is in neither table.
    BadIndex(usize),
    /// A Huffman string with an invalid code, EOS, or bad padding.
    BadHuffman,
    /// A dynamic table size update above the limit we advertised.
    TableSizeTooLarge(usize),
    /// A dynamic table size update after the first field of a block.
    LateTableSizeUpdate,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Truncated => write!(f, "truncated header block"),
            Error::IntegerOverflow => write!(f, "integer overflow"),
            Error::BadIndex(i) => write!(f, "bad table index {i}"),
            Error::BadHuffman => write!(f, "bad Huffman string"),
            Error::TableSizeTooLarge(n) => write!(f, "table size {n} above the limit"),
            Error::LateTableSizeUpdate => write!(f, "table size update after a field"),
        }
    }
}

/// Size the RFC charges for an entry: name, value and 32 bytes.
fn entry_size(name: &[u8], value: &[u8]) -> usize {
    name.len() + value.len() + 32
}

pub struct Decoder {
    /// Newest first: index 62 is `table[0]`.
    table: VecDeque<Field>,
    size: usize,
    max_size: usize,
    /// SETTINGS_HEADER_TABLE_SIZE we advertised; the peer may not go above.
    limit: usize,
}

impl Decoder {
    /// `limit` is the SETTINGS_HEADER_TABLE_SIZE we send (4096 by default).
    pub fn new(limit: usize) -> Self {
        Decoder {
            table: VecDeque::new(),
            size: 0,
            max_size: limit,
            limit,
        }
    }

    /// Decodes one complete header block.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<Field>, Error> {
        let mut r = Reader { b: block, pos: 0 };
        let mut out = Vec::new();
        while !r.done() {
            let first = r.peek()?;
            if first & 0x80 != 0 {
                // Indexed field.
                let idx = r.int(7)?;
                out.push(self.get(idx)?);
            } else if first & 0xc0 == 0x40 {
                // Literal with incremental indexing.
                let field = self.literal(&mut r, 6)?;
                self.insert(field.clone());
                out.push(field);
            } else if first & 0xe0 == 0x20 {
                // Dynamic table size update.
                if !out.is_empty() {
                    return Err(Error::LateTableSizeUpdate);
                }
                let size = r.int(5)?;
                if size > self.limit {
                    return Err(Error::TableSizeTooLarge(size));
                }
                self.max_size = size;
                self.evict(0);
            } else {
                // Literal without indexing (0000) or never indexed (0001).
                out.push(self.literal(&mut r, 4)?);
            }
        }
        Ok(out)
    }

    fn literal(&self, r: &mut Reader, prefix: u8) -> Result<Field, Error> {
        let idx = r.int(prefix)?;
        let name = if idx == 0 {
            r.string()?
        } else {
            self.get(idx)?.0
        };
        let value = r.string()?;
        Ok((name, value))
    }

    fn get(&self, idx: usize) -> Result<Field, Error> {
        if idx == 0 {
            return Err(Error::BadIndex(0));
        }
        if idx <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[idx - 1];
            return Ok((n.to_vec(), v.to_vec()));
        }
        self.table
            .get(idx - STATIC_TABLE.len() - 1)
            .cloned()
            .ok_or(Error::BadIndex(idx))
    }

    fn insert(&mut self, field: Field) {
        let size = entry_size(&field.0, &field.1);
        if size > self.max_size {
            // Larger than the whole table: it empties the table (4.4).
            self.table.clear();
            self.size = 0;
            return;
        }
        self.evict(size);
        self.size += size;
        self.table.push_front(field);
    }

    /// Drops the oldest entries until `room` more bytes fit.
    fn evict(&mut self, room: usize) {
        while self.size + room > self.max_size {
            match self.table.pop_back() {
                Some((n, v)) => self.size -= entry_size(&n, &v),
                None => break,
            }
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn done(&self) -> bool {
        self.pos >= self.b.len()
    }

    fn peek(&self) -> Result<u8, Error> {
        self.b.get(self.pos).copied().ok_or(Error::Truncated)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    /// An integer with an N-bit prefix (5.1).
    fn int(&mut self, prefix: u8) -> Result<usize, Error> {
        let max = (1usize << prefix) - 1;
        let mut value = (self.byte()? as usize) & max;
        if value < max {
            return Ok(value);
        }
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            let part = ((b & 0x7f) as usize)
                .checked_shl(shift)
                .filter(|_| shift < 28)
                .ok_or(Error::IntegerOverflow)?;
            value = value.checked_add(part).ok_or(Error::IntegerOverflow)?;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// A string literal, plain or Huffman-coded (5.2).
    fn string(&mut self) -> Result<Vec<u8>, Error> {
        let huffman = self.peek()? & 0x80 != 0;
        let len = self.int(7)?;
        let end = self.pos.checked_add(len).ok_or(Error::Truncated)?;
        let raw = self.b.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        if huffman {
            huffman_decode(raw)
        } else {
            Ok(raw.to_vec())
        }
    }
}

/// Appends a field as a literal without indexing, with a literal name and
/// both strings uncompressed (6.2.2).
pub fn encode_literal(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.push(0x00);
    encode_string(out, name);
    encode_string(out, value);
}

fn encode_string(out: &mut Vec<u8>, s: &[u8]) {
    encode_int(out, 0x00, 7, s.len());
    out.extend_from_slice(s);
}

fn encode_int(out: &mut Vec<u8>, flags: u8, prefix: u8, mut value: usize) {
    let max = (1usize << prefix) - 1;
    if value < max {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max as u8);
    value -= max;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn huffman_decode(src: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(src.len() * 8 / 5);
    let mut code = 0u32;
    let mut len = 0u8;
    for &byte in src {
        for i in (0..8).rev() {
            code = (code << 1) | ((byte >> i) & 1) as u32;
            len += 1;
            if let Some(sym) = HUFFMAN.lookup(code, len) {
                if sym == EOS {
                    return Err(Error::BadHuffman);
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            } else if len >= 30 {
                return Err(Error::BadHuffman);
            }
        }
    }
    // Padding: fewer than 8 bits, all ones (the start of EOS).
    if len >= 8 || code != (1u32 << len) - 1 {
        return Err(Error::BadHuffman);
    }
    Ok(out)
}

const EOS: u16 = 256;

/// The Huffman codes sorted by length and then code, with where each
/// length starts, so a code is found by a binary search among the codes of
/// its length. Built at compile time.
struct Huffman {
    /// (code, symbol), sorted by (length, code).
    sorted: [(u32, u16); 257],
    /// `sorted[start[l]..start[l + 1]]` are the codes of length `l`.
    start: [u16; 32],
}

impl Huffman {
    const fn build(codes: &[(u32, u8); 257]) -> Huffman {
        let mut sorted = [(0u32, 0u16); 257];
        let mut start = [0u16; 32];
        let mut n = 0;
        let mut l = 0;
        while l < 31 {
            start[l] = n as u16;
            // Codes of this length in increasing order (selection by
            // repeatedly taking the next larger code).
            let mut last: i64 = -1;
            loop {
                let mut best: i64 = i64::MAX;
                let mut best_sym = 0;
                let mut s = 0;
                while s < 257 {
                    let (c, cl) = codes[s];
                    if cl as usize == l && (c as i64) > last && (c as i64) < best {
                        best = c as i64;
                        best_sym = s;
                    }
                    s += 1;
                }
                if best == i64::MAX {
                    break;
                }
                sorted[n] = (best as u32, best_sym as u16);
                n += 1;
                last = best;
            }
            l += 1;
        }
        start[31] = n as u16;
        assert!(n == 257);
        Huffman { sorted, start }
    }

    fn lookup(&self, code: u32, len: u8) -> Option<u16> {
        let l = len as usize;
        if l >= 31 {
            return None;
        }
        let codes = &self.sorted[self.start[l] as usize..self.start[l + 1] as usize];
        codes
            .binary_search_by_key(&code, |&(c, _)| c)
            .ok()
            .map(|i| codes[i].1)
    }
}

static HUFFMAN: Huffman = Huffman::build(&HUFFMAN_CODES);

/// RFC 7541 Appendix B: (code, length in bits) for symbols 0 to 255 and EOS.
const HUFFMAN_CODES: [(u32, u8); 257] = [
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
    (0x3fffffff, 30),
];

/// RFC 7541 Appendix A.
const STATIC_TABLE: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn fields(list: &[(&str, &str)]) -> Vec<Field> {
        list.iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn huffman_codes_are_a_prefix_code_and_complete() {
        // Kraft sum of a complete prefix code is exactly 1.
        let sum: u64 = HUFFMAN_CODES.iter().map(|&(_, l)| 1u64 << (30 - l)).sum();
        assert_eq!(sum, 1 << 30);
        for (s, &(c, l)) in HUFFMAN_CODES.iter().enumerate() {
            assert_eq!(HUFFMAN.lookup(c, l), Some(s as u16));
        }
    }

    #[test]
    fn huffman_strings_from_rfc_7541() {
        // C.4 and C.6.
        for (coded, plain) in [
            ("f1e3 c2e5 f23a 6ba0 ab90 f4ff", "www.example.com"),
            ("a8eb 1064 9cbf", "no-cache"),
            ("25a8 49e9 5ba9 7d7f", "custom-key"),
            ("25a8 49e9 5bb8 e8b4 bf", "custom-value"),
            ("6402", "302"),
            ("aec3 771a 4b", "private"),
            (
                "d07a be94 1054 d444 a820 0595 040b 8166 e082 a62d 1bff",
                "Mon, 21 Oct 2013 20:13:21 GMT",
            ),
            (
                "9d29 ad17 1863 c78f 0b97 c8e9 ae82 ae43 d3",
                "https://www.example.com",
            ),
        ] {
            assert_eq!(huffman_decode(&hex(coded)).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn requests_from_rfc_7541_with_and_without_huffman() {
        // C.3 (plain) and C.4 (Huffman): three requests on one connection.
        let expected = [
            fields(&[
                (":method", "GET"),
                (":scheme", "http"),
                (":path", "/"),
                (":authority", "www.example.com"),
            ]),
            fields(&[
                (":method", "GET"),
                (":scheme", "http"),
                (":path", "/"),
                (":authority", "www.example.com"),
                ("cache-control", "no-cache"),
            ]),
            fields(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/index.html"),
                (":authority", "www.example.com"),
                ("custom-key", "custom-value"),
            ]),
        ];
        let plain = [
            "8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d",
            "8286 84be 5808 6e6f 2d63 6163 6865",
            "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
        ];
        let huffman = [
            "8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff",
            "8286 84be 5886 a8eb 1064 9cbf",
            "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf",
        ];
        for blocks in [plain, huffman] {
            let mut d = Decoder::new(4096);
            for (block, want) in blocks.iter().zip(&expected) {
                assert_eq!(&d.decode(&hex(block)).unwrap(), want);
            }
            // Table after the third request (C.3.3): 164 bytes.
            assert_eq!(d.size, 164);
            assert_eq!(
                d.table[0],
                (b"custom-key".to_vec(), b"custom-value".to_vec())
            );
        }
    }

    #[test]
    fn entries_are_evicted_oldest_first() {
        let mut d = Decoder::new(100);
        let mut block = Vec::new();
        for (n, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
            // Literal with incremental indexing, new name: 34 bytes each.
            block.push(0x40);
            encode_string(&mut block, n.as_bytes());
            encode_string(&mut block, v.as_bytes());
        }
        d.decode(&block).unwrap();
        // Two fit in 100 bytes; "a" was evicted. Index 62 is the newest.
        assert_eq!(d.size, 68);
        assert_eq!(
            d.decode(&[0xbe, 0xbf]).unwrap(),
            fields(&[("c", "3"), ("b", "2")])
        );
        assert_eq!(d.decode(&[0xc0]), Err(Error::BadIndex(64)));
        // Shrinking the table evicts as well.
        assert_eq!(d.decode(&[0x3f, 0x16]).unwrap(), vec![]); // size 53
        assert_eq!(d.decode(&[0xbe]).unwrap(), fields(&[("c", "3")]));
        assert_eq!(d.decode(&[0xbf]), Err(Error::BadIndex(63)));
    }

    #[test]
    fn bad_blocks_are_errors_not_panics() {
        let mut d = Decoder::new(4096);
        assert_eq!(d.decode(&[0x80]), Err(Error::BadIndex(0)));
        assert_eq!(d.decode(&[0xff, 0x80]), Err(Error::Truncated));
        assert_eq!(
            d.decode(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]),
            Err(Error::IntegerOverflow)
        );
        assert_eq!(d.decode(&[0x00, 0x05, b'a']), Err(Error::Truncated));
        // Table size above the advertised limit, and after a field.
        assert_eq!(
            d.decode(&[0x3f, 0xe2, 0x1f]),
            Err(Error::TableSizeTooLarge(4097))
        );
        assert_eq!(d.decode(&[0x82, 0x20]), Err(Error::LateTableSizeUpdate));
        // EOS inside a string, and padding that is not all ones.
        assert_eq!(
            d.decode(&[0x00, 0x84, 0xff, 0xff, 0xff, 0xff, 0x00]),
            Err(Error::BadHuffman)
        );
        assert_eq!(d.decode(&[0x00, 0x81, 0x00, 0x00]), Err(Error::BadHuffman));
        // Every truncation of a valid block fails cleanly.
        let block = hex("8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf");
        for cut in 1..block.len() {
            let _ = Decoder::new(4096).decode(&block[..cut]);
        }
    }

    #[test]
    fn literal_encoding_round_trips() {
        let list = fields(&[
            (":method", "POST"),
            (":path", "/machine/map"),
            ("content-type", "application/json"),
            ("x-long", "v".repeat(300).as_str()),
        ]);
        let mut block = Vec::new();
        for (n, v) in &list {
            encode_literal(&mut block, n, v);
        }
        let mut d = Decoder::new(4096);
        assert_eq!(d.decode(&block).unwrap(), list);
        assert_eq!(d.size, 0, "literals without indexing add nothing");
    }
}
