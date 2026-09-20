use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::{hint::black_box, time::Duration};
use vex_core::{CharOffset, Document, Rope, Selection, SelectionSet};
use vex_editor::{Editor, Key, KeyHandler};

fn editor(text: &Rope, count: usize) -> Editor {
    let mut editor = Editor::new(Document::from(text.clone()));
    let selections = SelectionSet::new(
        (0..count)
            .map(|index| {
                Selection::cursor(CharOffset((index + 1) * text.len_chars() / (count + 1)))
            })
            .collect(),
        0,
    )
    .unwrap();
    editor.set_selections(selections).unwrap();
    editor
}

fn commands(c: &mut Criterion) {
    let mut group = c.benchmark_group("move_right_left");
    for (name, bytes, line) in [
        (
            "source_10MiB",
            10usize << 20,
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
            "日本語 e\u{301} 👩\u{200d}💻\r\n",
        ),
        ("long_line_10MiB", 10 << 20, "x"),
    ] {
        let text = Rope::from_str(&line.repeat(bytes.div_ceil(line.len())));
        for count in [1, 1_000] {
            let mut editor = editor(&text, count);
            let mut input = KeyHandler::default();
            group.bench_with_input(BenchmarkId::new(name, count), &count, |b, _| {
                b.iter(|| {
                    input
                        .handle(&mut editor, black_box(Key::Char('l')))
                        .unwrap();
                    input
                        .handle(&mut editor, black_box(Key::Char('h')))
                        .unwrap();
                    black_box(editor.selections());
                });
            });
        }
    }
    group.finish();

    let text = Rope::from_str(&"a\t界0123456789\r\n".repeat(500_000));
    let mut editor = editor(&text, 1);
    c.bench_function("move_down_up_tabs_unicode", |b| {
        b.iter(|| {
            editor.execute(black_box("move_down"), 1).unwrap();
            editor.execute(black_box("move_up"), 1).unwrap();
            black_box(editor.selections());
        });
    });
}

fn search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_next_previous");
    for bytes in [1 << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        let mut editor = editor(&rope, 1);
        editor.execute("search_forward", 1).unwrap();
        editor.update_search("value").unwrap();
        editor.execute("search_accept", 1).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(bytes), &bytes, |b, _| {
            b.iter(|| {
                editor.execute(black_box("search_next"), 1).unwrap();
                editor.execute(black_box("search_previous"), 1).unwrap();
                black_box(editor.selections());
            });
        });
    }
    group.finish();
}

fn background_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("background_search_schedule_cancel");
    for bytes in [1 << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        let mut editor = editor(&rope, 1);
        editor.set_background_search(true);
        group.bench_with_input(BenchmarkId::from_parameter(bytes), &bytes, |b, _| {
            b.iter(|| {
                editor.execute("search_forward", 1).unwrap();
                editor.update_search(black_box("missing_query")).unwrap();
                let replaced = editor.take_search_job().unwrap();
                editor.update_search(black_box("new_query")).unwrap();
                let cancelled = editor.take_search_job().unwrap();
                editor.execute("search_cancel", 1).unwrap();
                black_box(&replaced);
                black_box(&cancelled);
                assert!(replaced.cancellation().is_cancelled());
                assert!(cancelled.cancellation().is_cancelled());
                black_box(editor.selections());
            });
        });
    }
    group.finish();
}

fn comments(c: &mut Criterion) {
    let mut group = c.benchmark_group("comment_selected_lines_and_undo");
    for bytes in [1 << 20, 100 << 20] {
        let line = "    let value = 123;\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        for count in [1, 1_000] {
            let mut editor = editor(&rope, count);
            editor.set_language(Some(vex_editor::Language::Rust));
            group.bench_function(BenchmarkId::new(bytes.to_string(), count), |b| {
                b.iter(|| {
                    editor.execute("toggle_comments", 1).unwrap();
                    editor.execute("undo", 1).unwrap();
                    black_box(editor.selections());
                });
            });
        }
    }
    group.finish();
}

fn insert_line_kill(c: &mut Criterion) {
    let mut group = c.benchmark_group("kill_line_start_near_beginning");
    group
        .sample_size(10)
        .sampling_mode(criterion::SamplingMode::Flat);
    for bytes in [1 << 20, 100 << 20] {
        let rope = Rope::from_str(&" ".repeat(bytes));
        let mut editor = Editor::new(Document::from(rope));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(8))))
            .unwrap();
        group.bench_function(BenchmarkId::from_parameter(bytes), |b| {
            b.iter(|| {
                editor.execute("kill_to_line_start", 1).unwrap();
                black_box(editor.selections());
                editor.execute("undo", 1).unwrap();
            });
        });
    }
    group.finish();
}

fn copy_selections(c: &mut Criterion) {
    let mut group = c.benchmark_group("copy_selection_next_line");
    for bytes in [1 << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        let mut editor = editor(&rope, 1);
        let origin = editor.selections().clone();
        group.bench_function(BenchmarkId::new("scan_and_restore", bytes), |b| {
            b.iter(|| {
                editor.execute("copy_selection_on_next_line", 1).unwrap();
                black_box(editor.selections());
                editor.set_selections(origin.clone()).unwrap();
            });
        });
        editor.set_background_search(true);
        group.bench_function(BenchmarkId::new("schedule_cancel", bytes), |b| {
            b.iter(|| {
                editor.execute("copy_selection_on_next_line", 1).unwrap();
                let job = editor.take_search_job().unwrap();
                editor.finish_undo_group();
                black_box(job);
            });
        });
    }
    group.finish();
}

fn textobjects(c: &mut Criterion) {
    let mut group = c.benchmark_group("textobjects");
    for bytes in [1 << 20, 100 << 20] {
        let line = "alpha beta gamma\nsecond line\n\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        let mut editor = editor(&rope, 1);
        let origin = editor.selections().clone();
        for (name, object) in [("word", 'w'), ("paragraph", 'p')] {
            group.bench_function(BenchmarkId::new(name, bytes), |b| {
                b.iter(|| {
                    let mut context = vex_editor::CommandContext::new(&mut editor);
                    context.character = Some(object);
                    vex_editor::commands::select_textobject_inner(&mut context).unwrap();
                    black_box(editor.selections());
                    editor.set_selections(origin.clone()).unwrap();
                });
            });
        }
        editor.set_background_search(true);
        group.bench_function(BenchmarkId::new("schedule_cancel", bytes), |b| {
            b.iter(|| {
                let mut context = vex_editor::CommandContext::new(&mut editor);
                context.character = Some('p');
                vex_editor::commands::select_textobject_inner(&mut context).unwrap();
                let job = editor.take_search_job().unwrap();
                editor.finish_undo_group();
                black_box(job);
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
    targets = commands, search, background_search, comments, copy_selections, insert_line_kill, textobjects
}
criterion_main!(benches);
