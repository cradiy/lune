# Terminal State Fuzzing

This standalone workspace uses cargo-fuzz and nightly Rust to compare terminal
storage and cursor-driven screen mutations with independent models.

## Storage Target

The `storage` target compares each mutation with a model that owns complete display
units in vectors. The model rebuilds rows after edits and copies rows during scrolling.

The target mixes writes, appends, erasure, insertion, deletion, region and full-grid
scrolling, and resizing. After every operation it checks text, styles, occupancy,
dimensions, and error categories. Rejected operations must preserve model contents.

## Screen Target

The `screen` target shares the complete-unit storage model and represents pending
wrapping with a column one past the right edge. Its ASCII oracle writes individual
display units, and erasure selects ranges in a flattened physical display.

Commands interleave unit and ASCII writes, absolute cursor positioning, scrolling
regions, carriage return, backspace, line feed, reverse index, line and display
erasure, counted erasure, cell and whole-row insertion/deletion, resizing,
relative cursor movement, tabulation and tab-stop editing, automatic-wrap toggles,
and explicit
upward/downward scrolling of the active region. Explicit scrolling preserves
cursor coordinates and pending wrapping,
including when the cursor is outside the region. The movement oracle walks one
physical cell at a time until reaching its directional barrier.
The tab oracle uses an ordered set of columns, compares every active stop after
each command, and selects destinations by rank. Width changes retain the common
prefix, discard cropped stops, and initialize new columns at eight-column intervals.
Automatic wrapping starts enabled. Disabling clears pending wrapping; repeated
enabling preserves it. The scalar write oracle clamps writes at the last column
when wrapping is disabled and ignores validated wide units that cannot fit the
remaining columns. Its per-byte ASCII writes check the optimized batch path.
An explicit same-size resize command exercises pending-wrap preservation.

Every command checks cell text, width, continuation occupancy, styles, dimensions,
cursor coordinates, pending wrapping, scrolling boundaries, and error categories.
Argument errors preserve all visible state. Text inputs include empty units,
multi-scalar units, and lengths at and above the per-cell byte limit. The dictionary
also includes sequences for zero-count edits, rejected writes, width-only resizing,
and wide wrapping inside and outside the scrolling region.

## Input Bounds and Replay

Each eight-byte command selects an operation, coordinates, a count, and synthetic
text and style values. Executions process at most 512 commands and retain at most
16 by 16 logical cells in the storage target, or 129 by 16 in the screen target.
Screen resize width bytes `f9` through `fe` select 63, 64, 65, 127, 128, and 129
columns to exercise bitmap word boundaries. Other valid widths and heights are
limited to 16. Zero, overlarge, and invalid arguments are included.
For screen coordinates, bytes `fc`, `fd`, `fe`, and `ff` select the last cell, the
exclusive bound, zero, and `usize::MAX`; other bytes are reduced by the dimension
plus two. Screen opcodes modulo 29 select: 0 unit write, 1 ASCII, 2 cursor, 3 region,
4 resize, 5 carriage return, 6 backspace, 7 line feed, 8 reverse index, 9 line erasure,
10 display erasure, 11 cell insertion, 12 cell deletion, 13 counted erasure,
14 line insertion, 15 line deletion, 16 same-size resize, 17 move up, 18 move down,
19 move left, 20 move right, 21 scroll up, 22 scroll down, 23 tab forward,
24 tab backward, 25 set current tab, 26 clear current tab, 27 clear all tabs, and
28 set automatic wrapping from the low bit of byte 3.
Byte 3 selects width, count (`ff` means `usize::MAX`), or erasure mode.
Byte 4 selects synthetic text;
bytes 4 through 7 also form the style key.
Generated corpus, artifacts, coverage, and build output are ignored by Git.
Allocation-failure injection is covered by the terminal integration tests.

Run from `crates/terminal` after installing cargo-fuzz:

```sh
cargo +nightly fuzz run storage -- -dict=fuzz/storage.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run screen -- -dict=fuzz/screen.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
```

Pass a finding's artifact file as the corpus argument to replay it. The command
uses AddressSanitizer; input limits and duration define the campaign scope. Add
`-len_control=0` to sample the full allowed input length from the start of a campaign.
