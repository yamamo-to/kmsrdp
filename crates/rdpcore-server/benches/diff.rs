use criterion::{Criterion, criterion_group, criterion_main};
use rdpcore_server::diff::find_dirty_rects;

fn dirty_rects_1080p_identical(c: &mut Criterion) {
    let width = 1920usize;
    let height = 1080usize;
    let stride = width * 4;
    let a = vec![0u8; stride * height];
    let b = a.clone();
    c.bench_function("diff_1080p_identical", |benches| {
        benches.iter(|| {
            find_dirty_rects(
                criterion::black_box(&a),
                stride,
                criterion::black_box(&b),
                stride,
                width,
                height,
                4,
            )
        })
    });
}

fn dirty_rects_1080p_one_tile(c: &mut Criterion) {
    let width = 1920usize;
    let height = 1080usize;
    let stride = width * 4;
    let a = vec![0u8; stride * height];
    let mut b = a.clone();
    b[stride * 100 + 400] = 1;
    c.bench_function("diff_1080p_one_tile", |benches| {
        benches.iter(|| {
            find_dirty_rects(
                criterion::black_box(&a),
                stride,
                criterion::black_box(&b),
                stride,
                width,
                height,
                4,
            )
        })
    });
}

criterion_group!(
    benches,
    dirty_rects_1080p_identical,
    dirty_rects_1080p_one_tile
);
criterion_main!(benches);
