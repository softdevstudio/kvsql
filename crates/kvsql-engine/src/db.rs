//! The database handle, read and write transactions, commit and recovery.
//!
//! File layout: pages 0 and 1 are meta pages, written alternately. A commit
//! writes every new page, syncs, then writes the meta page for the new
//! transaction id and syncs again. Opening the file picks the valid meta page
//! with the highest transaction id, so a commit is either fully visible or
//! not visible at all.
//!
//! Readers work on a snapshot: the tree as of one commit. Committed pages are
//! never changed in place, so a snapshot stays valid while the writer works,
//! as long as no page it can reach is reused. A page that commit T stops
//! using is tagged with T and reused only once no reader holds a snapshot
//! older than T.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{corrupt, Error, Result};
use crate::idhash::{IdMap, IdSet};
use crate::page::{
    self, get_u32, get_u64, new_page, put_u32, put_u64, PageType, HEADER, PAGE_SIZE,
};
use crate::sql::{self, QueryResult, SchemaCache, SchemaMap, Value as SqlValue};
use crate::storage::{FileStorage, Storage};
use crate::tree::{self, Page, PageRead, PageWrite, TreeStats, Val, MAX_KEY_LEN, MAX_VALUE_LEN};

const MAGIC: &[u8; 8] = b"KVSQLDB\0";
const FORMAT_VERSION: u32 = 1;
const FREE_HDR: usize = HEADER + 12;
const FREE_PER_PAGE: usize = (PAGE_SIZE - FREE_HDR) / 8;
const CACHE_SHARDS: usize = 16;

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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Page cache, split into shards so readers on different threads rarely
/// wait on each other. Each shard evicts with the clock algorithm: a page
/// read since the hand last passed gets a second chance, so the root and
/// branch pages that every lookup touches stay in memory.
struct Cache {
    shards: Vec<Mutex<Shard>>,
    cap_per_shard: usize,
}

#[derive(Default)]
struct Shard {
    index: IdMap<usize>,
    slots: Vec<Slot>,
    hand: usize,
}

struct Slot {
    id: u64,
    page: Page,
    used: bool,
}

impl Cache {
    fn new(cap: usize) -> Cache {
        Cache {
            shards: (0..CACHE_SHARDS)
                .map(|_| Mutex::new(Shard::default()))
                .collect(),
            cap_per_shard: cap.div_ceil(CACHE_SHARDS),
        }
    }
    fn shard(&self, id: u64) -> &Mutex<Shard> {
        &self.shards[(id as usize) % CACHE_SHARDS]
    }
    fn get(&self, id: u64) -> Option<Page> {
        let mut s = lock(self.shard(id));
        let i = *s.index.get(&id)?;
        let slot = &mut s.slots[i];
        slot.used = true;
        Some(slot.page.clone())
    }
    fn insert(&self, id: u64, page: Page) {
        if self.cap_per_shard == 0 {
            return;
        }
        let mut s = lock(self.shard(id));
        if let Some(&i) = s.index.get(&id) {
            s.slots[i].page = page;
            return;
        }
        if s.slots.len() < self.cap_per_shard {
            let i = s.slots.len();
            s.slots.push(Slot {
                id,
                page,
                used: false,
            });
            s.index.insert(id, i);
            return;
        }
        loop {
            let h = s.hand;
            s.hand = (h + 1) % s.slots.len();
            if s.slots[h].used {
                s.slots[h].used = false;
                continue;
            }
            let old = s.slots[h].id;
            s.index.remove(&old);
            s.slots[h] = Slot {
                id,
                page,
                used: false,
            };
            s.index.insert(id, h);
            return;
        }
    }
    fn remove(&self, id: u64) {
        let mut s = lock(self.shard(id));
        let Some(i) = s.index.remove(&id) else {
            return;
        };
        s.slots.swap_remove(i);
        if i < s.slots.len() {
            let moved = s.slots[i].id;
            s.index.insert(moved, i);
        }
        if s.hand >= s.slots.len() {
            s.hand = 0;
        }
    }
}

