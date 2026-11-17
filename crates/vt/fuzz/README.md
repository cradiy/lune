# VT Fuzzing

This standalone fuzz workspace uses cargo-fuzz and nightly Rust. Its generated
corpus, artifacts, and build output are ignored by Git.
See the [cargo-fuzz guide](https://rust-fuzz.github.io/book/cargo-fuzz/guide.html)
and [libFuzzer options](https://llvm.org/docs/LibFuzzer.html#options) for tool setup
and campaign controls.

- `chunking` compares bulk, bytewise, and random-chunk event streams. It also
  checks text-only decoding against the standard library's UTF-8 replacement policy.
- `lifecycle` varies chunk boundaries and `finish`/`reset` operations, compares
  each stream with a fresh parser, and checks reuse after stream termination.

Both targets normalize text spans into scalar events and OSC/DCS spans into byte
events, preserving headers, framing, cancellation, and output bounds. DCS cases
exercise raw C0/high bytes, fixed header limits, and rejected-string recovery.
Use libFuzzer's timeout and memory limits to bound individual executions.

Run from `crates/vt` after installing cargo-fuzz:

```sh
cargo +nightly fuzz run chunking -- -dict=fuzz/parser.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run lifecycle -- -dict=fuzz/parser.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
```

To replay a finding, pass its artifact file as the corpus argument to the same
target. Input limits and campaign duration define the scope of each run.
