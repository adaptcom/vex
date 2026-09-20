use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::{hint::black_box, io, time::Duration};
use vex_core::{CharOffset, Document, Selection, SelectionSet};
use vex_editor::Language;
use vex_term::{
    app::App,
    screen::{Renderer, Style},
};

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

fn rust_syntax(c: &mut Criterion) {
    // Measure ANSI color output even under test runners that set NO_COLOR.
    crossterm::style::force_color_output(true);
    fn paint_highlighted(app: &mut App, renderer: &mut Renderer) -> usize {
        let frame = renderer.frame(120, 40).unwrap();
        app.paint(frame).unwrap();
        // A budget fallback must not silently turn this into a plain-text benchmark.
        assert!((0..120).any(|x| matches!(frame.style_at(x, 0), Some(Style::Syntax(_)))));
        renderer.present(&mut io::sink()).unwrap()
    }
    let mut group = c.benchmark_group("rust_syntax");
    for kib in [64usize, 256] {
        let pattern = "fn demo(value: u32) -> u32 { /* note */ value + 42 }\n";
        let document = Document::from(
            pattern
                .repeat((kib << 10usize).div_ceil(pattern.len()))
                .as_str(),
        );
        let mut app = App::from_document(Document::from(document.text().clone()), (120, 40));
        app.editor.set_language(Some(Language::Rust));
        let mut renderer = Renderer::default();
        paint_highlighted(&mut app, &mut renderer);
        let label = format!("{kib}KiB");
        group.bench_function(BenchmarkId::new("first_draw", &label), |b| {
            b.iter(|| {
                let mut app =
                    App::from_document(Document::from(document.text().clone()), (120, 40));
                app.editor.set_language(Some(Language::Rust));
                let mut renderer = Renderer::default();
                black_box(paint_highlighted(&mut app, &mut renderer));
            });
        });
        let mut right = false;
        group.bench_function(BenchmarkId::new("move", &label), |b| {
            b.iter(|| {
                right = !right;
                app.editor
                    .execute(if right { "move_right" } else { "move_left" }, 1)
                    .unwrap();
                black_box(paint_highlighted(&mut app, &mut renderer));
            });
        });
        let middle = document
            .text()
            .line_to_char(document.text().len_lines() / 2);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                middle + 3,
            ))))
            .unwrap();
        paint_highlighted(&mut app, &mut renderer);
        group.bench_function(BenchmarkId::new("type_8_undo", &label), |b| {
            b.iter(|| {
                for _ in 0..8 {
                    app.editor.insert_text("z").unwrap();
                }
                black_box(paint_highlighted(&mut app, &mut renderer));
                app.editor.execute("undo", 1).unwrap();
                black_box(paint_highlighted(&mut app, &mut renderer));
            });
        });
    }
    group.finish();
}

fn background_syntax(c: &mut Criterion) {
    let mut group = c.benchmark_group("background_syntax");
    let size = (120, 40);
    for kib in [64usize, 256] {
        let pattern = "fn demo(value: u32) -> u32 { /* note */ value + 42 }\n";
        let document = Document::from(pattern.repeat((kib << 10).div_ceil(pattern.len())).as_str());
        let label = format!("{kib}KiB");
        group.bench_function(BenchmarkId::new("first_frame", &label), |b| {
            b.iter(|| {
                let mut app = App::from_document(Document::from(document.text().clone()), size);
                app.editor.set_language(Some(Language::Rust));
                app.editor.set_background_syntax(true);
                let mut renderer = Renderer::default();
                black_box(paint(&mut app, &mut renderer, size));
                black_box(app.editor.take_syntax_job().unwrap());
            });
        });
        let mut app = App::from_document(Document::from(document.text().clone()), size);
        app.editor.set_language(Some(Language::Rust));
        app.editor.set_background_syntax(true);
        app.editor.execute("insert_mode", 1).unwrap();
        let mut renderer = Renderer::default();
        paint(&mut app, &mut renderer, size);
        black_box(app.editor.take_syntax_job().unwrap());
        group.bench_function(BenchmarkId::new("type_8_schedule_undo", &label), |b| {
            b.iter(|| {
                for _ in 0..8 {
                    app.editor.insert_text("z").unwrap();
                }
                black_box(paint(&mut app, &mut renderer, size));
                black_box(app.editor.take_syntax_job().unwrap());
                app.editor.execute("undo", 1).unwrap();
                black_box(paint(&mut app, &mut renderer, size));
                black_box(app.editor.take_syntax_job().unwrap());
            });
        });
    }
    group.finish();
}

fn buffers(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffers");
    for mib in [1usize, 100] {
        for count in [2usize, 1000] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = App::from_document(
                Document::from("line\n".repeat((mib << 20).div_ceil(5)).as_str()),
                (120, 40),
            );
            for index in 1..count {
                let path = directory.path().join(format!("{index}.txt"));
                std::fs::write(&path, "small\n").unwrap();
                app.execute(&format!("vsplit {}", path.display())).unwrap();
                app.execute("only").unwrap();
            }
            app.execute("bn").unwrap(); // The initial large scratch buffer.
            let label = format!("{mib}MiB_{count}_buffers");
            // Both directions return to the same large document. Setup, initial
            // reads, and initial viewport indexing are outside the measurement.
            app.execute("bn").unwrap();
            app.execute("bp").unwrap();
            group.bench_function(BenchmarkId::new("next_previous", &label), |b| {
                b.iter(|| {
                    app.execute("bn").unwrap();
                    app.execute("bp").unwrap();
                    black_box(app.editor.document().id());
                });
            });
            let mut renderer = Renderer::default();
            paint(&mut app, &mut renderer, (120, 40));
            let mut right = false;
            group.bench_function(BenchmarkId::new("move_draw", &label), |b| {
                b.iter(|| {
                    app.editor
                        .execute(if right { "move_right" } else { "move_left" }, 1)
                        .unwrap();
                    right = !right;
                    black_box(paint(&mut app, &mut renderer, (120, 40)));
                });
            });
        }
    }
    group.finish();
}

fn clipboard(c: &mut Criterion) {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut group = c.benchmark_group("clipboard_schedule_cancel");
    for mib in [1usize, 100] {
        let mut app = App::from_document(
            Document::from("line\n".repeat((mib << 20).div_ceil(5)).as_str()),
            (120, 40),
        );
        // Select the entire document before measuring. No clipboard provider is
        // invoked: this isolates UI dispatch, snapshot scheduling, and cancel.
        app.editor.execute("select_all", 1).unwrap();
        group.bench_function(BenchmarkId::from_parameter(mib), |b| {
            b.iter(|| {
                for key in [KeyCode::Char(' '), KeyCode::Char('y'), KeyCode::Esc] {
                    app.handle(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
                }
                black_box(app.editor.selections());
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1));
    targets = rendering, long_lines, rust_syntax, background_syntax, buffers, clipboard
}
criterion_main!(benches);