/// A free page and the commit that stopped using it.
#[derive(Clone, Copy, Debug)]
struct Free {
    id: u64,
    freed_at: u64,
}

/// Committed state, changed only by a commit.
struct State {
    meta: Meta,
    /// Pages holding the persisted free list (reachable from `meta`).
    free_pages: Vec<u64>,
    free: Vec<Free>,
    /// Open read snapshots: commit id -> how many readers hold it.
    readers: BTreeMap<u64, usize>,
    poisoned: bool,
    /// Bumped by every commit that changes table definitions or the set of
    /// keyspaces.
    schema_generation: u64,
}

struct Shared {
    store: Box<dyn Storage>,
    cache: Cache,
    state: Mutex<State>,
    /// Held for the life of a write transaction.
    writer: Mutex<()>,
    /// Table definitions shared by transactions, valid for one schema
    /// generation.
    schemas: Mutex<SharedSchemas>,
    /// Parsed SQL by text.
    stmts: sql::StmtCache,
}

#[derive(Default)]
struct SharedSchemas {
    generation: u64,
    map: Arc<SchemaMap>,
}

impl Shared {
    /// The shared definitions, if they belong to `generation`.
    fn schema_base(&self, generation: u64) -> Option<Arc<SchemaMap>> {
        let s = lock(&self.schemas);
        (s.generation == generation).then(|| s.map.clone())
    }

    /// Fold definitions a transaction looked up back into the shared map.
    fn learn_schemas(&self, generation: u64, cache: &mut SchemaCache) {
        let Some((base, learned)) = cache.learned() else {
            return;
        };
        let mut s = lock(&self.schemas);
        if s.generation > generation {
            return;
        }
        let mut map = if s.generation == generation {
            match base {
                // Nothing else was added since this transaction started.
                Some(b) if Arc::ptr_eq(&b, &s.map) => b,
                _ => s.map.clone(),
            }
        } else {
            Arc::new(SchemaMap::new())
        };
        Arc::make_mut(&mut map).extend(learned);
        s.generation = generation;
        s.map = map;
    }
}

impl Shared {
    /// Read a committed page. `limit` is the page count of the snapshot.
    fn read_page(&self, id: u64, limit: u64) -> Result<Page> {
        if id < 2 || id >= limit {
            return corrupt(format!("page {id} is outside the file ({limit} pages)"));
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

fn get_value<R: PageRead>(r: &mut R, root: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
    match tree::get(r, root, key)? {
        None => Ok(None),
        Some(v) => Ok(Some(tree::read_value(r, v)?)),
    }
}

fn list_keyspaces<R: PageRead>(r: &mut R, dir_root: u64) -> Result<Vec<String>> {
    let mut raw = Vec::new();
    tree::scan(r, dir_root, &[], None, usize::MAX, &mut raw)?;
    Ok(raw
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k).into_owned())
        .filter(|n| !sql::is_reserved(n))
        .collect())
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
/// All methods take `&self`, and the handle can be shared between threads.
/// One write transaction runs at a time. Readers never wait for it: each
/// read works on the last commit made before it started.
pub struct Db {
    shared: Shared,
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
        let shared = Shared {
            store,
            cache: Cache::new(opts.cache_pages),
            state: Mutex::new(State {
                meta,
                free_pages: Vec::new(),
                free: Vec::new(),
                readers: BTreeMap::new(),
                poisoned: false,
                schema_generation: 0,
            }),
            writer: Mutex::new(()),
            schemas: Mutex::new(SharedSchemas::default()),
            stmts: sql::StmtCache::default(),
        };
        // Load the free list. Nobody holds a snapshot yet, so every listed
        // page is free to reuse at once.
        let (mut free_pages, mut free) = (Vec::new(), Vec::new());
        let mut id = meta.free_head;
        while id != 0 {
            if free_pages.len() as u64 > meta.page_count {
                return corrupt("free list loops");
            }
            let p = shared.read_page(id, meta.page_count)?;
            if page::page_type(&p) != PageType::Free as u8 {
                return corrupt("free list points at a non-free-list page");
            }
            let n = get_u32(&p, HEADER + 8) as usize;
            if n > FREE_PER_PAGE {
                return corrupt("free list page overfull");
            }
            for j in 0..n {
                free.push(Free {
                    id: get_u64(&p, FREE_HDR + 8 * j),
                    freed_at: 0,
                });
            }
            free_pages.push(id);
            id = get_u64(&p, HEADER);
        }
        if free.len() as u64 != meta.free_count {
            return corrupt("free list length does not match meta page");
        }
        {
            let mut st = lock(&shared.state);
            st.free_pages = free_pages;
            st.free = free;
        }
        Ok(Db { shared })
    }

    /// Id of the last committed transaction.
    pub fn txid(&self) -> u64 {
        lock(&self.shared.state).meta.txid
    }

    /// Start a read transaction: a stable view of the last commit. It sees
    /// nothing committed after it began. Pages it can reach are not reused
    /// while it lives, so long-lived readers make the file grow.
    pub fn begin_read(&self) -> Result<ReadTx<'_>> {
        let mut st = lock(&self.shared.state);
        if st.poisoned {
            return Err(Error::Poisoned);
        }
        let meta = st.meta;
        let schema_generation = st.schema_generation;
        *st.readers.entry(meta.txid).or_insert(0) += 1;
        drop(st);
        Ok(ReadTx {
            shared: &self.shared,
            meta,
            schema_generation,
            // Filled on first use, so plain key reads skip the shared lock.
            schemas: None,
        })
    }

