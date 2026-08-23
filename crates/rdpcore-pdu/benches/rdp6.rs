use criterion::{Criterion, criterion_group, criterion_main};
use rdpcore_pdu::rdp6;

fn encode_64x64(c: &mut Criterion) {
    let pixels = vec![0x40u8; 64 * 64 * 4];
    c.bench_function("rdp6_encode_64x64", |b| {
        b.iter(|| rdp6::encode(criterion::black_box(&pixels), 64, 64))
    });
}

fn encode_256x256(c: &mut Criterion) {
    let pixels = vec![0x40u8; 256 * 256 * 4];
    c.bench_function("rdp6_encode_256x256", |b| {
        b.iter(|| rdp6::encode(criterion::black_box(&pixels), 256, 256))
    });
}

criterion_group!(benches, encode_64x64, encode_256x256);
criterion_main!(benches);
