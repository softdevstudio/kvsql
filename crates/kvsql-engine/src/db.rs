//! The database handle, write transactions, commit and recovery.
//!
//! File layout: pages 0 and 1 are meta pages, written alternately. A commit
//! writes every new page, syncs, then writes the meta page for the new
//! transaction id and syncs again. Opening the file picks the valid meta page
//! with the highest transaction id, so a commit is either fully visible or
//! not visible at all.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::error::{corrupt, Error, Result};
use crate::page::{
    self, get_u32, get_u64, new_page, put_u32, put_u64, PageType, HEADER, PAGE_SIZE,
};
use crate::storage::{FileStorage, Storage};
use crate::tree::{self, Page, PageRead, PageWrite, TreeStats, Val, MAX_KEY_LEN, MAX_VALUE_LEN};

const MAGIC: &[u8; 8] = b"KVSQLDB\0";
const FORMAT_VERSION: u32 = 1;
const FREE_HDR: usize = HEADER + 12;
const FREE_PER_PAGE: usize = (PAGE_SIZE - FREE_HDR) / 8;

/// Tuning knobs for [`Db::open_with`].
#[derive(Clone, Debug)]
pub struct Options {
    /// Pages kept in the read cache (4 KiB each).
    pub cache_pages: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { cache_pages: 2048 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Meta {
    txid: u64,
    dir_root: u64,
    page_count: u64,
    free_head: u64,
    free_count: u64,
}

impl Meta {
    fn encode(&self, slot: u64) -> Vec<u8> {
        let mut p = new_page(PageType::Meta);
        p[HEADER..HEADER + 8].copy_from_slice(MAGIC);
        put_u32(&mut p, HEADER + 8, FORMAT_VERSION);
        put_u32(&mut p, HEADER + 12, PAGE_SIZE as u32);
        put_u64(&mut p, HEADER + 16, self.txid);
        put_u64(&mut p, HEADER + 24, self.dir_root);
        put_u64(&mut p, HEADER + 32, self.page_count);
        put_u64(&mut p, HEADER + 40, self.free_head);
        put_u64(&mut p, HEADER + 48, self.free_count);
        page::seal(&mut p, slot);
        p
    }

    fn decode(p: &[u8], slot: u64) -> Option<Meta> {
        if page::check(p, slot).ok()? != PageType::Meta {
            return None;
        }
        if &p[HEADER..HEADER + 8] != MAGIC
            || get_u32(p, HEADER + 8) != FORMAT_VERSION
            || get_u32(p, HEADER + 12) != PAGE_SIZE as u32
        {
            return None;
        }
        let m = Meta {
            txid: get_u64(p, HEADER + 16),
            dir_root: get_u64(p, HEADER + 24),
            page_count: get_u64(p, HEADER + 32),
            free_head: get_u64(p, HEADER + 40),
            free_count: get_u64(p, HEADER + 48),
        };
        let sane = m.page_count >= 2
            && m.dir_root < m.page_count
            && m.free_head < m.page_count
            && m.free_count < m.page_count
            && m.txid % 2 == slot;
        sane.then_some(m)
    }
}

struct Cache {
    map: HashMap<u64, Page>,
    cap: usize,
}

impl Cache {
    fn get(&self, id: u64) -> Option<Page> {
        self.map.get(&id).cloned()
    }
    fn insert(&mut self, id: u64, p: Page) {
        if self.cap == 0 {
            return;
        }
        if self.map.len() >= self.cap && !self.map.contains_key(&id) {
            let drop: Vec<u64> = self.map.keys().take(self.cap / 4 + 1).copied().collect();
            for k in drop {
                self.map.remove(&k);
            }
        }
        self.map.insert(id, p);
    }
    fn remove(&mut self, id: u64) {
        self.map.remove(&id);
    }
}

struct Inner {
    store: Box<dyn Storage>,
    cache: Cache,
    meta: Meta,
    /// Pages holding the persisted free list (reachable from `meta`).
    free_pages: Vec<u64>,
    /// Page ids the persisted free list says are unused.
    free_ids: Vec<u64>,
    poisoned: bool,
}

impl PageRead for Inner {
    fn read(&mut self, id: u64) -> Result<Page> {
        if id < 2 || id >= self.meta.page_count {
            return corrupt(format!(
                "page {id} is outside the file ({} pages)",
                self.meta.page_count
            ));
        }
        if let Some(p) = self.cache.get(id) {
            return Ok(p);
        }
        let mut buf = vec![0u8; PAGE_SIZE];
        self.store.read_at(id * PAGE_SIZE as u64, &mut buf)?;
        page::check(&buf, id)?;
        let p: Page = buf.into();
        self.cache.insert(id, p.clone());
        Ok(p)
    }
}

impl Inner {
    fn check(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn ks_root(&mut self, name: &[u8]) -> Result<Option<u64>> {
        let dir = self.meta.dir_root;
        lookup_root(self, dir, name)
    }
}

fn lookup_root<R: PageRead>(r: &mut R, dir_root: u64, name: &[u8]) -> Result<Option<u64>> {
    match tree::get(r, dir_root, name)? {
        None => Ok(None),
        Some(Val::Inline(b)) if b.len() == 8 => Ok(Some(u64::from_le_bytes(b.try_into().unwrap()))),
        Some(_) => corrupt("directory entry is not a page id"),
    }
}

fn check_key(key: &[u8]) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(Error::KeyTooLarge(key.len()));
    }
    Ok(())
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_KEY_LEN {
        return Err(Error::BadKeyspaceName);
    }
    Ok(())
}

fn collect<R: PageRead>(
    r: &mut R,
    root: u64,
    from: &[u8],
    to: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut raw = Vec::new();
    tree::scan(r, root, from, to, limit, &mut raw)?;
    raw.into_iter()
        .map(|(k, v)| Ok((k, tree::read_value(r, v)?)))
        .collect()
}

/// What [`Db::verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub txid: u64,
    pub page_count: u64,
    pub used_pages: u64,
    pub free_pages: u64,
    pub keyspaces: u64,
    pub keys: u64,
}

