use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::{hint::black_box, io, time::Duration};
use vex_core::Document;
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

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1));
    targets = rendering
}
criterion_main!(benches);
