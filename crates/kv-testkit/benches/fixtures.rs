//! Smoke benchmark for the regression `benchmarks` job: rendering a click
//! track in memory. Real performance benchmarks arrive with their features
//! (auto-edit regenerate, export, ...); see `docs/roadmap.md`.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn render_click_track(c: &mut Criterion) {
    c.bench_function("render_click_track_wav 120bpm 4s 48k", |b| {
        b.iter(|| kv_testkit::render_click_track_wav(black_box(120.0), 4.0, 48_000))
    });
}

criterion_group!(benches, render_click_track);
criterion_main!(benches);