/// An open database file.
///
/// All methods take `&self`; the handle can be shared between threads.
/// One write transaction runs at a time, and it blocks reads made through
/// the handle until it commits or is dropped.
pub struct Db {
    inner: Mutex<Inner>,
}

impl Db {
    /// Open or create a database file. The file stays locked against other
    /// processes until the handle is dropped.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(
            Box::new(FileStorage::open(path.as_ref())?),
            Options::default(),
        )
    }

    /// Open on any [`Storage`].
    pub fn open_with(store: Box<dyn Storage>, opts: Options) -> Result<Db> {
        let len = store.len()?;
        let mut best: Option<Meta> = None;
        for slot in 0..2u64 {
            if len >= (slot + 1) * PAGE_SIZE as u64 {
                let mut buf = vec![0u8; PAGE_SIZE];
                store.read_at(slot * PAGE_SIZE as u64, &mut buf)?;
                if let Some(m) = Meta::decode(&buf, slot) {
                    if best.is_none_or(|b| m.txid > b.txid) {
                        best = Some(m);
                    }
                }
            }
        }
        let meta = match best {
            Some(m) => m,
            None if len <= 2 * PAGE_SIZE as u64 => {
                // New file, or one whose first commit never finished.
                let m = Meta {
                    txid: 0,
                    dir_root: 0,
                    page_count: 2,
                    free_head: 0,
                    free_count: 0,
                };
                store.write_at(0, &m.encode(0))?;
                store.sync()?;
                m
            }
            None => return corrupt("no valid meta page"),
        };
        let mut inner = Inner {
            store,
            cache: Cache {
                map: HashMap::new(),
                cap: opts.cache_pages,
            },
            meta,
            free_pages: Vec::new(),
            free_ids: Vec::new(),
            poisoned: false,
        };
        // Load the free list.
        let mut id = meta.free_head;
        while id != 0 {
            if inner.free_pages.len() as u64 > meta.page_count {
                return corrupt("free list loops");
            }
            let p = inner.read(id)?;
            if page::page_type(&p) != PageType::Free as u8 {
                return corrupt("free list points at a non-free-list page");
            }
            let n = get_u32(&p, HEADER + 8) as usize;
            if n > FREE_PER_PAGE {
                return corrupt("free list page overfull");
            }
            for j in 0..n {
                inner.free_ids.push(get_u64(&p, FREE_HDR + 8 * j));
            }
            inner.free_pages.push(id);
            id = get_u64(&p, HEADER);
        }
        if inner.free_ids.len() as u64 != meta.free_count {
            return corrupt("free list length does not match meta page");
        }
        Ok(Db {
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.check()?;
        Ok(g)
    }

    /// Id of the last committed transaction.
    pub fn txid(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .txid
    }

    pub fn get(&self, keyspace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut g = self.lock()?;
        let Some(root) = g.ks_root(keyspace.as_bytes())? else {
            return Ok(None);
        };
        match tree::get(&mut *g, root, key)? {
            None => Ok(None),
            Some(v) => Ok(Some(tree::read_value(&mut *g, v)?)),
        }
    }

    /// Pairs with `from <= key < to` (no upper bound when `to` is `None`),
    /// in key order, at most `limit`.
    pub fn scan(
        &self,
        keyspace: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut g = self.lock()?;
        let Some(root) = g.ks_root(keyspace.as_bytes())? else {
            return Ok(Vec::new());
        };
        collect(&mut *g, root, from, to, limit)
    }

    /// Names of all keyspaces, in byte order.
    pub fn keyspaces(&self) -> Result<Vec<String>> {
        let mut g = self.lock()?;
        let dir = g.meta.dir_root;
        let mut raw = Vec::new();
        tree::scan(&mut *g, dir, &[], None, usize::MAX, &mut raw)?;
        Ok(raw
            .into_iter()
            .map(|(k, _)| String::from_utf8_lossy(&k).into_owned())
            .collect())
    }

    /// Put one pair in its own transaction.
    pub fn put(&self, keyspace: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let mut tx = self.begin_write()?;
        tx.put(keyspace, key, value)?;
        tx.commit()
    }

    /// Delete one key in its own transaction. Returns whether it existed.
    pub fn delete(&self, keyspace: &str, key: &[u8]) -> Result<bool> {
        let mut tx = self.begin_write()?;
        let found = tx.delete(keyspace, key)?;
        tx.commit()?;
        Ok(found)
    }

    /// Start a write transaction. Nothing reaches the file until `commit`;
    /// dropping the transaction discards it.
    pub fn begin_write(&self) -> Result<WriteTx<'_>> {
        let g = self.lock()?;
        let mut reusable = g.free_ids.clone();
        reusable.sort_unstable_by(|a, b| b.cmp(a));
        let page_count = g.meta.page_count;
        let dir_root = g.meta.dir_root;
        Ok(WriteTx {
            inner: g,
            dirty: HashMap::new(),
            fresh: HashSet::new(),
            reusable,
            pending: Vec::new(),
            page_count,
            dir_root,
            roots: HashMap::new(),
            changed: Vec::new(),
        })
    }

    /// Check every page and invariant in the file.
    pub fn verify(&self) -> Result<VerifyReport> {
        let mut g = self.lock()?;
        let meta = g.meta;
        let mut seen: HashSet<u64> = HashSet::from([0, 1]);
        let mut dir_stats = TreeStats::default();
        tree::verify(&mut *g, meta.dir_root, &mut seen, &mut dir_stats)?;
        let mut entries = Vec::new();
        tree::scan(&mut *g, meta.dir_root, &[], None, usize::MAX, &mut entries)?;
        let mut keys = 0;
        for (name, v) in &entries {
            let root = match v {
                Val::Inline(b) if b.len() == 8 => {
                    u64::from_le_bytes(b.as_slice().try_into().unwrap())
                }
                _ => return corrupt("directory entry is not a page id"),
            };
            let mut st = TreeStats::default();
            tree::verify(&mut *g, root, &mut seen, &mut st).map_err(|e| {
                Error::Corrupt(format!("keyspace {:?}: {e}", String::from_utf8_lossy(name)))
            })?;
            keys += st.keys;
        }
        for &fp in &g.free_pages.clone() {
            if !seen.insert(fp) {
                return corrupt(format!("free list page {fp} is also in use"));
            }
        }
        let mut free = HashSet::new();
        for &f in &g.free_ids {
            if f < 2 || f >= meta.page_count {
                return corrupt(format!("free page {f} is outside the file"));
            }
            if seen.contains(&f) {
                return corrupt(format!("page {f} is both free and in use"));
            }
            if !free.insert(f) {
                return corrupt(format!("page {f} is listed free twice"));
            }
        }
        let accounted = seen.len() as u64 + free.len() as u64;
        if accounted != meta.page_count {
            return corrupt(format!(
                "{} pages in file, {} accounted for",
                meta.page_count, accounted
            ));
        }
        Ok(VerifyReport {
            txid: meta.txid,
            page_count: meta.page_count,
            used_pages: seen.len() as u64,
            free_pages: free.len() as u64,
            keyspaces: entries.len() as u64,
            keys,
        })
    }
}

