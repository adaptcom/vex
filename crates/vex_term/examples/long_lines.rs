//! A quick, reproducible end-of-line latency probe, including cold layout.
//! Usage: cargo run --release -p vex_term --example long_lines -- 1 10
//! Arguments are MiB per line and samples per operation. Output goes to a sink.
use std::{env, io, time::Instant};
use vex_core::{CharOffset, Document, Selection, SelectionSet};
use vex_term::{app::App, screen::Renderer};

fn paint(app: &mut App, renderer: &mut Renderer) {
    app.paint(renderer.frame(120, 40).unwrap()).unwrap();
    renderer.present(&mut io::sink()).unwrap();
}

fn measure(name: &str, samples: usize, mut operation: impl FnMut()) {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        operation();
        times.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{name:24} median {:10.3} ms  max {:10.3} ms",
        times[samples / 2],
        times[samples - 1]
    );
}

fn main() {
    let mib = env::args()
        .nth(1)
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(1);
    let samples = env::args()
        .nth(2)
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(10)
        .max(1);
    for (name, pattern) in [
        ("ASCII", "x"),
        ("Unicode/tabs", "ab\t界e\u{301}👩\u{200d}💻 "),
    ] {
        let line = pattern.repeat((mib << 20).div_ceil(pattern.len()));
        let text = format!("{line}\n{line}");
        let mut app = App::from_document(Document::from(text.as_str()), (120, 40));
        app.editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                line.chars().count() - 1,
            ))))
            .unwrap();
        let mut renderer = Renderer::default();
        println!("{name}: {mib} MiB per line, 120x40, {samples} samples (except cold)");
        measure("cold draw", 1, || paint(&mut app, &mut renderer));
        let mut right = false;
        measure("horizontal + draw", samples, || {
            right = !right;
            app.editor
                .execute(if right { "move_left" } else { "move_right" }, 1)
                .unwrap();
            paint(&mut app, &mut renderer);
        });
        let mut down = false;
        measure("vertical + draw", samples, || {
            down = !down;
            app.editor
                .execute(if down { "move_down" } else { "move_up" }, 1)
                .unwrap();
            paint(&mut app, &mut renderer);
        });
        app.editor.execute("insert_mode", 1).unwrap();
        measure("insert/draw/undo/draw", samples, || {
            app.editor.insert_text("z").unwrap();
            paint(&mut app, &mut renderer);
            app.editor.execute("undo", 1).unwrap();
            paint(&mut app, &mut renderer);
        });
    }
}
