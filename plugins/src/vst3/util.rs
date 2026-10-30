//! Small helpers for the fixed-size strings and ids VST3 structs carry.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`.

use std::ffi::c_char;
use vst3::Steinberg as sb;

/// Reads a fixed-size `char8[N]` buffer, stopping at the first NUL or the end of the array.
pub(crate) fn read_c8(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Reads a fixed-size UTF-16 `String128`, stopping at the first NUL or the end of the array.
pub(crate) fn read_c16(buf: &[u16]) -> String {
    let units: Vec<u16> = buf.iter().take_while(|c| **c != 0).copied().collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// Writes `s` into a UTF-16 buffer, truncated and always NUL-terminated.
pub(crate) fn write_c16(s: &str, dst: &mut [u16]) {
    let Some(room) = dst.len().checked_sub(1) else { return };
    let mut n = 0;
    for (d, u) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = u;
        n += 1;
    }
    if let Some(z) = dst.get_mut(n) {
        *z = 0;
    }
}

/// A `TUID` (`[i8; 16]`) as raw bytes.
pub(crate) fn tuid_bytes(t: &sb::TUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (o, b) in out.iter_mut().zip(t.iter()) {
        *o = *b as u8;
    }
    out
}

/// Upper-case hex of a class id: the form used as [`crate::PluginInfo::class_id`].
pub(crate) fn hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

/// The 16 bytes of a class id written by [`hex`] (`None` unless it is 32 hex digits).
pub(crate) fn parse_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_bounded() {
        assert_eq!(read_c8(&[b'a' as c_char, b'b' as c_char, 0, b'c' as c_char]), "ab");
        assert_eq!(read_c8(&[b' ' as c_char, b'x' as c_char]), "x");
        let mut buf = [7u16; 4];
        write_c16("héllo", &mut buf);
        assert_eq!(buf, ['h' as u16, 'é' as u16, 'l' as u16, 0]);
        assert_eq!(read_c16(&buf), "hél");
        write_c16("x", &mut []);
    }

    #[test]
    fn class_ids_round_trip_through_hex() {
        let cid: [u8; 16] = std::array::from_fn(|i| (i * 17) as u8);
        let h = hex(&cid);
        assert_eq!(h, "00112233445566778899AABBCCDDEEFF");
        assert_eq!(parse_hex(&h), Some(cid));
        assert_eq!(parse_hex("00"), None);
        assert_eq!(parse_hex("zz112233445566778899AABBCCDDEEFF"), None);
    }
}
