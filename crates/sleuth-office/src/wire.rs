//! Variable-length integers and strings, shared by the in-memory folder
//! blobs and the catalog files.

pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// `None` when the bytes end inside a number or it does not fit 64 bits.
pub(crate) fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let (mut v, mut shift) = (0u64, 0u32);
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        if shift > 63 {
            return None;
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return Some(v);
        }
        shift += 7;
    }
}

/// Signed numbers with small magnitudes stay short.
pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub(crate) fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub(crate) fn get_str<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a str> {
    let n = usize::try_from(get_varint(buf, pos)?).ok()?;
    let end = pos.checked_add(n)?;
    let s = std::str::from_utf8(buf.get(*pos..end)?).ok()?;
    *pos = end;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip() {
        let mut b = Vec::new();
        let vals = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for v in vals {
            put_varint(&mut b, v);
        }
        let mut p = 0;
        for v in vals {
            assert_eq!(get_varint(&b, &mut p), Some(v));
        }
        assert_eq!(get_varint(&b, &mut p), None, "nothing left");
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, 1_700_000_000] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(get_varint(&[0x80, 0x80], &mut 0), None, "cut short");
        assert_eq!(get_varint(&[0xff; 11], &mut 0), None, "too long");
    }

    #[test]
    fn strings_round_trip() {
        let mut b = Vec::new();
        put_str(&mut b, "Обычный.pdf");
        put_str(&mut b, "");
        let mut p = 0;
        assert_eq!(get_str(&b, &mut p), Some("Обычный.pdf"));
        assert_eq!(get_str(&b, &mut p), Some(""));
        assert_eq!(get_str(&[5, b'a'], &mut 0), None, "length past the end");
        assert_eq!(get_str(&[2, 0xff, 0xfe], &mut 0), None, "not text");
    }
}
