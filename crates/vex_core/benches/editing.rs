use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use vex_core::{CharOffset, Document, Edit, Rope, Selection, SelectionSet};

fn fixture(bytes: usize, line: &str) -> Rope {
    Rope::from_str(&line.repeat(bytes.div_ceil(line.len())))
}

fn editing(c: &mut Criterion) {
    let mut group = c.benchmark_group("edit_undo");
    for (label, bytes, line) in [
        (
            "source_1MiB",
            1 << 20,
            "fn main() { println!(\"hello\"); }\n",
        ),
        (
            "source_10MiB",
            10 << 20,
            "fn main() { println!(\"hello\"); }\n",
        ),
        (
            "source_100MiB",
            100 << 20,
            "fn main() { println!(\"hello\"); }\n",
        ),
        ("long_line_10MiB", 10 << 20, "x"),
        (
            "unicode_10MiB",
            10 << 20,
            "日本語 e\u{301} 👩\u{200d}💻\r\n",
        ),
    ] {
        let text = fixture(bytes, line);
        for count in [1, 1_000] {
            let mut document = Document::from(text.clone());
            let mut selections = SelectionSet::new(
                (0..count)
                    .map(|index| {
                        Selection::cursor(CharOffset((index + 1) * text.len_chars() / (count + 1)))
                    })
                    .collect(),
                0,
            )
            .unwrap();
            group.bench_with_input(BenchmarkId::new(label, count), &count, |b, _| {
                b.iter(|| {
                    let edit = document
                        .replace_selections(&selections, black_box("a"))
                        .unwrap();
                    document.apply(edit, &mut selections).unwrap();
                    black_box(document.text());
                    document.undo(&mut selections).unwrap();
                });
            });
        }
    }
    group.finish();

    let text = fixture(10 << 20, "fn main() {}\n");
    let mut document = Document::from(text.clone());
    let mut selections =
        SelectionSet::single(Selection::new(CharOffset(1_000), CharOffset(11_000)));
    c.bench_function("delete_10k_scalars_and_undo", |b| {
        b.iter(|| {
            let edit = document.replace_selections(&selections, "").unwrap();
            document.apply(edit, &mut selections).unwrap();
            document.undo(&mut selections).unwrap();
            black_box(document.text());
        });
    });

    let transaction = document
        .transaction([Edit::insert(CharOffset(0), "a")])
        .unwrap();
    document.apply(transaction, &mut selections).unwrap();
    c.bench_function("undo_redo_10MiB", |b| {
        b.iter(|| {
            document.undo(&mut selections).unwrap();
            document.redo(&mut selections).unwrap();
            black_box(document.text());
        });
    });
    c.bench_function("snapshot_10MiB", |b| {
        b.iter(|| black_box(document.snapshot()));
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1));
    targets = editing
}
criterion_main!(benches);