    pub fn get(&self, keyspace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.begin_read()?.get(keyspace, key)
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
        self.begin_read()?.scan(keyspace, from, to, limit)
    }

    /// Names of all keyspaces, in byte order.
    pub fn keyspaces(&self) -> Result<Vec<String>> {
        self.begin_read()?.keyspaces()
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

    /// Run SQL in its own transaction. Read-only statements run on a
    /// snapshot; anything else runs in one write transaction that commits
    /// only if every statement succeeds. `?` placeholders take `params`.
    pub fn execute(&self, sql_text: &str, params: &[SqlValue]) -> Result<QueryResult> {
        let parsed = self.shared.stmts.parse(sql_text)?;
        if parsed.read_only() {
            return sql::query(&mut self.begin_read()?, &parsed, params);
        }
        let mut tx = self.begin_write()?;
        let r = sql::execute(&mut tx, &parsed, params)?;
        tx.commit()?;
        Ok(r)
    }

    /// KV door for tables: the row with this primary key, read straight from
    /// storage with no SQL parsing.
    pub fn get_row(&self, table: &str, pk: &SqlValue) -> Result<Option<Vec<SqlValue>>> {
        self.begin_read()?.get_row(table, pk)
    }

    /// Start a write transaction. Waits while another one is open. Nothing
    /// reaches the file until `commit`; dropping the transaction discards it.
    pub fn begin_write(&self) -> Result<WriteTx<'_>> {
        let guard = lock(&self.shared.writer);
        let st = lock(&self.shared.state);
        if st.poisoned {
            return Err(Error::Poisoned);
        }
        // Pages freed by commit T are safe once every reader is at T or later.
        let oldest = st.readers.keys().next().copied().unwrap_or(u64::MAX);
        let (mut reusable, mut held) = (Vec::new(), Vec::new());
        for f in &st.free {
            if f.freed_at <= oldest {
                reusable.push(f.id);
            } else {
                held.push(*f);
            }
        }
        reusable.sort_unstable_by(|a, b| b.cmp(a));
        let base = st.meta;
        let old_free_pages = st.free_pages.clone();
        let schema_generation = st.schema_generation;
        drop(st);
        let schemas = SchemaCache::with_base(self.shared.schema_base(schema_generation));
        Ok(WriteTx {
            shared: &self.shared,
            _guard: guard,
            base,
            old_free_pages,
            dirty: IdMap::default(),
            fresh: IdSet::default(),
            reusable,
            held,
            pending: Vec::new(),
            page_count: base.page_count,
            dir_root: base.dir_root,
            roots: HashMap::new(),
            changed: Vec::new(),
            schema_generation,
            schemas,
            savepoint: None,
        })
    }

