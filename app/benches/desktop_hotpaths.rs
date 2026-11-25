use std::hint::black_box;
use std::time::{Duration, Instant};

use lune::desktop_bench::{discover_viewport_blocks, search_scrollback};
use lune_terminal::{HistoryLimits, MAX_STYLES, Palette, Processor, Rgb, Screen, UnicodeWriter};

fn build_scrollback(blocks_count: usize, lines_per_block: usize, with_output_mark: bool) -> Screen {
    let black = Rgb {
        red: 0,
        green: 0,
        blue: 0,
    };
    let white = Rgb {
        red: 255,
        green: 255,
        blue: 255,
    };
    let palette = Palette::new([black; 256], white, black, white);
    let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
    let mut screen = Screen::new(120, 48).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 12_000,
            max_bytes: 32 * 1024 * 1024,
        })
        .unwrap();
    let mut writer = UnicodeWriter::default();

    let mut payload = String::with_capacity(blocks_count * (lines_per_block + 2) * 80);
    for b in 0..blocks_count {
        payload.push_str("\x1b]133;A\x07$ \x1b]133;B\x07cargo test --workspace module_");
        payload.push_str(&b.to_string());
        if with_output_mark {
            payload.push_str("\r\n\x1b]133;C\x07");
        } else {
            payload.push_str("\r\n");
        }
        for l in 0..lines_per_block {
            if l % 25 == 0 {
                payload.push_str("WARNING: ERR-4042 latency spike in worker pool #");
                payload.push_str(&b.to_string());
                payload.push_str(" line ");
                payload.push_str(&l.to_string());
                payload.push_str(" [ok]\r\n");
            } else {
                payload.push_str("  running test case desktop::renderer::frame_pipeline_invariant ... ok (0.12ms)\r\n");
            }
        }
        payload.push_str("\x1b]133;D;0\x07");
    }

    processor
        .advance(payload.as_bytes(), &mut screen, &mut writer)
        .unwrap();
    screen
}

fn measure(name: &str, mut f: impl FnMut() -> usize) {
    let mut samples = [0.0_f64; 5];
    let mut last_count = 0;

    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(120) {
            last_count = black_box(f());
            iterations += 1;
        }
        *sample = start.elapsed().as_secs_f64() * 1_000.0 / iterations as f64;
    }

    samples.sort_by(f64::total_cmp);
    println!("{name:<36} {:>8.3} ms/op  (count={last_count})", samples[2]);
}

fn main() {
    let screen = build_scrollback(200, 48, true);
    let view = screen.view();
    let total_rows = view.rows();
    let viewport = total_rows.saturating_sub(48)..total_rows;
    println!("Scrollback rows: {total_rows}");

    measure("search literal ci (sparse)", || {
        search_scrollback(view, 120, viewport.clone(), "ERR-4042", false, false)
    });
    measure("search literal cs (sparse)", || {
        search_scrollback(view, 120, viewport.clone(), "ERR-4042", true, false)
    });
    measure("search literal ci (dense)", || {
        search_scrollback(view, 120, viewport.clone(), "renderer", false, false)
    });
    measure("search regex (sparse)", || {
        search_scrollback(view, 120, viewport.clone(), "ERR-[0-9]+", true, true)
    });
    measure("blocks_in_viewport (marked)", || {
        discover_viewport_blocks(view, viewport.clone())
    });

    let unmarked = build_scrollback(4, 2500, false);
    let unmarked_view = unmarked.view();
    let u_rows = unmarked_view.rows();
    let u_viewport = u_rows.saturating_sub(48)..u_rows;
    measure("blocks_in_viewport (unmarked 2.5k)", || {
        discover_viewport_blocks(unmarked_view, u_viewport.clone())
    });
}
