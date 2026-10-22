# Third Party Licenses

KvSQL Engine has no external crate dependencies. It uses only the Rust standard library.

Distributed executables link the Rust standard library, which is dual licensed under the MIT License and the Apache License 2.0. Release archives will carry those licence texts once binaries are published.

The tests use rusqlite (MIT) with a bundled copy of SQLite (public domain) to check SQL answers. They are development dependencies only and are not part of the library or any binary.

GitHub Actions used in CI are build infrastructure and are not part of any library or binary.
