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

fn delimiter_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("delimiter_matching");
    for bytes in [1 << 20, 100 << 20] {
        let text = Rope::from_str(&"(word)\n".repeat(bytes / 7));
        for count in [1, 1000] {
            let mut editor = Editor::new(Document::from(text.clone()));
            editor
                .set_selections(
                    SelectionSet::new(
                        (0..count)
                            .map(|index| Selection::cursor(CharOffset(index * 7)))
                            .collect(),
                        0,
                    )
                    .unwrap(),
                )
                .unwrap();
            group.bench_function(
                BenchmarkId::new(format!("local_round_trip_{count}"), bytes),
                |b| {
                    b.iter(|| {
                        editor.execute("match_brackets", 1).unwrap();
                        editor.execute("match_brackets", 1).unwrap();
                        black_box(editor.selections());
                    });
                },
            );
            if count == 1 {
                editor.set_background_search(true);
                group.bench_function(BenchmarkId::new("schedule_cancel", bytes), |b| {
                    b.iter(|| {
                        editor.execute("match_brackets", 1).unwrap();
                        black_box(editor.take_search_job().unwrap());
                        editor.finish_undo_group();
                    });
                });
            }
        }
    }
    for bytes in [1024, 64 << 10] {
        let line = "fn f() { f(1); }\n";
        let mut editor = Editor::new(Document::from(line.repeat(bytes / line.len()).as_str()));
        editor.set_language(Some(vex_editor::Language::Rust));
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(11))))
            .unwrap();
        editor.execute("match_brackets", 1).unwrap();
        assert_eq!(editor.selections().primary().start(), CharOffset(12));
        group.bench_function(BenchmarkId::new("warm_syntax_round_trip", bytes), |b| {
            b.iter(|| {
                editor.execute("match_brackets", 1).unwrap();
                editor.execute("match_brackets", 1).unwrap();
                black_box(editor.selections());
            });
        });
    }
    group.finish();
}

fn surround_edit(c: &mut Criterion) {
    let mut group = c.benchmark_group("surround_edit_undo");
    for bytes in [1 << 20, 100 << 20] {
        let rope = Rope::from_str(&"(word)\n".repeat(bytes / 7));
        for count in [1, 1000] {
            let mut editor = Editor::new(Document::from(rope.clone()));
            editor
                .set_selections(
                    SelectionSet::new(
                        (0..count)
                            .map(|index| Selection::cursor(CharOffset(index * 7 + 1)))
                            .collect(),
                        0,
                    )
                    .unwrap(),
                )
                .unwrap();
            for replace in [false, true] {
                let name = if replace { "replace" } else { "delete" };
                group.bench_function(BenchmarkId::new(format!("{name}_{count}"), bytes), |b| {
                    b.iter(|| {
                        let mut context = vex_editor::CommandContext::new(&mut editor);
                        context.character = Some('(');
                        if replace {
                            vex_editor::commands::surround_replace(&mut context).unwrap();
                            context.character = Some(']');
                            vex_editor::commands::surround_replace_finish(&mut context).unwrap();
                        } else {
                            vex_editor::commands::surround_delete(&mut context).unwrap();
                        }
                        editor.execute("undo", 1).unwrap();
                        black_box(editor.document().text());
                    });
                });
            }
        }
    }
    group.finish();
}

fn surround_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("surround_add_undo");
    for bytes in [1 << 20, 100 << 20] {
        let rope = Rope::from_str(&"word\n".repeat(bytes / 5));
        for (name, count) in [("whole_buffer", 1), ("1000_selections", 1000)] {
            let mut editor = editor(&rope, count);
            if count == 1 {
                editor.execute("select_all", 1).unwrap();
            }
            group.bench_function(BenchmarkId::new(name, bytes), |b| {
                b.iter(|| {
                    let mut context = vex_editor::CommandContext::new(&mut editor);
                    context.character = Some('(');
                    vex_editor::commands::surround_add(&mut context).unwrap();
                    editor.execute("undo", 1).unwrap();
                    black_box(editor.document().text());
                });
            });
        }
    }
    group.finish();
}

fn insert_repeat(c: &mut Criterion) {
    fn record(editor: &mut Editor) {
        editor.execute("insert_mode", 1).unwrap();
        for text in ["n", "e", "w", "_", "v", "a", "l", "u", "e"] {
            editor.insert_text(black_box(text)).unwrap();
        }
        editor.execute("normal_mode", 1).unwrap();
    }
    let mut group = c.benchmark_group("insert_repeat");
    for bytes in [1usize << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes.div_ceil(line.len())));
        for count in [1, 1_000] {
            let mut editor = editor(&rope, count);
            record(&mut editor);
            editor.execute("undo", 1).unwrap();
            for recording in [true, false] {
                let name = format!("{}_{}", if recording { "record" } else { "replay" }, count);
                group.bench_function(BenchmarkId::new(name, bytes), |b| {
                    b.iter(|| {
                        if recording {
                            record(&mut editor);
                        } else {
                            editor.execute("repeat_insert", 1).unwrap();
                        }
                        editor.execute("undo", 1).unwrap();
                        black_box(editor.document().text());
                    });
                });
            }
        }
    }
    group.finish();
}

fn registers(c: &mut Criterion) {
    use std::sync::Arc;
    let mut group = c.benchmark_group("registers");
    for bytes in [1usize << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let text = line.repeat(bytes.div_ceil(line.len()));
        let mut editor = Editor::new(Document::from(text.as_str()));
        editor
            .set_register('a', Arc::from([Arc::from(text)]))
            .unwrap();
        let mut keys = KeyHandler::default();
        group.bench_function(BenchmarkId::new("read", bytes), |b| {
            b.iter(|| black_box(editor.register(black_box('a')).unwrap()));
        });
        group.bench_function(BenchmarkId::new("helper_cancel", bytes), |b| {
            b.iter(|| {
                keys.handle(&mut editor, Key::Char('"')).unwrap();
                black_box(keys.hints());
                keys.handle(&mut editor, Key::Escape).unwrap();
            });
        });
        editor.execute("select_all", 1).unwrap();
        group.bench_function(BenchmarkId::new("discard_delete_undo", bytes), |b| {
            b.iter(|| {
                for ch in "\"_d".chars() {
                    keys.handle(&mut editor, Key::Char(ch)).unwrap();
                }
                editor.execute("undo", 1).unwrap();
                black_box(editor.document().text());
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
    targets = commands, search, background_search, comments, copy_selections, insert_line_kill, textobjects, delimiter_matching, surround_add, surround_edit, insert_repeat, registers
}
criterion_main!(benches);
