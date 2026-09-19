use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::{hint::black_box, io, time::Duration};
use vex_core::{CharOffset, Document, Selection, SelectionSet};
use vex_term::{app::App, screen::Renderer};

fn paint(app: &mut App, renderer: &mut Renderer, size: (u16, u16)) -> usize {
    app.paint(renderer.frame(size.0, size.1).unwrap()).unwrap();
    renderer.present(&mut io::sink()).unwrap()
}

fn rendering(c: &mut Criterion) {
    let mut group = c.benchmark_group("viewport");
    for (name, bytes, pattern) in [
        (
            "source_1MiB",
            1usize << 20,
            "fn main() { println!(\"hello\"); }\n",
        ),
        (
            "source_100MiB",
            100 << 20,
            "fn main() { println!(\"hello\"); }\n",
        ),
        (
            "unicode_10MiB",
            10 << 20,
            "\t日本語 e\u{301} 👩\u{200d}💻\r\n",
        ),
        ("long_line_10MiB", 10 << 20, "x"),
    ] {
        let document = Document::from(pattern.repeat(bytes.div_ceil(pattern.len())).as_str());
        for size in [(120, 40), (240, 80)] {
            let mut app = App::from_document(Document::from(document.text().clone()), size);
            let mut renderer = Renderer::default();
            paint(&mut app, &mut renderer, size);
            let label = format!("{name}_{}x{}", size.0, size.1);
            group.bench_function(BenchmarkId::new("full", &label), |b| {
                b.iter(|| {
                    renderer.invalidate();
                    black_box(paint(&mut app, &mut renderer, size));
                });
            });
            let mut right = false;
            group.bench_function(BenchmarkId::new("move", &label), |b| {
                b.iter(|| {
                    right = !right;
                    app.handle(Event::Key(KeyEvent::new(
                        KeyCode::Char(if right { 'l' } else { 'h' }),
                        KeyModifiers::NONE,
                    )));
                    black_box(paint(&mut app, &mut renderer, size));
                });
            });
        }
    }
    group.finish();
}

fn long_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("deep_line");
    let size = (120, 40);
    for mib in [1usize, 10] {
        for (name, pattern) in [
            ("ascii", "x"),
            ("unicode_tabs", "ab\t界e\u{301}👩\u{200d}💻 "),
        ] {
            let line = pattern.repeat((mib << 20usize).div_ceil(pattern.len()));
            let mut app =
                App::from_document(Document::from(format!("{line}\n{line}").as_str()), size);
            app.editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                    line.chars().count() - 1,
                ))))
                .unwrap();
            let mut renderer = Renderer::default();
            // Initial indexing is measured separately by examples/long_lines.rs.
            paint(&mut app, &mut renderer, size);
            let label = format!("{name}_{mib}MiB");
            let mut right = false;
            group.bench_function(BenchmarkId::new("horizontal", &label), |b| {
                b.iter(|| {
                    right = !right;
                    app.editor
                        .execute(if right { "move_left" } else { "move_right" }, 1)
                        .unwrap();
                    black_box(paint(&mut app, &mut renderer, size));
                });
            });
            let mut down = false;
            group.bench_function(BenchmarkId::new("vertical", &label), |b| {
                b.iter(|| {
                    down = !down;
                    app.editor
                        .execute(if down { "move_down" } else { "move_up" }, 1)
                        .unwrap();
                    black_box(paint(&mut app, &mut renderer, size));
                });
            });
            app.editor.execute("insert_mode", 1).unwrap();
            // Always edit the first line so the second visible line also shifts.
            app.editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                    line.chars().count() - 1,
                ))))
                .unwrap();
            for count in [1, 8] {
                group.bench_function(
                    BenchmarkId::new(format!("type_{count}_undo"), &label),
                    |b| {
                        b.iter(|| {
                            for _ in 0..count {
                                app.editor.insert_text("z").unwrap();
                            }
                            black_box(paint(&mut app, &mut renderer, size));
                            app.editor.execute("undo", 1).unwrap();
                            black_box(paint(&mut app, &mut renderer, size));
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1));
    targets = rendering, long_lines
}
criterion_main!(benches);
