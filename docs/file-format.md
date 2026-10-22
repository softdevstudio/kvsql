# File format

A KvSQL file is a sequence of 4096-byte pages. All integers are little endian.

## Page header

Every page starts with 16 bytes:

| Bytes | Meaning |
|---|---|
| 0..4 | CRC-32C of bytes 4..4096 |
| 4 | page type: 1 meta, 2 branch, 3 leaf, 4 overflow, 5 free list |
| 5..8 | zero |
| 8..16 | the page's own number |

A page read back with the wrong checksum or the wrong number is reported as corrupt and never returned as data.

## Meta pages

Pages 0 and 1. Transaction N writes its meta page into slot N mod 2, so the previous commit's meta page is never touched by the next commit.

| Bytes | Meaning |
|---|---|
| 16..24 | magic `KVSQLDB\0` |
| 24..28 | format version, 1 |
| 28..32 | page size, 4096 |
| 32..40 | transaction id |
| 40..48 | root page of the directory tree, 0 if empty |
| 48..56 | number of pages in the file |
| 56..64 | first free-list page, 0 if none |
| 64..72 | number of free page ids listed |

On open, the valid meta page with the highest transaction id wins.

## Commit order

1. New pages are written to page numbers that no committed state uses: pages freed by an earlier commit, or new pages at the end of the file.
2. The file is synced.
3. The meta page for the new transaction is written to its slot.
4. The file is synced again.

A crash before step 4 completes leaves the previous meta page as the newest valid one, and everything it points to is untouched. Pages a transaction stops using go on the free list only for the commit after it, because the previous commit still points at them.

## Trees

Each keyspace is a B+tree. A directory tree maps keyspace names to their root page numbers (an 8-byte value; 0 means an empty keyspace).

Leaf and branch pages, after the header:

| Bytes | Meaning |
|---|---|
| 16..18 | cell count |
| 18..20 | zero |
| 20.. | one 2-byte offset per cell, then the cells |

Leaf cell: key length (2), flag (1: 0 inline, 1 overflow), value length (4), key, then the value or the first overflow page number (8). A cell larger than 1000 bytes stores its value in overflow pages.

Branch cell: key length (2), child page (8), key. The first cell's key is empty. Child i holds keys from key i up to, not including, key i+1.

## Overflow pages

| Bytes | Meaning |
|---|---|
| 16..24 | next overflow page, 0 at the end |
| 24..28 | bytes of value in this page |
| 28.. | value bytes |

## Free list pages

| Bytes | Meaning |
|---|---|
| 16..24 | next free-list page, 0 at the end |
| 24..28 | number of ids in this page |
| 28.. | free page numbers, 8 bytes each |
