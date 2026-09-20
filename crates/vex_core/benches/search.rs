use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::{hint::black_box, time::Duration};
use vex_core::{
    ByteOffset, Rope,
    regex::{Cache, Options, Regex},
    search::{Direction, Literal},
};

fn search(c: &mut Criterion) {
    let mut group = c.benchmark_group("literal_search");
    for bytes in [1 << 20, 10 << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        let near = Literal::new("value");
        for direction in [Direction::Forward, Direction::Backward] {
            group.bench_with_input(
                BenchmarkId::new(format!("near_{direction:?}"), bytes),
                &rope,
                |b, rope| {
                    b.iter(|| {
                        black_box(
                            near.matches(
                                rope,
                                ByteOffset(0)..ByteOffset(rope.len_bytes()),
                                direction,
                            )
                            .next()
                            .unwrap(),
                        );
                    });
                },
            );
        }
        let missing = Literal::new("value_missing");
        for direction in [Direction::Forward, Direction::Backward] {
            group.bench_with_input(
                BenchmarkId::new(format!("missing_{direction:?}"), bytes),
                &rope,
                |b, rope| {
                    b.iter(|| {
                        assert!(
                            black_box(
                                missing
                                    .matches(
                                        rope,
                                        ByteOffset(0)..ByteOffset(rope.len_bytes()),
                                        direction
                                    )
                                    .next()
                            )
                            .is_none()
                        );
                    });
                },
            );
        }
    }
    group.finish();
}

fn regex_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("regex_search");
    for bytes in [1 << 20, 100 << 20] {
        let line = "fn main() { let value = 123; }\n";
        let rope = Rope::from_str(&line.repeat(bytes / line.len()));
        for (name, pattern) in [
            ("near", "value = [0-9]+"),
            ("missing", "value_missing[0-9]+"),
            ("word_boundary", r"\bvalue\b"),
        ] {
            let regex = Regex::new(pattern, Options::default()).unwrap();
            let mut cache = Cache::default();
            group.bench_function(BenchmarkId::new(name, bytes), |b| {
                b.iter(|| {
                    black_box(regex.find(
                        rope.slice(..),
                        ByteOffset(0)..ByteOffset(rope.len_bytes()),
                        &mut cache,
                        &|| false,
                    ));
                });
            });
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_millis(500));
    targets = search, regex_search
}
criterion_main!(benches);
