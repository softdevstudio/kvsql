//! Copy-on-write B+tree.
//!
//! Committed pages are never changed in place. A write copies every page on
//! the path from the root to the leaf it touches, so the last committed tree
//! stays whole until the new root is published in a meta page.
//!
//! Leaf and branch pages share one layout after the page header:
//!
//! ```text
//! 16..18  cell count
//! 18..20  reserved
//! 20..    one u16 offset per cell, then the cells
//! leaf cell:   key_len u16 | flag u8 | value_len u32 | key | value or overflow page u64
//! branch cell: key_len u16 | child u64 | key
//! ```
//!
//! The first cell of a branch has an empty key and covers everything below the
//! second cell's key. Child `i` holds keys in `[key_i, key_{i+1})`.

use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{corrupt, Result};
use crate::page::{
    get_u16, get_u32, get_u64, new_page, page_type, put_u16, put_u32, put_u64, PageType, HEADER,
    PAGE_SIZE,
};

pub type Page = Arc<[u8]>;

/// Longest key accepted, in bytes. Keyspace names follow the same limit.
pub const MAX_KEY_LEN: usize = 512;
/// Longest value accepted, in bytes.
pub const MAX_VALUE_LEN: usize = 1 << 30;

const NODE_HDR: usize = HEADER + 4;
const CAPACITY: usize = PAGE_SIZE - NODE_HDR;
/// A leaf cell bigger than this moves its value to overflow pages.
const MAX_INLINE_CELL: usize = 1000;
/// A non-root node smaller than this is merged with a neighbour.
const UNDERFULL: usize = CAPACITY / 4;

const OVF_HDR: usize = HEADER + 12;
const OVF_DATA: usize = PAGE_SIZE - OVF_HDR;

/// Committed or in-transaction page reads.
pub trait PageRead {
    fn read(&mut self, id: u64) -> Result<Page>;
}

/// Page allocation for a write transaction.
pub trait PageWrite: PageRead {
    fn alloc(&mut self) -> u64;
    fn free(&mut self, id: u64);
    /// Store a finished page body. The implementation seals it before it
    /// reaches the file.
    fn write(&mut self, id: u64, page: Vec<u8>);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    Inline(Vec<u8>),
    Overflow { page: u64, len: u32 },
}

#[derive(Clone, Debug)]
struct LeafCell {
    key: Vec<u8>,
    val: Val,
}

#[derive(Clone, Debug)]
struct BranchCell {
    key: Vec<u8>,
    child: u64,
}

enum Node {
    Leaf(Vec<LeafCell>),
    Branch(Vec<BranchCell>),
}

fn leaf_cell_size(c: &LeafCell) -> usize {
    2 + 7
        + c.key.len()
        + match &c.val {
            Val::Inline(v) => v.len(),
            Val::Overflow { .. } => 8,
        }
}

fn branch_cell_size(c: &BranchCell) -> usize {
    2 + 10 + c.key.len()
}

/// True when a key/value pair is stored inline in its leaf.
pub fn fits_inline(key_len: usize, val_len: usize) -> bool {
    7 + key_len + val_len <= MAX_INLINE_CELL
}

// ---------------------------------------------------------------------------
// Raw page access with bounds checks, so a damaged page fails instead of panicking.

fn slice(p: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    match at.checked_add(len) {
        Some(end) if end <= p.len() => Ok(&p[at..end]),
        _ => corrupt("cell runs past the end of its page"),
    }
}

fn n_cells(p: &[u8]) -> Result<usize> {
    let n = get_u16(p, HEADER) as usize;
    if NODE_HDR + 2 * n > PAGE_SIZE {
        return corrupt("cell count too large for page");
    }
    Ok(n)
}

fn cell_at(p: &[u8], i: usize) -> Result<usize> {
    let o = get_u16(p, NODE_HDR + 2 * i) as usize;
    if !(NODE_HDR..PAGE_SIZE).contains(&o) {
        return corrupt("cell offset out of range");
    }
    Ok(o)
}

fn leaf_key(p: &[u8], i: usize) -> Result<&[u8]> {
    let o = cell_at(p, i)?;
    let k = get_u16(slice(p, o, 2)?, 0) as usize;
    slice(p, o + 7, k)
}