    /// Check every page and invariant in the file. Waits for any write
    /// transaction to finish and holds off new ones while it runs.
    pub fn verify(&self) -> Result<VerifyReport> {
        let _w = lock(&self.shared.writer);
        let (meta, free_pages, free_list) = {
            let st = lock(&self.shared.state);
            if st.poisoned {
                return Err(Error::Poisoned);
            }
            (st.meta, st.free_pages.clone(), st.free.clone())
        };
        let mut r = ReadTx {
            shared: &self.shared,
            meta,
            schema_generation: u64::MAX,
            schemas: Some(SchemaCache::default()),
        };
        *lock(&self.shared.state)
            .readers
            .entry(meta.txid)
            .or_insert(0) += 1;
        let mut seen: HashSet<u64> = HashSet::from([0, 1]);
        let mut dir_stats = TreeStats::default();
        tree::verify(&mut r, meta.dir_root, &mut seen, &mut dir_stats)?;
        let mut entries = Vec::new();
        tree::scan(&mut r, meta.dir_root, &[], None, usize::MAX, &mut entries)?;
        let mut keys = 0;
        for (name, v) in &entries {
            let root = match v {
                Val::Inline(b) if b.len() == 8 => {
                    u64::from_le_bytes(b.as_slice().try_into().unwrap())
                }
                _ => return corrupt("directory entry is not a page id"),
            };
            let mut st = TreeStats::default();
            tree::verify(&mut r, root, &mut seen, &mut st).map_err(|e| {
                Error::Corrupt(format!("keyspace {:?}: {e}", String::from_utf8_lossy(name)))
            })?;
            keys += st.keys;
        }
        for &fp in &free_pages {
            if !seen.insert(fp) {
                return corrupt(format!("free list page {fp} is also in use"));
            }
        }
        let mut free = HashSet::new();
        for f in &free_list {
            if f.id < 2 || f.id >= meta.page_count {
                return corrupt(format!("free page {} is outside the file", f.id));
            }
            if seen.contains(&f.id) {
                return corrupt(format!("page {} is both free and in use", f.id));
            }
            if !free.insert(f.id) {
                return corrupt(format!("page {} is listed free twice", f.id));
            }
        }
        sql::verify(&mut r)?;
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

/// A read transaction. See [`Db::begin_read`].
pub struct ReadTx<'a> {
    shared: &'a Shared,
    meta: Meta,
    schema_generation: u64,
    schemas: Option<SchemaCache>,
}

impl PageRead for ReadTx<'_> {
    fn read(&mut self, id: u64) -> Result<Page> {
        self.shared.read_page(id, self.meta.page_count)
    }
}

impl Drop for ReadTx<'_> {
    fn drop(&mut self) {
        if let (Some(cache), true) = (&mut self.schemas, self.schema_generation != u64::MAX) {
            self.shared.learn_schemas(self.schema_generation, cache);
        }
        let mut st = lock(&self.shared.state);
        if let Some(n) = st.readers.get_mut(&self.meta.txid) {
            *n -= 1;
            if *n == 0 {
                st.readers.remove(&self.meta.txid);
            }
        }
    }
}

impl sql::Kv for ReadTx<'_> {
    fn kv_get(&mut self, ks: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get(ks, key)
    }
    fn kv_scan(
        &mut self,
        ks: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan(ks, from, to, limit)
    }
    fn kv_exists(&mut self, ks: &str) -> Result<bool> {
        let dir = self.meta.dir_root;
        Ok(lookup_root(self, dir, ks.as_bytes())?.is_some())
    }
    fn kv_last_key(&mut self, ks: &str) -> Result<Option<Vec<u8>>> {
        let dir = self.meta.dir_root;
        match lookup_root(self, dir, ks.as_bytes())? {
            Some(root) => tree::last_key(self, root),
            None => Ok(None),
        }
    }
    fn schemas(&mut self) -> &mut SchemaCache {
        let (shared, generation) = (self.shared, self.schema_generation);
        self.schemas
            .get_or_insert_with(|| SchemaCache::with_base(shared.schema_base(generation)))
    }
}

