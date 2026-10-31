# Screen Storage Fuzzing

This standalone workspace uses cargo-fuzz and nightly Rust. The `storage` target
compares each mutation with a model that owns complete display units in vectors.
The model rebuilds rows after edits and copies rows during scrolling.

The target mixes writes, appends, erasure, insertion, deletion, region and full-grid
scrolling, and resizing. After every operation it checks text, styles, occupancy,
dimensions, and error categories. Rejected operations must preserve model contents.

Each eight-byte command selects an operation, coordinates, a count, and synthetic
text and style values. Executions process at most 512 commands and retain at most
16 by 16 logical cells. Zero, overlarge, and invalid arguments are included.
Generated corpus, artifacts, coverage, and build output are ignored by Git.

Run from `crates/terminal` after installing cargo-fuzz:

```sh
cargo +nightly fuzz run storage -- -dict=fuzz/storage.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
```

Pass a finding's artifact file as the corpus argument to replay it. The command
above uses AddressSanitizer; input limits and duration define the campaign scope.