/// A write transaction. See [`Db::begin_write`].
pub struct WriteTx<'a> {
    inner: MutexGuard<'a, Inner>,
    /// New page bodies, sealed, not yet in the file.
    dirty: HashMap<u64, Page>,
    /// Pages allocated by this transaction. They are reachable from no
    /// committed state, so they can be freed and reused at once.
    fresh: HashSet<u64>,
    /// Free pages safe to reuse now.
    reusable: Vec<u64>,
    /// Committed pages this transaction stopped using. Still reachable from
    /// the last commit, so reusable only after this one lands.
    pending: Vec<u64>,
    page_count: u64,
    dir_root: u64,
    /// Keyspace roots seen or changed here; `None` means dropped or absent.
    roots: HashMap<Vec<u8>, Option<u64>>,
    changed: Vec<Vec<u8>>,
}

impl PageRead for WriteTx<'_> {
    fn read(&mut self, id: u64) -> Result<Page> {
        if let Some(p) = self.dirty.get(&id) {
            return Ok(p.clone());
        }
        self.inner.read(id)
    }
}

impl PageWrite for WriteTx<'_> {
    fn alloc(&mut self) -> u64 {
        let id = match self.reusable.pop() {
            Some(id) => id,
            None => {
                self.page_count += 1;
                self.page_count - 1
            }
        };
        self.fresh.insert(id);
        id
    }

    fn free(&mut self, id: u64) {
        if self.fresh.remove(&id) {
            self.dirty.remove(&id);
            self.reusable.push(id);
        } else {
            self.pending.push(id);
        }
    }

    fn write(&mut self, id: u64, page: Vec<u8>) {
        debug_assert!(
            self.fresh.contains(&id),
            "write to page {id} not allocated here"
        );
        // Sealed once at commit; a page can be rewritten many times before then.
        self.dirty.insert(id, page.into());
    }
}