impl ReadTx<'_> {
    /// Run SELECT statements on this snapshot.
    pub fn query(&mut self, sql_text: &str, params: &[SqlValue]) -> Result<QueryResult> {
        let parsed = self.shared.stmts.parse(sql_text)?;
        sql::query(self, &parsed, params)
    }

    /// KV door for tables: see [`Db::get_row`].
    pub fn get_row(&mut self, table: &str, pk: &SqlValue) -> Result<Option<Vec<SqlValue>>> {
        sql::get_row(self, table, pk)
    }

    /// The commit this transaction sees.
    pub fn txid(&self) -> u64 {
        self.meta.txid
    }

    pub fn get(&mut self, keyspace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let dir = self.meta.dir_root;
        let Some(root) = lookup_root(self, dir, keyspace.as_bytes())? else {
            return Ok(None);
        };
        get_value(self, root, key)
    }

    /// Same as [`Db::scan`], on this snapshot.
    pub fn scan(
        &mut self,
        keyspace: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let dir = self.meta.dir_root;
        let Some(root) = lookup_root(self, dir, keyspace.as_bytes())? else {
            return Ok(Vec::new());
        };
        collect(self, root, from, to, limit)
    }

    pub fn keyspaces(&mut self) -> Result<Vec<String>> {
        let dir = self.meta.dir_root;
        list_keyspaces(self, dir)
    }
}

/// A write transaction. See [`Db::begin_write`].
pub struct WriteTx<'a> {
    shared: &'a Shared,
    _guard: MutexGuard<'a, ()>,
    base: Meta,
    old_free_pages: Vec<u64>,
    /// New page bodies, not yet sealed or in the file.
    dirty: IdMap<Page>,
    /// Pages allocated by this transaction. No committed state reaches them,
    /// so they can be freed and reused at once.
    fresh: IdSet,
    /// Free pages safe to overwrite now.
    reusable: Vec<u64>,
    /// Free pages an open reader may still reach.
    held: Vec<Free>,
    /// Committed pages this transaction stopped using. The last commit, and
    /// readers on it, still reach them.
    pending: Vec<u64>,
    page_count: u64,
    dir_root: u64,
    /// Keyspace roots seen or changed here; `None` means dropped or absent.
    roots: HashMap<Vec<u8>, Option<u64>>,
    changed: Vec<Vec<u8>>,
    schema_generation: u64,
    schemas: SchemaCache,
    /// State at the start of the current SQL statement.
    savepoint: Option<Box<Savepoint>>,
}

/// How to undo a statement: the small parts of the transaction state as
/// they were at its start, and a log of page-level changes since then.
/// Undoing replays the log backwards, so the cost follows the size of the
/// statement and not the size of the transaction.
struct Savepoint {
    page_count: u64,
    dir_root: u64,
    roots: HashMap<Vec<u8>, Option<u64>>,
    changed: Vec<Vec<u8>>,
    log: Vec<Undo>,
}

enum Undo {
    /// A page was allocated, from the reusable list or by growing the file.
    Alloc { id: u64, reused: bool },
    /// A page from this transaction was freed and went back on the reusable list.
    FreeFresh { id: u64, page: Option<Page> },
    /// A committed page was freed and went on the pending list.
    FreeCommitted,
    /// A page body was written; this was the body before.
    Write { id: u64, prev: Option<Page> },
}

impl PageRead for WriteTx<'_> {
    fn read(&mut self, id: u64) -> Result<Page> {
        if let Some(p) = self.dirty.get(&id) {
            return Ok(p.clone());
        }
        self.shared.read_page(id, self.base.page_count)
    }
}

