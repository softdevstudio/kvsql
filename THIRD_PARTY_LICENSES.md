# Third Party Licenses

KvSQL Engine and KvSQL DB have no external crate dependencies. They use only the Rust standard library.

## Rust standard library

The `kvsqld` and `kvsql` executables in each release link the Rust standard library, which is dual licensed under the MIT License and the Apache License 2.0. The Apache License 2.0 text is in LICENSE, shipped in every release archive. The MIT License text follows.

```
Copyright (c) The Rust Project Contributors

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## Development only

The tests and the benchmark use rusqlite (MIT) with a bundled copy of SQLite (public domain) to check SQL answers and to compare speed. They are development dependencies only and are not part of the library or any binary.

GitHub Actions used in CI and in the release workflow are build infrastructure and are not part of any library or binary.
