//! A simulated disk for crash tests.
//!
//! Writes land in a volatile image. `sync` makes them durable. The device can
//! be told to die after a number of writes and syncs, and `crash_image` then
//! builds what a disk could hold after a power cut: the durable image plus any
//! subset of the unsynced writes, some of them torn at 512-byte sectors.
//! Choosing each write independently also covers a disk that reorders writes.

use std::io;
use std::sync::{Arc, Mutex};

use crate::storage::Storage;

const SECTOR: usize = 512;

/// Small deterministic random generator (xorshift64*), so a failing seed can
/// be replayed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }
}

#[derive(Default)]
struct State {
    durable: Vec<u8>,
    view: Vec<u8>,
    pending: Vec<(u64, Vec<u8>)>,
    ops_left: Option<u64>,
    dead: bool,
    writes: u64,
    syncs: u64,
}

/// Cloning shares the same simulated disk.
#[derive(Clone, Default)]
pub struct SimStorage {
    state: Arc<Mutex<State>>,
}

fn died() -> io::Error {
    io::Error::other("simulated power cut")
}

impl SimStorage {
    pub fn new() -> SimStorage {
        SimStorage::default()
    }

    /// A disk holding exactly `image`, all of it durable.
    pub fn from_image(image: Vec<u8>) -> SimStorage {
        let s = SimStorage::default();
        {
            let mut st = s.state.lock().unwrap();
            st.view = image.clone();
            st.durable = image;
        }
        s
    }

    /// Let `n` more writes or syncs succeed; the next one cuts the power.
    pub fn fail_after(&self, n: u64) {
        self.state.lock().unwrap().ops_left = Some(n);
    }

    pub fn is_dead(&self) -> bool {
        self.state.lock().unwrap().dead
    }

    /// Writes and syncs seen so far.
    pub fn counts(&self) -> (u64, u64) {
        let st = self.state.lock().unwrap();
        (st.writes, st.syncs)
    }

    /// What the disk could hold after a power cut now.
    pub fn crash_image(&self, rng: &mut Rng) -> Vec<u8> {
        let st = self.state.lock().unwrap();
        let mut img = st.durable.clone();
        // Pick one of a few disk behaviours per crash, then decide each
        // unsynced write independently under it.
        let (keep, tear) = match rng.below(4) {
            0 => (100, 0), // every write landed (a process crash)
            1 => (0, 0),   // nothing after the last sync landed
            2 => (50, 0),  // any subset, i.e. reordered
            _ => (50, 30), // subset, and some writes torn
        };
        for (off, data) in &st.pending {
            if rng.chance(tear) {
                for (i, sector) in data.chunks(SECTOR).enumerate() {
                    if rng.chance(50) {
                        apply(&mut img, *off + (i * SECTOR) as u64, sector);
                    }
                }
            } else if rng.chance(keep) {
                apply(&mut img, *off, data);
            }
        }
        img
    }

    fn tick(st: &mut State) -> io::Result<()> {
        if st.dead {
            return Err(died());
        }
        if let Some(n) = st.ops_left.as_mut() {
            if *n == 0 {
                st.dead = true;
                return Err(died());
            }
            *n -= 1;
        }
        Ok(())
    }
}

fn apply(img: &mut Vec<u8>, off: u64, data: &[u8]) {
    let end = off as usize + data.len();
    if img.len() < end {
        img.resize(end, 0);
    }
    img[off as usize..end].copy_from_slice(data);
}

impl Storage for SimStorage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let st = self.state.lock().unwrap();
        if st.dead {
            return Err(died());
        }
        let end = offset as usize + buf.len();
        if end > st.view.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end",
            ));
        }
        buf.copy_from_slice(&st.view[offset as usize..end]);
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut st = self.state.lock().unwrap();
        st.writes += 1;
        if let Err(e) = SimStorage::tick(&mut st) {
            // The write that hit the power cut may still partly land.
            st.pending.push((offset, data.to_vec()));
            return Err(e);
        }
        apply(&mut st.view, offset, data);
        st.pending.push((offset, data.to_vec()));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut st = self.state.lock().unwrap();
        st.syncs += 1;
        SimStorage::tick(&mut st)?;
        st.durable = st.view.clone();
        st.pending.clear();
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        let st = self.state.lock().unwrap();
        if st.dead {
            return Err(died());
        }
        Ok(st.view.len() as u64)
    }
}
