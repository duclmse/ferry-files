//! The index's own file attribute bitmask, and its mapping to and from
//! Windows `FILE_ATTRIBUTE_*` bits. It lives here because the client needs
//! the mapping too, to write `.efu` file lists.

pub const ARCHIVE: u32 = 1;
pub const COMPRESSED: u32 = 2;
pub const DIRECTORY: u32 = 4;
pub const ENCRYPTED: u32 = 8;
pub const HIDDEN: u32 = 16;
pub const REPARSE_POINT: u32 = 32;
pub const NORMAL: u32 = 64;
pub const OFFLINE: u32 = 128;
pub const SPARSE: u32 = 256;
pub const READONLY: u32 = 512;
pub const SYSTEM: u32 = 1024;
pub const TEMPORARY: u32 = 2048;

/// `FILE_ATTRIBUTE_DIRECTORY`.
pub const WINDOWS_DIRECTORY: u32 = 0x0010;

/// (Windows bit, index bit).
const MAP: [(u32, u32); 12] = [
    (0x0020, ARCHIVE),
    (0x0800, COMPRESSED),
    (WINDOWS_DIRECTORY, DIRECTORY),
    (0x4000, ENCRYPTED),
    (0x0002, HIDDEN),
    (0x0400, REPARSE_POINT),
    (0x0080, NORMAL),
    (0x1000, OFFLINE),
    (0x0200, SPARSE),
    (0x0001, READONLY),
    (0x0004, SYSTEM),
    (0x0100, TEMPORARY),
];

/// Windows `FILE_ATTRIBUTE_*` bits to the index's bitmask; other bits drop.
pub fn from_windows(windows: u32) -> u32 {
    MAP.iter()
        .filter(|(w, _)| windows & w != 0)
        .fold(0, |mask, (_, ours)| mask | ours)
}

/// The index's bitmask to Windows `FILE_ATTRIBUTE_*` bits.
pub fn to_windows(ours: u32) -> u32 {
    MAP.iter()
        .filter(|(_, o)| ours & o != 0)
        .fold(0, |mask, (w, _)| mask | w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_bit_round_trips() {
        for (w, o) in MAP {
            assert_eq!(from_windows(w), o);
            assert_eq!(to_windows(o), w);
        }
        let all = MAP.iter().fold(0, |m, (w, _)| m | w);
        assert_eq!(to_windows(from_windows(all)), all);
        // FILE_ATTRIBUTE_VIRTUAL has no counterpart.
        assert_eq!(from_windows(0x10000), 0);
    }
}
