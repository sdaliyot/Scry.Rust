# Third-party notices

Scry.Rust is distributed under the MIT license (see [`LICENSE`](LICENSE)).

## The `scry-rs` command line

The `scry-rs` binaries on the [releases page](https://github.com/sdaliyot/Scry.Rust/releases)
include the crates below. Each one offers MIT among its licenses, and is used here under MIT.

| Crate | Version | License | Copyright |
|---|---|---|---|
| [`serde_json`](https://github.com/serde-rs/json) | 1.0.154 | MIT or Apache-2.0 | The serde_json authors |
| [`serde`](https://github.com/serde-rs/serde) | 1.0.229 | MIT or Apache-2.0 | The serde authors |
| [`serde_core`](https://github.com/serde-rs/serde) | 1.0.229 | MIT or Apache-2.0 | The serde authors |
| [`itoa`](https://github.com/dtolnay/itoa) | 1.0.18 | MIT or Apache-2.0 | The itoa authors |
| [`zmij`](https://github.com/dtolnay/zmij) | 1.0.23 | MIT | The zmij authors |
| [`memchr`](https://github.com/BurntSushi/memchr) | 2.8.3 | Unlicense or MIT | Copyright (c) 2015 Andrew Gallant |

The MIT license text that applies to each of them:

```text
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

## The crates on crates.io

`scry-rs`, `scry-rs-derive`, `scry-rs-cargo` and `scry-rs-client` are published as source. Cargo fetches their
dependencies separately, under each dependency's own license; `cargo tree` lists them.
