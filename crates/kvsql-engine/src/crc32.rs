//! CRC-32C (Castagnoli). Used to catch torn or misdirected page writes.
//!
//! Every page read from disk is checked, so this sits on the read path. On
//! x86_64 with SSE4.2 and on aarch64 with the CRC extension it uses the CPU's
//! CRC-32C instructions; elsewhere a slicing-by-8 table. All three give the
//! same answer, which the tests check.

const POLY: u32 = 0x82F6_3B78;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut j = 1;
        while j < 8 {
            let prev = t[j - 1][i];
            t[j][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            j += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = make_tables();

fn update_soft(mut c: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xFF) as usize]
            ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        c = TABLES[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn update_hw(c: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut c = c as u64;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        c = _mm_crc32_u64(c, u64::from_le_bytes(ch.try_into().unwrap()));
    }
    let mut c = c as u32;
    for &b in chunks.remainder() {
        c = _mm_crc32_u8(c, b);
    }
    c
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn update_hw(c: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut c = c;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        c = __crc32cd(c, u64::from_le_bytes(ch.try_into().unwrap()));
    }
    for &b in chunks.remainder() {
        c = __crc32cb(c, b);
    }
    c
}

fn has_hw() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("sse4.2")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("crc")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

pub fn crc32c(data: &[u8]) -> u32 {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // The feature check caches its answer, so this is a load and a branch.
        if has_hw() {
            // SAFETY: the CPU supports the instructions, checked just above.
            return !unsafe { update_hw(!0, data) };
        }
    }
    !update_soft(!0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytewise(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c = TABLES[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        !c
    }

    #[test]
    fn known_vector() {
        // Standard check value for CRC-32C.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(!update_soft(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn all_paths_agree() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let data: Vec<u8> = (0..10_010)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for len in [0, 1, 7, 8, 9, 63, 64, 65, 4096, 9999] {
            for off in [0, 1, 3] {
                let d = &data[off..off + len];
                let want = bytewise(d);
                assert_eq!(!update_soft(!0, d), want, "soft len {len} off {off}");
                assert_eq!(crc32c(d), want, "len {len} off {off}");
            }
        }
        println!("hardware CRC-32C in use: {}", has_hw());
    }
}