impl PageWrite for WriteTx<'_> {
    fn alloc(&mut self) -> u64 {
        let (id, reused) = match self.reusable.pop() {
            Some(id) => (id, true),
            None => {
                self.page_count += 1;
                (self.page_count - 1, false)
            }
        };
        self.fresh.insert(id);
        if let Some(sp) = &mut self.savepoint {
            sp.log.push(Undo::Alloc { id, reused });
        }
        id
    }

    fn free(&mut self, id: u64) {
        if self.fresh.remove(&id) {
            let page = self.dirty.remove(&id);
            self.reusable.push(id);
            if let Some(sp) = &mut self.savepoint {
                sp.log.push(Undo::FreeFresh { id, page });
            }
        } else {
            self.pending.push(id);
            if let Some(sp) = &mut self.savepoint {
                sp.log.push(Undo::FreeCommitted);
            }
        }
    }

    fn write(&mut self, id: u64, page: Vec<u8>) {
        debug_assert!(
            self.fresh.contains(&id),
            "write to page {id} not allocated here"
        );
        // Sealed once at commit; a page can be rewritten many times before then.
        let prev = self.dirty.insert(id, page.into());
        if let Some(sp) = &mut self.savepoint {
            sp.log.push(Undo::Write { id, prev });
        }
    }
}

impl sql::Kv for WriteTx<'_> {
    fn kv_get(&mut self, ks: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get(ks, key)
    }
    fn kv_scan(
        &mut self,
        ks: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan(ks, from, to, limit)
    }
    fn kv_exists(&mut self, ks: &str) -> Result<bool> {
        Ok(self.root_of(ks.as_bytes())?.is_some())
    }
    fn kv_last_key(&mut self, ks: &str) -> Result<Option<Vec<u8>>> {
        match self.root_of(ks.as_bytes())? {
            Some(root) => tree::last_key(self, root),
            None => Ok(None),
        }
    }
    fn schemas(&mut self) -> &mut SchemaCache {
        &mut self.schemas
    }
}

