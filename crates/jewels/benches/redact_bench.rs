use criterion::{Criterion, black_box, criterion_group, criterion_main};
use jewels::redact;

fn bench_redact(c: &mut Criterion) {
    let mut group = c.benchmark_group("redact");

    let text_no_secrets = "This is a plain text without any secrets. It is just used to benchmark the overhead of scanning text when no match occurs.";
    group.bench_function("no_secrets", |b| {
        b.iter(|| redact(black_box(text_no_secrets)))
    });

    let text_with_secret = "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef and AWS AKIA1234567890123456 here.";
    group.bench_function("with_secrets", |b| {
        b.iter(|| redact(black_box(text_with_secret)))
    });

    let large_text = text_no_secrets.repeat(100) + text_with_secret + &text_no_secrets.repeat(100);
    group.bench_function("large_text", |b| b.iter(|| redact(black_box(&large_text))));

    group.finish();
}

criterion_group!(benches, bench_redact);
criterion_main!(benches);