fn leaf_val(p: &[u8], i: usize) -> Result<Val> {
    let o = cell_at(p, i)?;
    let h = slice(p, o, 7)?;
    let k = get_u16(h, 0) as usize;
    let flag = h[2];
    let vlen = get_u32(h, 3) as usize;
    match flag {
        0 => Ok(Val::Inline(slice(p, o + 7 + k, vlen)?.to_vec())),
        1 => Ok(Val::Overflow {
            page: get_u64(slice(p, o + 7 + k, 8)?, 0),
            len: vlen as u32,
        }),
        _ => corrupt("unknown leaf value flag"),
    }
}

fn branch_key(p: &[u8], i: usize) -> Result<&[u8]> {
    let o = cell_at(p, i)?;
    let k = get_u16(slice(p, o, 2)?, 0) as usize;
    slice(p, o + 10, k)
}

fn branch_child(p: &[u8], i: usize) -> Result<u64> {
    let o = cell_at(p, i)?;
    Ok(get_u64(slice(p, o + 2, 8)?, 0))
}

fn kind(p: &[u8]) -> Result<PageType> {
    match page_type(p) {
        2 => Ok(PageType::Branch),
        3 => Ok(PageType::Leaf),
        t => corrupt(format!("expected a tree page, found type {t}")),
    }
}

/// Index of the child covering `key`: the last cell whose key is <= `key`.
fn branch_find(p: &[u8], n: usize, key: &[u8]) -> Result<usize> {
    let (mut lo, mut hi) = (1usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if branch_key(p, mid)? <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo - 1)
}

/// First leaf index whose key is >= `key`, and whether it is equal.
fn leaf_find(p: &[u8], n: usize, key: &[u8]) -> Result<(usize, bool)> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if leaf_key(p, mid)? < key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let eq = lo < n && leaf_key(p, lo)? == key;
    Ok((lo, eq))
}

/// Bytes a cell takes in its page, not counting its offset slot.
fn cell_len(p: &[u8], i: usize) -> Result<usize> {
    let o = cell_at(p, i)?;
    let h = slice(p, o, 7)?;
    let k = get_u16(h, 0) as usize;
    Ok(match page_type(p) {
        3 => 7 + k + if h[2] == 0 { get_u32(h, 3) as usize } else { 8 },
        _ => 10 + k,
    })
}

fn raw_cell(p: &[u8], i: usize) -> Result<&[u8]> {
    slice(p, cell_at(p, i)?, cell_len(p, i)?)
}

/// Bytes used after the node header: offset slots plus cells.
fn used(p: &[u8]) -> Result<usize> {
    let n = n_cells(p)?;
    let mut total = 2 * n;
    for i in 0..n {
        total += cell_len(p, i)?;
    }
    Ok(total)
}

fn leaf_cell_bytes(key: &[u8], val: &Val) -> Vec<u8> {
    let mut c = Vec::with_capacity(7 + key.len() + 8);
    c.extend_from_slice(&(key.len() as u16).to_le_bytes());
    match val {
        Val::Inline(v) => {
            c.push(0);
            c.extend_from_slice(&(v.len() as u32).to_le_bytes());
            c.extend_from_slice(key);
            c.extend_from_slice(v);
        }
        Val::Overflow { page, len } => {
            c.push(1);
            c.extend_from_slice(&len.to_le_bytes());
            c.extend_from_slice(key);
            c.extend_from_slice(&page.to_le_bytes());
        }
    }
    c
}

/// Build a leaf page from raw cells already in order.
fn build_leaf(cells: &[&[u8]]) -> Vec<u8> {
    let mut p = new_page(PageType::Leaf);
    put_u16(&mut p, HEADER, cells.len() as u16);
    let mut at = NODE_HDR + 2 * cells.len();
    for (i, c) in cells.iter().enumerate() {
        put_u16(&mut p, NODE_HDR + 2 * i, at as u16);
        p[at..at + c.len()].copy_from_slice(c);
        at += c.len();
    }
    p
}