impl sql::KvWrite for WriteTx<'_> {
    fn kv_put(&mut self, ks: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.raw_put(ks, key, value)
    }
    fn kv_delete(&mut self, ks: &str, key: &[u8]) -> Result<bool> {
        self.raw_delete(ks, key)
    }
    fn kv_create(&mut self, ks: &str) -> Result<()> {
        check_name(ks)?;
        if self.root_of(ks.as_bytes())?.is_none() {
            self.set_root(ks.as_bytes(), Some(0));
        }
        Ok(())
    }
    fn kv_drop(&mut self, ks: &str) -> Result<bool> {
        self.raw_drop(ks)
    }
    fn mark(&mut self) {
        WriteTx::mark(self)
    }
    fn rollback(&mut self) {
        WriteTx::rollback(self)
    }
    fn release(&mut self) {
        WriteTx::release(self)
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
        let old = self.roots.insert(name.to_vec(), root);
        if old.map(|o| o.is_some()) != Some(root.is_some()) {
            // A keyspace appeared or went away: plain-keyspace tables change.
            self.schemas.clear();
        }
        if !self.changed.iter().any(|n| n == name) {
            self.changed.push(name.to_vec());
        }
    }

    pub fn get(&mut self, keyspace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(root) = self.root_of(keyspace.as_bytes())? else {
            return Ok(None);
        };
        get_value(self, root, key)
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
    ///
    /// When the keyspace is a SQL table, `value` must be an encoded row
    /// (see [`crate::sql::encode_row`]) whose primary key matches `key`; the
    /// table's indexes are updated in the same transaction.
    pub fn put(&mut self, keyspace: &str, key: &[u8], value: &[u8]) -> Result<()> {
        check_name(keyspace)?;
        check_key(key)?;
        if sql::raw_put_hook(self, keyspace, key, value)? {
            return Ok(());
        }
        self.raw_put(keyspace, key, value)
    }

    pub(crate) fn raw_put(&mut self, keyspace: &str, key: &[u8], value: &[u8]) -> Result<()> {
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

    /// Remove a key. Returns whether it existed. Deleting from a SQL table
    /// also removes the row's index entries.
    pub fn delete(&mut self, keyspace: &str, key: &[u8]) -> Result<bool> {
        if let Some(found) = sql::raw_delete_hook(self, keyspace, key)? {
            return Ok(found);
        }
        self.raw_delete(keyspace, key)
    }

    pub(crate) fn raw_delete(&mut self, keyspace: &str, key: &[u8]) -> Result<bool> {
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
    /// SQL tables are dropped with DROP TABLE instead.
    pub fn drop_keyspace(&mut self, keyspace: &str) -> Result<bool> {
        sql::raw_drop_hook(self, keyspace)?;
        self.schemas.clear();
        self.raw_drop(keyspace)
    }

    pub(crate) fn raw_drop(&mut self, keyspace: &str) -> Result<bool> {
        let name = keyspace.as_bytes();
        let Some(root) = self.root_of(name)? else {
            return Ok(false);
        };
        tree::free_tree(self, root)?;
        self.set_root(name, None);
        Ok(true)
    }

    /// Names of all keyspaces as this transaction sees them.
    pub fn keyspaces(&mut self) -> Result<Vec<String>> {
        let dir = self.dir_root;
        let mut names: Vec<String> = list_keyspaces(self, dir)?;
        for (name, root) in &self.roots {
            let n = String::from_utf8_lossy(name).into_owned();
            if sql::is_reserved(&n) {
                continue;
            }
            match root {
                Some(_) if !names.contains(&n) => names.push(n),
                None => names.retain(|x| *x != n),
                _ => {}
            }
        }
        names.sort();
        Ok(names)
    }

    /// Run SQL statements in this transaction. A statement that fails is
    /// undone on its own; earlier statements stay, and the transaction can
    /// go on. `?` placeholders take `params`.
    pub fn execute(&mut self, sql_text: &str, params: &[SqlValue]) -> Result<QueryResult> {
        let parsed = self.shared.stmts.parse(sql_text)?;
        sql::execute(self, &parsed, params)
    }

    /// KV door for tables: see [`Db::get_row`].
    pub fn get_row(&mut self, table: &str, pk: &SqlValue) -> Result<Option<Vec<SqlValue>>> {
        sql::get_row(self, table, pk)
    }

    /// KV door for tables: insert or replace a whole row by its primary key,
    /// with no SQL parsing. Values are checked against the column types and
    /// the table's indexes are kept up to date.
    pub fn put_row(&mut self, table: &str, row: &[SqlValue]) -> Result<()> {
        self.mark();
        let r = sql::put_row(self, table, row.to_vec());
        if r.is_err() {
            self.rollback();
        } else {
            self.release();
        }
        r
    }

    /// KV door for tables: delete a row by primary key. Returns whether it
    /// existed.
    pub fn delete_row(&mut self, table: &str, pk: &SqlValue) -> Result<bool> {
        self.mark();
        let r = sql::delete_row(self, table, pk);
        if r.is_err() {
            self.rollback();
        } else {
            self.release();
        }
        r
    }

    fn mark(&mut self) {
        self.savepoint = Some(Box::new(Savepoint {
            page_count: self.page_count,
            dir_root: self.dir_root,
            roots: self.roots.clone(),
            changed: self.changed.clone(),
            log: Vec::new(),
        }));
    }

    /// The statement since the last `mark` succeeded; stop logging.
    fn release(&mut self) {
        self.savepoint = None;
    }

    fn rollback(&mut self) {
        if let Some(sp) = self.savepoint.take() {
            let sp = *sp;
            for u in sp.log.into_iter().rev() {
                match u {
                    Undo::Alloc { id, reused } => {
                        self.fresh.remove(&id);
                        self.dirty.remove(&id);
                        if reused {
                            self.reusable.push(id);
                        }
                    }
                    Undo::FreeFresh { id, page } => {
                        let back = self.reusable.pop();
                        debug_assert_eq!(back, Some(id));
                        self.fresh.insert(id);
                        if let Some(p) = page {
                            self.dirty.insert(id, p);
                        }
                    }
                    Undo::FreeCommitted => {
                        self.pending.pop();
                    }
                    Undo::Write { id, prev } => match prev {
                        Some(p) => {
                            self.dirty.insert(id, p);
                        }
                        None => {
                            self.dirty.remove(&id);
                        }
                    },
                }
            }
            self.page_count = sp.page_count;
            self.dir_root = sp.dir_root;
            self.roots = sp.roots;
            self.changed = sp.changed;
        }
        self.schemas.clear();
    }

    /// Make every change durable, all at once.
    pub fn commit(mut self) -> Result<()> {
        if self.changed.is_empty() && self.dirty.is_empty() && self.pending.is_empty() {
            return Ok(());
        }
        // Table definitions and plain-keyspace tables depend on the catalog
        // and on which keyspaces exist. Did this commit change either?
        let mut schema_changed = false;
        let base_dir = self.base.dir_root;
        for name in self.changed.clone() {
            let before = base_dir != 0 && lookup_root(&mut self, base_dir, &name)?.is_some();
            let after = self.roots[&name].is_some();
            if before != after || name.as_slice() == sql::CATALOG.as_bytes() {
                schema_changed = true;
                break;
            }
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
        // Readers never touch them, but the last commit does.
        let old_free_pages = std::mem::take(&mut self.old_free_pages);
        self.pending.extend(old_free_pages);
        let txid = self.base.txid + 1;

        // Pick pages for the new free list from pages that are safe to
        // overwrite now. Taking a page shrinks the list, so loop.
        let mut list_pages = Vec::new();
        loop {
            let total = self.reusable.len() + self.held.len() + self.pending.len();
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
        let mut free: Vec<Free> = self
            .reusable
            .iter()
            .map(|&id| Free { id, freed_at: 0 })
            .chain(self.held.iter().copied())
            .chain(self.pending.iter().map(|&id| Free { id, freed_at: txid }))
            .collect();
        free.sort_unstable_by_key(|f| f.id);
        let ids: Vec<u64> = free.iter().map(|f| f.id).collect();
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
            txid,
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
        let store = &self.shared.store;
        let result = (|| -> Result<()> {
            // Pages with consecutive ids go out in one write.
            let mut all: Vec<(u64, &[u8])> = writes
                .iter()
                .map(|(id, p)| (*id, &p[..]))
                .chain(list_bodies.iter().map(|(id, p)| (*id, &p[..])))
                .collect();
            all.sort_unstable_by_key(|(id, _)| *id);
            let mut run: Vec<u8> = Vec::new();
            let mut run_start = 0u64;
            let mut run_pages = 0u64;
            for (id, p) in all {
                if run_pages > 0 && (id != run_start + run_pages || run_pages >= 256) {
                    store.write_at(run_start * PAGE_SIZE as u64, &run)?;
                    run.clear();
                    run_pages = 0;
                }
                if run_pages == 0 {
                    run_start = id;
                }
                run.extend_from_slice(p);
                run_pages += 1;
            }
            if run_pages > 0 {
                store.write_at(run_start * PAGE_SIZE as u64, &run)?;
            }
            store.sync()?;
            let slot = meta.txid % 2;
            store.write_at(slot * PAGE_SIZE as u64, &meta.encode(slot))?;
            store.sync()?;
            Ok(())
        })();
        if let Err(e) = result {
            // The file may hold a partial commit. Reopening recovers the last
            // good state; this handle must not guess.
            lock(&self.shared.state).poisoned = true;
            return Err(e);
        }

        // No reader can reach a page written here, so the cache can take
        // the new contents before the new meta is visible to readers.
        for (id, p) in writes {
            self.shared.cache.insert(id, p);
        }
        for (id, _) in &list_bodies {
            self.shared.cache.remove(*id);
        }
        let mut st = lock(&self.shared.state);
        st.meta = meta;
        st.free_pages = list_pages;
        st.free = free;
        if schema_changed {
            st.schema_generation += 1;
        }
        drop(st);
        if !schema_changed {
            self.shared
                .learn_schemas(self.schema_generation, &mut self.schemas);
        }
        Ok(())
    }
}
