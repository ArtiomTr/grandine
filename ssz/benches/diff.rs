use criterion::{Criterion, criterion_group, criterion_main};

fn state_diff(c: &mut Criterion) {}

criterion_group!(benches, state_diff);
criterion_main!(benches);
