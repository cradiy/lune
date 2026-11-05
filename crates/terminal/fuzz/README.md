# Terminal State Fuzzing

This standalone workspace uses cargo-fuzz and nightly Rust to compare terminal
storage, cursor-driven screen mutations, byte-stream execution, and SGR rendition
with independent expectations.

## Storage Target

The `storage` target compares each mutation with a model that owns complete display
units in vectors. The model rebuilds rows after edits and copies rows during scrolling.

The target mixes writes, appends, erasure, insertion, deletion, region and full-grid
scrolling, and resizing. After every operation it checks text, styles, occupancy,
dimensions, and error categories. Rejected operations must preserve model contents.
Complete units also record erased state. Printed spaces and nonempty appends are
text; blank filling, erased ranges, cropped wide fragments, and exposed cells are
erased. Copying or remapping a retained unit preserves this distinction. All
targets sharing this model compare erased state independently of text and style.

## Screen Target

The `screen` target shares the complete-unit storage model and represents pending
wrapping with a column one past the right edge. Its ASCII oracle writes individual
display units, and erasure selects ranges in a flattened physical display.

Commands interleave unit and ASCII writes, clamped cursor addressing, scrolling
regions, carriage return, backspace, line feed, reverse index, line and display
erasure, counted erasure, cell and whole-row insertion/deletion, resizing,
relative cursor movement, tabulation and tab-stop editing, automatic-wrap and
origin-mode settings, and explicit
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
Origin mode selects a full-screen or regional row iterator for addressing.
Mode and region settings home the cursor and clear pending wrapping. Every
command compares both mode flags and checks that origin-mode cursors remain
within the scrolling region. Width changes retain the region; height changes
reset it to the full screen while preserving origin mode.
Save and restore commands retain modes and the position in its addressing scope.
The saved oracle records pending wrapping as an optional original width. Restore
selects a row in the current addressing range and restores pending wrapping only
when that row fits and the width matches. The slot survives resizing and repeated
restores. A final restore also checks saves at the end of each input.
Insertion-mode writes first edit complete display units in the reference row,
then write one unit. The ASCII oracle repeats that operation per input byte,
checking the batched implementation. Reset disables insertion mode; resize and
cursor restores preserve it.
Reset rebuilds the oracle at the current dimensions with styled blanks and
initial cursor state, modes, region, tabs, and save slot. Mixed sequences also
check subsequent restore, writing, tabulation, and resizing after reset.

Every command checks cell text, width, continuation occupancy, styles, dimensions,
cursor coordinates, pending wrapping, scrolling boundaries, and error categories.
Argument errors preserve all visible state. Text inputs include empty units,
multi-scalar units, and lengths at and above the per-cell byte limit. The dictionary
also includes sequences for zero-count edits, rejected writes, width-only resizing,
and wide wrapping inside and outside the scrolling region.

## Processor Target

The `processor` target serializes commands into VT bytes and compares execution
with the complete-unit Screen model. It exercises cursor addressing, scrolling,
cell and line edits, erasure, tabs, mode changes, saved cursors, and reset, together
with malformed UTF-8, discarded strings, rejected parameter forms, and diagnostics.
Unsupported mode lists preserve all screen state.

Each input runs as one chunk and in chunks of one through seventeen bytes. A text
callback can fail before mutation; execution retries the reported input suffix.
Both runs must produce the same modeled screen, saved cursor, and diagnostics.
SGR commands update an independent rendition value and a vector of reference
styles. Cell styles are compared by resolved value, allowing actual IDs to change
as collection runs between chunks and retries. Collection must retain current
and saved rendition even when they are absent from screen cells.
The reference state also tracks G0/G1 designation, SI/SO selection, and saved
character sets. ASCII fixtures map through an independent set of expected glyphs;
malformed UTF-8 followed by text or a shift exercises reprocessed bytes and
pending-event order. A final restore and text write exposes saved character sets.
The reference also tracks ANSI new-line mode independently of cursor saves. LF,
VT, and FF select either vertical movement or carriage return plus line feed;
explicit IND/NEL and reset commands check the distinct mode boundaries.
The fixed-width fixture maps one synthetic scalar to an owned multi-scalar display
unit, exercising text storage through the execution boundary. Inputs contain at
most 512 eight-byte commands and use an eight-column, four-row screen.

## Style Target

The `style` target creates a rendition value from synthetic input and independently
encodes it as one SGR sequence. Whole-input and fragmented parsing must recover
that value, including default, indexed, and RGB colors in semicolon and colon
forms. Attributes include selective intensity flags and both underline variants.
Arbitrary parameter lists additionally check rejection atomicity, idempotence,
and an explicit final reset. The target retains at most 32 generated parameters;
its wire encoder uses a fixed-size synthetic seed.

## Style Table Target

The `styles` target compares a bounded table with a map of live IDs and a history
of every issued ID. Commands interleave interning, duplicate lookup, valid and
invalid root sets, collection, and stale-ID queries. Reclaimed IDs must stay
invalid through slot reuse, retained values keep their IDs, and rejected root
sets preserve all mappings. Inputs contain at most 128 four-byte commands and a
capacity of one through sixteen styles. Generation retirement and allocation
failures are covered by the terminal tests.

## Input Bounds and Replay

Each eight-byte command selects an operation, coordinates, a count, and synthetic
text and style values. Executions process at most 512 commands and retain at most
16 by 16 logical cells in the storage target, or 129 by 16 in the screen target.
Screen resize width bytes `f9` through `fe` select 63, 64, 65, 127, 128, and 129
columns to exercise bitmap word boundaries. Other valid widths and heights are
limited to 16. Zero, overlarge, and invalid arguments are included.
For screen coordinates, bytes `fc`, `fd`, `fe`, and `ff` select the last cell, the
exclusive bound, zero, and `usize::MAX`; other bytes are reduced by the dimension
plus two. Screen opcodes modulo 34 select: 0 unit write, 1 ASCII, 2 cursor, 3 region,
4 resize, 5 carriage return, 6 backspace, 7 line feed, 8 reverse index, 9 line erasure,
10 display erasure, 11 cell insertion, 12 cell deletion, 13 counted erasure,
14 line insertion, 15 line deletion, 16 same-size resize, 17 move up, 18 move down,
19 move left, 20 move right, 21 scroll up, 22 scroll down, 23 tab forward,
24 tab backward, 25 set current tab, 26 clear current tab, 27 clear all tabs,
28 set automatic wrapping, 29 set origin mode, 30 save cursor state, 31 restore
cursor state, 32 reset the screen with the style key, and 33 set insertion mode.
Mode settings use the low bit of byte 3.
Byte 3 selects width, count (`ff` means `usize::MAX`), or erasure mode.
Byte 4 selects synthetic text;
bytes 4 through 7 also form the style key.
Generated corpus, artifacts, coverage, and build output are ignored by Git.
Allocation-failure injection is covered by the terminal integration tests.

Run from `crates/terminal` after installing cargo-fuzz:

```sh
cargo +nightly fuzz run storage -- -dict=fuzz/storage.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run screen -- -dict=fuzz/screen.dict -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run processor -- -max_len=4096 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run style -- -max_len=128 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
cargo +nightly fuzz run styles -- -max_len=512 -timeout=2 -rss_limit_mb=1024 -max_total_time=60
```

Pass a finding's artifact file as the corpus argument to replay it. The command
uses AddressSanitizer; input limits and duration define the campaign scope. Add
`-len_control=0` to sample the full allowed input length from the start of a campaign.