fn decode(p: &[u8]) -> Result<Node> {
    let n = n_cells(p)?;
    match kind(p)? {
        PageType::Leaf => {
            let mut cells = Vec::with_capacity(n + 1);
            for i in 0..n {
                cells.push(LeafCell {
                    key: leaf_key(p, i)?.to_vec(),
                    val: leaf_val(p, i)?,
                });
            }
            Ok(Node::Leaf(cells))
        }
        _ => {
            let mut cells = Vec::with_capacity(n + 1);
            for i in 0..n {
                cells.push(BranchCell {
                    key: branch_key(p, i)?.to_vec(),
                    child: branch_child(p, i)?,
                });
            }
            Ok(Node::Branch(cells))
        }
    }
}

fn encode(node: &Node) -> Vec<u8> {
    match node {
        Node::Leaf(cells) => {
            let mut p = new_page(PageType::Leaf);
            put_u16(&mut p, HEADER, cells.len() as u16);
            let mut at = NODE_HDR + 2 * cells.len();
            for (i, c) in cells.iter().enumerate() {
                put_u16(&mut p, NODE_HDR + 2 * i, at as u16);
                put_u16(&mut p, at, c.key.len() as u16);
                match &c.val {
                    Val::Inline(v) => {
                        p[at + 2] = 0;
                        put_u32(&mut p, at + 3, v.len() as u32);
                        p[at + 7..at + 7 + c.key.len()].copy_from_slice(&c.key);
                        let vs = at + 7 + c.key.len();
                        p[vs..vs + v.len()].copy_from_slice(v);
                        at = vs + v.len();
                    }
                    Val::Overflow { page, len } => {
                        p[at + 2] = 1;
                        put_u32(&mut p, at + 3, *len);
                        p[at + 7..at + 7 + c.key.len()].copy_from_slice(&c.key);
                        let vs = at + 7 + c.key.len();
                        put_u64(&mut p, vs, *page);
                        at = vs + 8;
                    }
                }
            }
            p
        }
        Node::Branch(cells) => {
            let mut p = new_page(PageType::Branch);
            put_u16(&mut p, HEADER, cells.len() as u16);
            let mut at = NODE_HDR + 2 * cells.len();
            for (i, c) in cells.iter().enumerate() {
                put_u16(&mut p, NODE_HDR + 2 * i, at as u16);
                put_u16(&mut p, at, c.key.len() as u16);
                put_u64(&mut p, at + 2, c.child);
                p[at + 10..at + 10 + c.key.len()].copy_from_slice(&c.key);
                at += 10 + c.key.len();
            }
            p
        }
    }
}

/// Split points: each chunk fits a page. Balanced splits keep every chunk
/// near the same size. When keys arrive in order at the end of a node,
/// `fill_left` packs the earlier pages full instead, so a run of appends
/// leaves full pages behind it rather than half-empty ones.
fn chunk_bounds(sizes: &[usize], fill_left: bool) -> Vec<(usize, usize)> {
    let total: usize = sizes.iter().sum();
    if total <= CAPACITY {
        return vec![(0, sizes.len())];
    }
    let parts = total.div_ceil(CAPACITY);
    let target = if fill_left { CAPACITY } else { total / parts };
    let mut out = Vec::new();
    let (mut start, mut acc) = (0, 0);
    for (i, &s) in sizes.iter().enumerate() {
        if i > start && (acc + s > CAPACITY || acc >= target) {
            out.push((start, i));
            start = i;
            acc = 0;
        }
        acc += s;
    }
    out.push((start, sizes.len()));
    out
}

/// A page produced by a write: separator for the parent, page id, encoded size.
struct Written {
    sep: Vec<u8>,
    id: u64,
    size: usize,
}

/// Free `olds`, then write `node` into as many new pages as it needs.
fn write_node<W: PageWrite>(w: &mut W, olds: &[u64], node: Node) -> Vec<Written> {
    write_node_as(w, olds, node, false)
}

