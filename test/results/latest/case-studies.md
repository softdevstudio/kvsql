# KvSQL case studies and use examples

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.80GHz, 2 hardware threads
- KvSQL 0.1.0, release build, one run
- Sessions and gateway: real files in the temp directory, every commit synced. Checkout: the simulated disk used by the power-cut tests

## Case study: sessions and reporting in one store

- 100000 sessions for 20,000 users, written in one transaction with `put_row`: 964.3 ms
- 200000 random token lookups with `get_row`: 2.00 µs each, all found
- The same lookups through `SELECT user_id FROM sessions WHERE token = ?`: 4.10 µs each
- Dashboard query, live sessions per country over all 100000 rows: 53.5 ms (49586 live, 8 countries)
- Revoking user 4242: 5 sessions before, 5 deleted through the index in 1.7 ms including the synced commit, 0 left; a reader opened before the delete still counted 5; `get_row` on one of the revoked tokens then found nothing
- `verify` afterwards: 3747 pages, 199992 keys, every index entry matches its row

## Case study: an edge gateway with kvsqld

- 20 devices on their own connections sent 10000 readings, one request each, every request a synced transaction holding the history row and the latest value: 8.1 s, about 1236 requests a second
- 2000 dashboard reads of `GET /rows/latest/dev-NN` over HTTP: 71.40 µs each, round trip included (last answer `{"row":["dev-19",1800014970,23.8]}`)
- Per-device summary over all 10000 readings, top three by peak: 5.0 ms, answer `[["dev-07",500,22.83,34.9],["dev-00",500,22.03,25.9],["dev-01",500,22.01,25.9]]`
- Alert query, readings over 30 °C: 3.4 ms, answer `[["dev-07",40,1800006000,1800007170]]` (device, count, first and last timestamp)
- One device's history through the index: 0.6 ms, answer `[[500,22.83]]`
- `GET /verify` afterwards: `{"ok":true,"txid":10001,"pages":327,"free_pages":7,"keyspaces":4,"keys":20023}`

## Case study: a checkout that is never half done

- The checkout reads the cart by key, inserts the order and three order lines and takes the stock through SQL, reads each price by key, and deletes the cart by key, all in one transaction. Its commit is 3 writes and 2 syncs on the simulated disk
- Before: cart `kettlex1,mugx4,teapotx1`, stock [12, 40, 5, 9]. After: no cart, stock [11, 36, 4, 9], order 5001 total 9700 cents
- Power cut after each of the 5 steps and after none, each cut replayed 200 ways (every unsynced write kept, none kept, any subset, subsets with torn sectors): 1200 reopened files
- Every one passed `verify` and held either the shop before the checkout (794) or after it (406). None held an order without its stock taken, or a cart still there beside its order

## Use example: a cache with expiry

- 10,000 cached responses. `get_row` hit: true, miss: false. Sweep `DELETE FROM cache WHERE expires < 1300`: 5100 removed, 4900 left

## Use example: settings read by key, audited by SQL

- `get("settings", b"checkout.new_flow")` gives `on`. The same plain keyspace in SQL, with no schema, `WHERE CAST(key AS TEXT) LIKE 'search.%'`: `search.fuzzy = off`, `search.limit = 50`

## Use example: the kvsql shell on a file

```
kvsql> CREATE TABLE jobs (id INTEGER PRIMARY KEY, kind TEXT, state TEXT)
0 rows changed
kvsql> INSERT INTO jobs VALUES (1, 'resize', 'done'), (2, 'resize', 'queued'), (3, 'email', 'queued')
3 rows changed
kvsql> SELECT state, count(*) AS n FROM jobs GROUP BY state
state  | n
-------+--
done   | 1
queued | 2
(2 rows)
kvsql> .row jobs 2
2 | resize | queued
kvsql> .put notes deploy shipped 0.1.0
ok
kvsql> .get notes deploy
shipped 0.1.0
kvsql> .verify
ok: commit 3, 9 pages (2 free), 3 keyspaces, 5 keys
```