impl WriteTx<'_> {
    fn root_of(&mut self, name: &[u8]) -> Result<Option<u64>> {
        if let Some(r) = self.roots.get(name) {
            return Ok(*r);
        }
        let dir = self.dir_root;
        let r = lookup_root(self, dir, name)?;
        self.roots.insert(name.to_vec(), r);
        Ok(r)
    }

    fn set_root(&mut self, name: &[u8], root: Option<u64>) {
        self.roots.insert(name.to_vec(), root);
        if !self.changed.iter().any(|n| n == name) {
            self.changed.push(name.to_vec());
        }
    }

    pub fn get(&mut self, keyspace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(root) = self.root_of(keyspace.as_bytes())? else {
            return Ok(None);
        };
        match tree::get(self, root, key)? {
            None => Ok(None),
            Some(v) => Ok(Some(tree::read_value(self, v)?)),
        }
    }

    pub fn scan(
        &mut self,
        keyspace: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(root) = self.root_of(keyspace.as_bytes())? else {
            return Ok(Vec::new());
        };
        collect(self, root, from, to, limit)
    }

    /// Insert or replace. Creates the keyspace if it does not exist.
    pub fn put(&mut self, keyspace: &str, key: &[u8], value: &[u8]) -> Result<()> {
        check_name(keyspace)?;
        check_key(key)?;
        if value.len() > MAX_VALUE_LEN {
            return Err(Error::ValueTooLarge(value.len()));
        }
        let name = keyspace.as_bytes();
        let root = self.root_of(name)?.unwrap_or(0);
        let val = if tree::fits_inline(key.len(), value.len()) {
            Val::Inline(value.to_vec())
        } else {
            Val::Overflow {
                page: tree::write_overflow(self, value),
                len: value.len() as u32,
            }
        };
        let (new_root, old) = tree::insert(self, root, key, val)?;
        if let Some(old) = old {
            tree::free_value(self, &old)?;
        }
        self.set_root(name, Some(new_root));
        Ok(())
    }

    /// Remove a key. Returns whether it existed.
    pub fn delete(&mut self, keyspace: &str, key: &[u8]) -> Result<bool> {
        let name = keyspace.as_bytes();
        let Some(root) = self.root_of(name)? else {
            return Ok(false);
        };
        match tree::delete(self, root, key)? {
            None => Ok(false),
            Some((new_root, old)) => {
                tree::free_value(self, &old)?;
                self.set_root(name, Some(new_root));
                Ok(true)
            }
        }
    }

    /// Remove a keyspace and everything in it. Returns whether it existed.
    pub fn drop_keyspace(&mut self, keyspace: &str) -> Result<bool> {
        let name = keyspace.as_bytes();
        let Some(root) = self.root_of(name)? else {
            return Ok(false);
        };
        tree::free_tree(self, root)?;
        self.set_root(name, None);
        Ok(true)
    }

    /// Make every change durable, all at once.
    pub fn commit(mut self) -> Result<()> {
        if self.changed.is_empty() && self.dirty.is_empty() && self.pending.is_empty() {
            return Ok(());
        }
        // Publish keyspace roots in the directory tree.
        for name in std::mem::take(&mut self.changed) {
            let dir = self.dir_root;
            match self.roots[&name] {
                Some(root) => {
                    let (d, _) = tree::insert(
                        &mut self,
                        dir,
                        &name,
                        Val::Inline(root.to_le_bytes().to_vec()),
                    )?;
                    self.dir_root = d;
                }
                None => {
                    if let Some((d, _)) = tree::delete(&mut self, dir, &name)? {
                        self.dir_root = d;
                    }
                }
            }
        }

        // The old free-list pages stop being needed once this commit lands.
        let old_free_pages = self.inner.free_pages.clone();
        self.pending.extend(old_free_pages);

        // Pick pages for the new free list from pages that are safe to
        // overwrite now. Taking a page shrinks the list, so loop.
        let mut list_pages = Vec::new();
        loop {
            let total = self.reusable.len() + self.pending.len();
            if total.div_ceil(FREE_PER_PAGE) <= list_pages.len() {
                break;
            }
            match self.reusable.pop() {
                Some(id) => list_pages.push(id),
                None => {
                    list_pages.push(self.page_count);
                    self.page_count += 1;
                }
            }
        }
        let mut ids: Vec<u64> = self
            .reusable
            .iter()
            .chain(self.pending.iter())
            .copied()
            .collect();
        ids.sort_unstable();
        let mut list_bodies = Vec::new();
        let chunks: Vec<&[u64]> = ids.chunks(FREE_PER_PAGE).collect();
        for (i, &pid) in list_pages.iter().enumerate() {
            let chunk = chunks.get(i).copied().unwrap_or(&[]);
            let mut p = new_page(PageType::Free);
            put_u64(&mut p, HEADER, list_pages.get(i + 1).copied().unwrap_or(0));
            put_u32(&mut p, HEADER + 8, chunk.len() as u32);
            for (j, &f) in chunk.iter().enumerate() {
                put_u64(&mut p, FREE_HDR + 8 * j, f);
            }
            page::seal(&mut p, pid);
            list_bodies.push((pid, p));
        }

        let meta = Meta {
            txid: self.inner.meta.txid + 1,
            dir_root: self.dir_root,
            page_count: self.page_count,
            free_head: list_pages.first().copied().unwrap_or(0),
            free_count: ids.len() as u64,
        };

        let mut writes: Vec<(u64, Page)> = std::mem::take(&mut self.dirty)
            .into_iter()
            .map(|(id, p)| {
                let mut body = p.to_vec();
                page::seal(&mut body, id);
                (id, Page::from(body))
            })
            .collect();
        writes.sort_unstable_by_key(|(id, _)| *id);
        let result = (|| -> Result<()> {
            for (id, p) in &writes {
                self.inner.store.write_at(id * PAGE_SIZE as u64, p)?;
            }
            for (id, p) in &list_bodies {
                self.inner.store.write_at(id * PAGE_SIZE as u64, p)?;
            }
            self.inner.store.sync()?;
            let slot = meta.txid % 2;
            self.inner
                .store
                .write_at(slot * PAGE_SIZE as u64, &meta.encode(slot))?;
            self.inner.store.sync()?;
            Ok(())
        })();
        if let Err(e) = result {
            // The file may hold a partial commit. Reopening recovers the last
            // good state; this handle must not guess.
            self.inner.poisoned = true;
            return Err(e);
        }

        let inner = &mut *self.inner;
        for (id, p) in writes {
            inner.cache.insert(id, p);
        }
        for (id, _) in &list_bodies {
            inner.cache.remove(*id);
        }
        inner.meta = meta;
        inner.free_pages = list_pages;
        inner.free_ids = ids;
        Ok(())
    }
}
