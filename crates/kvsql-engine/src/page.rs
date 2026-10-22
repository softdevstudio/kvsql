//! Page layout shared by every page in the file.
//!
//! Every page starts with a 16-byte header:
//!
//! ```text
//! 0..4   CRC-32C of bytes 4..PAGE_SIZE
//! 4      page type
//! 5..8   reserved, zero
//! 8..16  the page's own id, so a page written to the wrong place is caught
//! ```

use crate::crc32::crc32c;
use crate::error::{corrupt, Result};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Meta = 1,
    Branch = 2,
    Leaf = 3,
    Overflow = 4,
    Free = 5,
}

impl PageType {
    fn from_u8(b: u8) -> Option<PageType> {
        Some(match b {
            1 => PageType::Meta,
            2 => PageType::Branch,
            3 => PageType::Leaf,
            4 => PageType::Overflow,
            5 => PageType::Free,
            _ => return None,
        })
    }
}

/// A zeroed page buffer with its type set.
pub fn new_page(t: PageType) -> Vec<u8> {
    let mut p = vec![0u8; PAGE_SIZE];
    p[4] = t as u8;
    p
}

/// Stamp the page id and checksum. Call last, after the body is final.
pub fn seal(page: &mut [u8], id: u64) {
    debug_assert_eq!(page.len(), PAGE_SIZE);
    page[8..16].copy_from_slice(&id.to_le_bytes());
    let c = crc32c(&page[4..]);
    page[0..4].copy_from_slice(&c.to_le_bytes());
}

/// Check checksum and id, return the page type.
pub fn check(page: &[u8], id: u64) -> Result<PageType> {
    if page.len() != PAGE_SIZE {
        return corrupt(format!("page {id}: short read"));
    }
    let stored = u32::from_le_bytes(page[0..4].try_into().unwrap());
    if stored != crc32c(&page[4..]) {
        return corrupt(format!("page {id}: checksum mismatch"));
    }
    let own = u64::from_le_bytes(page[8..16].try_into().unwrap());
    if own != id {
        return corrupt(format!("page {id}: holds page {own}"));
    }
    match PageType::from_u8(page[4]) {
        Some(t) => Ok(t),
        None => corrupt(format!("page {id}: unknown type {}", page[4])),
    }
}

pub fn page_type(page: &[u8]) -> u8 {
    page[4]
}

#[inline]
pub fn get_u16(p: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([p[at], p[at + 1]])
}
#[inline]
pub fn get_u32(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().unwrap())
}
#[inline]
pub fn get_u64(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}
#[inline]
pub fn put_u16(p: &mut [u8], at: usize, v: u16) {
    p[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn put_u32(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn put_u64(p: &mut [u8], at: usize, v: u64) {
    p[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_then_check() {
        let mut p = new_page(PageType::Leaf);
        p[100] = 7;
        seal(&mut p, 42);
        assert_eq!(check(&p, 42).unwrap(), PageType::Leaf);
        assert!(check(&p, 43).is_err(), "wrong id must fail");
        p[200] ^= 1;
        assert!(check(&p, 42).is_err(), "flipped bit must fail");
    }
}