fn write_node_as<W: PageWrite>(
    w: &mut W,
    olds: &[u64],
    node: Node,
    fill_left: bool,
) -> Vec<Written> {
    for &o in olds {
        w.free(o);
    }
    let mut out = Vec::new();
    match node {
        Node::Leaf(cells) => {
            let sizes: Vec<usize> = cells.iter().map(leaf_cell_size).collect();
            let bounds = chunk_bounds(&sizes, fill_left);
            let mut cells = cells;
            for &(a, b) in bounds.iter().rev() {
                let chunk: Vec<LeafCell> = cells.drain(a..b).collect();
                let size = sizes[a..b].iter().sum();
                let sep = chunk[0].key.clone();
                let id = w.alloc();
                w.write(id, encode(&Node::Leaf(chunk)));
                out.push(Written { sep, id, size });
            }
        }
        Node::Branch(cells) => {
            let sizes: Vec<usize> = cells.iter().map(branch_cell_size).collect();
            let bounds = chunk_bounds(&sizes, fill_left);
            let mut cells = cells;
            for &(a, b) in bounds.iter().rev() {
                let mut chunk: Vec<BranchCell> = cells.drain(a..b).collect();
                let sep = std::mem::take(&mut chunk[0].key);
                let size = chunk.iter().map(branch_cell_size).sum::<usize>();
                let id = w.alloc();
                w.write(id, encode(&Node::Branch(chunk)));
                out.push(Written { sep, id, size });
            }
        }
    }
    out.reverse();
    out
}

/// Grow a new root above several sibling pages until one page holds them all.
fn new_root<W: PageWrite>(w: &mut W, mut pages: Vec<Written>) -> u64 {
    while pages.len() > 1 {
        let cells = pages
            .into_iter()
            .enumerate()
            .map(|(i, p)| BranchCell {
                key: if i == 0 { Vec::new() } else { p.sep },
                child: p.id,
            })
            .collect();
        pages = write_node(w, &[], Node::Branch(cells));
    }
    pages[0].id
}

// ---------------------------------------------------------------------------
// Lookups

pub fn get<R: PageRead>(r: &mut R, root: u64, key: &[u8]) -> Result<Option<Val>> {
    if root == 0 {
        return Ok(None);
    }
    let mut id = root;
    for _ in 0..64 {
        let p = r.read(id)?;
        let n = n_cells(&p)?;
        match kind(&p)? {
            PageType::Leaf => {
                let (i, eq) = leaf_find(&p, n, key)?;
                return if eq {
                    Ok(Some(leaf_val(&p, i)?))
                } else {
                    Ok(None)
                };
            }
            _ => {
                if n == 0 {
                    return corrupt("empty branch page");
                }
                id = branch_child(&p, branch_find(&p, n, key)?)?;
            }
        }
    }
    corrupt("tree deeper than 64 levels")
}

/// Collect pairs with `from <= key < to`, at most `limit` of them.
pub fn scan<R: PageRead>(
    r: &mut R,
    root: u64,
    from: &[u8],
    to: Option<&[u8]>,
    limit: usize,
    out: &mut Vec<(Vec<u8>, Val)>,
) -> Result<()> {
    if root == 0 || limit == 0 {
        return Ok(());
    }
    scan_rec(r, root, from, to, limit, out, 0).map(|_| ())
}

