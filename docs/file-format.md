# File format

A KvSQL file is a sequence of 4096-byte pages. All integers are little endian.

## Page header

Every page starts with 16 bytes:

| Bytes | Meaning |
|---|---|
| 0..4 | CRC-32C of bytes 4..4096 |
| 4 | page type: 1 meta, 2 branch, 3 leaf, 4 overflow, 5 free list, 6 commit record |
| 5..8 | zero |
| 8..16 | the page's own number |

A page read back with the wrong checksum or the wrong number is reported as corrupt and never returned as data.

## Meta pages

Pages 0 and 1. Transaction N writes its meta page into slot N mod 2, so the previous commit's meta page is never touched by the next commit.

| Bytes | Meaning |
|---|---|
| 16..24 | magic `KVSQLDB\0` |
| 24..28 | format version, 2 (1 in files last written by KvSQL 0.1) |
| 28..32 | page size, 4096 |
| 32..40 | transaction id |
| 40..48 | root page of the directory tree, 0 if empty |
| 48..56 | number of pages in the file |
| 56..64 | first free-list page, 0 if none |
| 64..72 | number of free page ids listed |
| 72..80 | first commit record page, 0 if none (version 2) |
| 80..88 | number of commit record entries (version 2) |
| 88..92 | CRC-32C of all commit record entries in order (version 2) |

On open, meta pages are tried newest transaction first. A version 1 meta page, and the empty file's first meta page (transaction 0, no record), are accepted as they are. A version 2 meta page is accepted only if its commit record checks out: every record page is sealed, has type 6 and carries this transaction id, the entries add up to the count and CRC in the meta page, and every page the record lists reads back sealed with the listed CRC. Otherwise the other meta page is tried.

A version 1 file is read as it is and becomes version 2 at its first commit. KvSQL 0.1 refuses a version 2 file.

## Commit order

1. New pages are written to page numbers that no committed state uses: pages freed by an earlier commit, or new pages at the end of the file. This includes the new free list and the commit record.
2. The meta page for the new transaction is written to its slot.
3. The file is synced, once.

The commit returns only after step 3. A crash before then can leave any mix of these writes on disk, in any order and partly written. If the meta page is missing or damaged, the previous meta page is the newest valid one. If the meta page arrived but some page it depends on did not, its commit record fails the check on open and the previous meta page is used. Either way the previous commit is intact: it was synced in full before this commit began, and this commit writes only to pages the previous commit does not use, including its free-list and record pages, which are freed only by the commit after this one. Pages a transaction stops using go on the free list only for the commit after it, because the previous commit still points at them.

Version 1 files were written with two syncs, one after step 1 and one after step 2, and have no commit record.

In memory, each free page is tagged with the commit that stopped using it. A page freed by commit T is handed out again only when no open reader holds a snapshot older than T. The tag is not stored in the file: after a reopen no reader exists, so every listed page is free at once.

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

## Commit record pages

| Bytes | Meaning |
|---|---|
| 16..24 | next record page, 0 at the end |
| 24..28 | number of entries in this page |
| 28..36 | transaction id of the commit this record belongs to |
| 36.. | entries: page number (8), then that page's stored CRC-32C (4) |

The record lists every page its commit wrote except the record pages and the meta page, sorted by page number. Record pages are freed by the next commit, the same way as free-list pages.

## Free list pages

| Bytes | Meaning |
|---|---|
| 16..24 | next free-list page, 0 at the end |
| 24..28 | number of ids in this page |
| 28.. | free page numbers, 8 bytes each |