/// Returns true when the scan is finished (hit `to` or `limit`).
fn scan_rec<R: PageRead>(
    r: &mut R,
    id: u64,
    from: &[u8],
    to: Option<&[u8]>,
    limit: usize,
    out: &mut Vec<(Vec<u8>, Val)>,
    depth: usize,
) -> Result<bool> {
    if depth > 64 {
        return corrupt("tree deeper than 64 levels");
    }
    let p = r.read(id)?;
    let n = n_cells(&p)?;
    match kind(&p)? {
        PageType::Leaf => {
            let (start, _) = leaf_find(&p, n, from)?;
            for i in start..n {
                let k = leaf_key(&p, i)?;
                if let Some(t) = to {
                    if k >= t {
                        return Ok(true);
                    }
                }
                out.push((k.to_vec(), leaf_val(&p, i)?));
                if out.len() >= limit {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => {
            if n == 0 {
                return corrupt("empty branch page");
            }
            let start = branch_find(&p, n, from)?;
            for i in start..n {
                if i > start {
                    if let Some(t) = to {
                        if branch_key(&p, i)? >= t {
                            return Ok(true);
                        }
                    }
                }
                if scan_rec(r, branch_child(&p, i)?, from, to, limit, out, depth + 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Insert

/// Insert or replace. Returns the new root and the value it replaced.
/// The caller frees a replaced overflow chain.
pub fn insert<W: PageWrite>(
    w: &mut W,
    root: u64,
    key: &[u8],
    val: Val,
) -> Result<(u64, Option<Val>)> {
    if root == 0 {
        let pages = write_node(
            w,
            &[],
            Node::Leaf(vec![LeafCell {
                key: key.to_vec(),
                val,
            }]),
        );
        return Ok((new_root(w, pages), None));
    }
    let (pages, old) = ins(w, root, key, val, 0)?;
    Ok((new_root(w, pages), old))
}

fn ins<W: PageWrite>(
    w: &mut W,
    id: u64,
    key: &[u8],
    val: Val,
    depth: usize,
) -> Result<(Vec<Written>, Option<Val>)> {
    if depth > 64 {
        return corrupt("tree deeper than 64 levels");
    }
    let p = w.read(id)?;
    let n = n_cells(&p)?;
    if kind(&p)? == PageType::Leaf {
        let (pos, eq) = leaf_find(&p, n, key)?;
        let old = if eq { Some(leaf_val(&p, pos)?) } else { None };
        let cell = leaf_cell_bytes(key, &val);
        let size = used(&p)? + cell.len() + 2 - if eq { cell_len(&p, pos)? + 2 } else { 0 };
        if size <= CAPACITY {
            // Common case: copy raw cells into a new page, no decoding.
            let mut cells: Vec<&[u8]> = Vec::with_capacity(n + 1);
            for i in 0..pos {
                cells.push(raw_cell(&p, i)?);
            }
            cells.push(&cell);
            for i in pos + usize::from(eq)..n {
                cells.push(raw_cell(&p, i)?);
            }
            let body = build_leaf(&cells);
            w.free(id);
            let nid = w.alloc();
            w.write(nid, body);
            return Ok((
                vec![Written {
                    sep: Vec::new(),
                    id: nid,
                    size,
                }],
                old,
            ));
        }
        let Node::Leaf(mut cells) = decode(&p)? else {
            unreachable!()
        };
        let mut append = false;
        match cells.binary_search_by(|c| c.key.as_slice().cmp(key)) {
            Ok(i) => cells[i].val = val,
            Err(i) => {
                append = i == cells.len();
                cells.insert(
                    i,
                    LeafCell {
                        key: key.to_vec(),
                        val,
                    },
                )
            }
        }
        return Ok((write_node_as(w, &[id], Node::Leaf(cells), append), old));
    }
    if n == 0 {
        return corrupt("empty branch page");
    }
    let i = branch_find(&p, n, key)?;
    let child = branch_child(&p, i)?;
    let (kids, old) = ins(w, child, key, val, depth + 1)?;
    if kids.len() == 1 && kids[0].id == child {
        // The child was already rewritten in this transaction and kept its
        // id, so this page does not change.
        return Ok((
            vec![Written {
                sep: Vec::new(),
                id,
                size: used(&p)?,
            }],
            old,
        ));
    }
    let Node::Branch(mut cells) = decode(&p)? else {
        unreachable!()
    };
    let append = i == cells.len() - 1 && kids.len() > 1;
    splice(&mut cells, i, 1, kids);
    Ok((write_node_as(w, &[id], Node::Branch(cells), append), old))
}

/// Replace `count` cells at `at` with `pages`. The first page keeps the
/// separator already in the parent.
fn splice(cells: &mut Vec<BranchCell>, at: usize, count: usize, pages: Vec<Written>) {
    let keep = cells[at].key.clone();
    let new: Vec<BranchCell> = pages
        .into_iter()
        .enumerate()
        .map(|(j, p)| BranchCell {
            key: if j == 0 { keep.clone() } else { p.sep },
            child: p.id,
        })
        .collect();
    cells.splice(at..at + count, new);
}

// ---------------------------------------------------------------------------
// Delete

/// Remove `key`. Returns the new root (0 when the tree is empty) and the
/// removed value, or `None` when the key was absent and nothing changed.
pub fn delete<W: PageWrite>(w: &mut W, root: u64, key: &[u8]) -> Result<Option<(u64, Val)>> {
    if root == 0 {
        return Ok(None);
    }
    let Some((pages, old)) = del(w, root, key, 0)? else {
        return Ok(None);
    };
    if pages.is_empty() {
        return Ok(Some((0, old)));
    }
    let mut root = new_root(w, pages);
    // A branch root with one child is a wasted level.
    loop {
        let p = w.read(root)?;
        if kind(&p)? == PageType::Branch && n_cells(&p)? == 1 {
            let child = branch_child(&p, 0)?;
            w.free(root);
            root = child;
        } else {
            break;
        }
    }
    Ok(Some((root, old)))
}

#[allow(clippy::type_complexity)]
fn del<W: PageWrite>(
    w: &mut W,
    id: u64,
    key: &[u8],
    depth: usize,
) -> Result<Option<(Vec<Written>, Val)>> {
    if depth > 64 {
        return corrupt("tree deeper than 64 levels");
    }
    let p = w.read(id)?;
    let n = n_cells(&p)?;
    if kind(&p)? == PageType::Leaf {
        let (pos, eq) = leaf_find(&p, n, key)?;
        if !eq {
            return Ok(None);
        }
        let old = leaf_val(&p, pos)?;
        w.free(id);
        if n == 1 {
            return Ok(Some((Vec::new(), old)));
        }
        let size = used(&p)? - cell_len(&p, pos)? - 2;
        let mut cells: Vec<&[u8]> = Vec::with_capacity(n);
        for i in (0..n).filter(|&i| i != pos) {
            cells.push(raw_cell(&p, i)?);
        }
        let body = build_leaf(&cells);
        let nid = w.alloc();
        w.write(nid, body);
        return Ok(Some((
            vec![Written {
                sep: Vec::new(),
                id: nid,
                size,
            }],
            old,
        )));
    }
    if n == 0 {
        return corrupt("empty branch page");
    }
    let i = branch_find(&p, n, key)?;
    let child = branch_child(&p, i)?;
    let Some((kids, old)) = del(w, child, key, depth + 1)? else {
        return Ok(None);
    };
    let underfull = kids.len() == 1 && kids[0].size < UNDERFULL;
    if kids.len() == 1 && kids[0].id == child && !underfull {
        return Ok(Some((
            vec![Written {
                sep: Vec::new(),
                id,
                size: used(&p)?,
            }],
            old,
        )));
    }
    let Node::Branch(mut cells) = decode(&p)? else {
        unreachable!()
    };
    if kids.is_empty() {
        cells.remove(i);
        if i == 0 && !cells.is_empty() {
            cells[0].key.clear();
        }
    } else {
        splice(&mut cells, i, 1, kids);
    }
    if cells.is_empty() {
        w.free(id);
        return Ok(Some((Vec::new(), old)));
    }
    if underfull && cells.len() >= 2 {
        let left = if i + 1 < cells.len() { i } else { i - 1 };
        merge(w, &mut cells, left)?;
    }
    Ok(Some((write_node(w, &[id], Node::Branch(cells)), old)))
}

/// Combine children `left` and `left + 1` and write them back as one or two
/// balanced pages.
fn merge<W: PageWrite>(w: &mut W, cells: &mut Vec<BranchCell>, left: usize) -> Result<()> {
    let (a, b) = (cells[left].child, cells[left + 1].child);
    let pa = w.read(a)?;
    let pb = w.read(b)?;
    let combined = match (decode(&pa)?, decode(&pb)?) {
        (Node::Leaf(mut x), Node::Leaf(y)) => {
            x.extend(y);
            Node::Leaf(x)
        }
        (Node::Branch(mut x), Node::Branch(mut y)) => {
            // The right node's empty first key becomes the parent's separator.
            y[0].key = cells[left + 1].key.clone();
            x.extend(y);
            Node::Branch(x)
        }
        _ => return corrupt("siblings at different levels"),
    };
    let pages = write_node(w, &[a, b], combined);
    splice(cells, left, 2, pages);
    Ok(())
}

// ---------------------------------------------------------------------------
// Overflow values

pub fn write_overflow<W: PageWrite>(w: &mut W, data: &[u8]) -> u64 {
    let chunks: Vec<&[u8]> = data.chunks(OVF_DATA).collect();
    let ids: Vec<u64> = (0..chunks.len().max(1)).map(|_| w.alloc()).collect();
    for (i, &id) in ids.iter().enumerate() {
        let chunk = chunks.get(i).copied().unwrap_or(&[]);
        let mut p = new_page(PageType::Overflow);
        let next = ids.get(i + 1).copied().unwrap_or(0);
        put_u64(&mut p, HEADER, next);
        put_u32(&mut p, HEADER + 8, chunk.len() as u32);
        p[OVF_HDR..OVF_HDR + chunk.len()].copy_from_slice(chunk);
        w.write(id, p);
    }
    ids[0]
}

pub fn read_value<R: PageRead>(r: &mut R, v: Val) -> Result<Vec<u8>> {
    match v {
        Val::Inline(b) => Ok(b),
        Val::Overflow { page, len } => {
            if len as usize > MAX_VALUE_LEN {
                return corrupt("overflow value longer than the largest value");
            }
            // Grow as pages arrive; a damaged length must not reserve gigabytes.
            let mut out = Vec::with_capacity((len as usize).min(1 << 20));
            let mut id = page;
            while out.len() < len as usize {
                if id == 0 {
                    return corrupt("overflow chain ends early");
                }
                let p = r.read(id)?;
                if page_type(&p) != PageType::Overflow as u8 {
                    return corrupt("overflow chain points at a non-overflow page");
                }
                let n = get_u32(&p, HEADER + 8) as usize;
                if n == 0 {
                    return corrupt("empty overflow page");
                }
                out.extend_from_slice(slice(&p, OVF_HDR, n)?);
                id = get_u64(&p, HEADER);
            }
            if out.len() != len as usize {
                return corrupt("overflow chain longer than its value");
            }
            Ok(out)
        }
    }
}

pub fn free_value<W: PageWrite>(w: &mut W, v: &Val) -> Result<()> {
    if let Val::Overflow { page, .. } = v {
        let mut id = *page;
        let mut guard = 0u64;
        while id != 0 {
            let p = w.read(id)?;
            let next = get_u64(&p, HEADER);
            w.free(id);
            id = next;
            guard += 1;
            if guard > (MAX_VALUE_LEN / OVF_DATA + 2) as u64 {
                return corrupt("overflow chain loops");
            }
        }
    }
    Ok(())
}

/// Free every page of a tree, including overflow chains.
pub fn free_tree<W: PageWrite>(w: &mut W, root: u64) -> Result<()> {
    free_tree_rec(w, root, 0)
}

fn free_tree_rec<W: PageWrite>(w: &mut W, root: u64, depth: usize) -> Result<()> {
    if root == 0 {
        return Ok(());
    }
    if depth > 64 {
        return corrupt("tree deeper than 64 levels");
    }
    let p = w.read(root)?;
    match decode(&p)? {
        Node::Leaf(cells) => {
            for c in &cells {
                free_value(w, &c.val)?;
            }
        }
        Node::Branch(cells) => {
            for c in &cells {
                free_tree_rec(w, c.child, depth + 1)?;
            }
        }
    }
    w.free(root);
    Ok(())
}

// ---------------------------------------------------------------------------
// Verification

#[derive(Default, Debug)]
pub struct TreeStats {
    pub keys: u64,
    pub pages: u64,
}

/// Walk a whole tree and check every invariant: checksums (via `read`), key
/// order, separator bounds, equal leaf depth, no page used twice, and
/// overflow chain lengths. Every page seen is added to `seen`.
pub fn verify<R: PageRead>(
    r: &mut R,
    root: u64,
    seen: &mut HashSet<u64>,
    stats: &mut TreeStats,
) -> Result<()> {
    if root == 0 {
        return Ok(());
    }
    let mut leaf_depth = None;
    verify_rec(r, root, None, None, 0, true, &mut leaf_depth, seen, stats)
}

#[allow(clippy::too_many_arguments)]
fn verify_rec<R: PageRead>(
    r: &mut R,
    id: u64,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    depth: usize,
    is_root: bool,
    leaf_depth: &mut Option<usize>,
    seen: &mut HashSet<u64>,
    stats: &mut TreeStats,
) -> Result<()> {
    if depth > 64 {
        return corrupt("tree deeper than 64 levels");
    }
    if !seen.insert(id) {
        return corrupt(format!("page {id} is used twice"));
    }
    stats.pages += 1;
    let p = r.read(id)?;
    let node = decode(&p)?;
    let in_bounds = |k: &[u8]| lo.is_none_or(|l| k >= l) && hi.is_none_or(|h| k < h);
    match node {
        Node::Leaf(cells) => {
            if cells.is_empty() {
                return corrupt(format!("leaf {id} is empty"));
            }
            match leaf_depth {
                None => *leaf_depth = Some(depth),
                Some(d) if *d != depth => return corrupt("leaves at different depths"),
                _ => {}
            }
            for (j, c) in cells.iter().enumerate() {
                if c.key.is_empty() || c.key.len() > MAX_KEY_LEN {
                    return corrupt(format!("leaf {id}: bad key length"));
                }
                if j > 0 && cells[j - 1].key >= c.key {
                    return corrupt(format!("leaf {id}: keys out of order"));
                }
                if !in_bounds(&c.key) {
                    return corrupt(format!("leaf {id}: key outside parent bounds"));
                }
                match &c.val {
                    Val::Inline(v) => {
                        if !fits_inline(c.key.len(), v.len()) {
                            return corrupt(format!("leaf {id}: oversized inline value"));
                        }
                    }
                    Val::Overflow { page, len } => {
                        let mut pid = *page;
                        let mut total = 0usize;
                        while pid != 0 {
                            if !seen.insert(pid) {
                                return corrupt(format!("page {pid} is used twice"));
                            }
                            stats.pages += 1;
                            let op = r.read(pid)?;
                            if page_type(&op) != PageType::Overflow as u8 {
                                return corrupt("overflow chain points at a non-overflow page");
                            }
                            total += get_u32(&op, HEADER + 8) as usize;
                            pid = get_u64(&op, HEADER);
                        }
                        if total != *len as usize {
                            return corrupt(format!("leaf {id}: overflow length mismatch"));
                        }
                    }
                }
                stats.keys += 1;
            }
        }
        Node::Branch(cells) => {
            if cells.is_empty() {
                return corrupt(format!("branch {id} is empty"));
            }
            if is_root && cells.len() < 2 {
                return corrupt(format!("root branch {id} has one child"));
            }
            if !cells[0].key.is_empty() {
                return corrupt(format!("branch {id}: first key not empty"));
            }
            for j in 1..cells.len() {
                if j > 1 && cells[j - 1].key >= cells[j].key {
                    return corrupt(format!("branch {id}: keys out of order"));
                }
                if !in_bounds(&cells[j].key) {
                    return corrupt(format!("branch {id}: separator outside bounds"));
                }
            }
            for j in 0..cells.len() {
                let clo = if j == 0 {
                    lo
                } else {
                    Some(cells[j].key.as_slice())
                };
                let chi = if j + 1 < cells.len() {
                    Some(cells[j + 1].key.as_slice())
                } else {
                    hi
                };
                verify_rec(
                    r,
                    cells[j].child,
                    clo,
                    chi,
                    depth + 1,
                    false,
                    leaf_depth,
                    seen,
                    stats,
                )?;
            }
        }
    }
    Ok(())
}

/// The largest key in a tree.
pub fn last_key<R: PageRead>(r: &mut R, root: u64) -> Result<Option<Vec<u8>>> {
    if root == 0 {
        return Ok(None);
    }
    let mut id = root;
    for _ in 0..64 {
        let p = r.read(id)?;
        let n = n_cells(&p)?;
        if n == 0 {
            return corrupt("empty tree page");
        }
        match kind(&p)? {
            PageType::Leaf => return Ok(Some(leaf_key(&p, n - 1)?.to_vec())),
            _ => id = branch_child(&p, n - 1)?,
        }
    }
    corrupt("tree deeper than 64 levels")
}
