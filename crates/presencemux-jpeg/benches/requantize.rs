//! Benchmarks of the requantization of saved camera frames.
//!
//! ```sh
//! PRESENCEMUX_JPEG_FRAMES=DIR cargo bench -p presencemux-jpeg
//! ```
//!
//! Each step runs with 1 to 4 parts, in turn on one thread (`sequential`) and
//! on one thread for each part (`threads`). The threads start for each frame.
//! `--save-baseline NAME` keeps the results, and `--baseline NAME` compares
//! with them.

#![expect(clippy::print_stderr, reason = "the benchmark reports missing frames")]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use presencemux_jpeg::{Coefficients, Encoded, Encoder, Frame, Part, Prepared, Tables, join};

/// The number of saved frames that the benchmarks use in turn.
const FRAMES: usize = 10;
const PARTS: [usize; 4] = [1, 2, 3, 4];

/// Returns the saved frames, or `None` without `PRESENCEMUX_JPEG_FRAMES`.
fn frames() -> Option<Vec<Vec<u8>>> {
    let dir = std::env::var("PRESENCEMUX_JPEG_FRAMES").ok()?;
    let frames: Vec<Vec<u8>> = (0..FRAMES)
        .map_while(|i| std::fs::read(format!("{dir}/f{i:03}.jpg")).ok())
        .collect();
    (!frames.is_empty()).then_some(frames)
}

/// Decodes the parts of `frame`, each on its own thread with `threaded`.
fn decode_parts(frame: &Frame<'_>, parts: &mut [Part], threaded: bool) {
    let count = parts.len();
    if threaded {
        std::thread::scope(|scope| {
            for (index, part) in parts.iter_mut().enumerate() {
                scope.spawn(move || part.decode(frame, index, count));
            }
        });
    } else {
        for (index, part) in parts.iter_mut().enumerate() {
            part.decode(frame, index, count);
        }
    }
}

/// Encodes the parts of `coefficients`, each on its own thread with
/// `threaded`.
fn encode_parts(
    prepared: &Prepared<'_>,
    coefficients: &Coefficients<'_>,
    encoded: &mut [Encoded],
    threaded: bool,
) {
    if threaded {
        std::thread::scope(|scope| {
            for (index, out) in encoded.iter_mut().enumerate() {
                scope.spawn(move || prepared.encode(coefficients, index, out));
            }
        });
    } else {
        for (index, out) in encoded.iter_mut().enumerate() {
            prepared.encode(coefficients, index, out);
        }
    }
}

/// Returns the name of a benchmark for `threaded`.
fn mode(threaded: bool) -> &'static str {
    if threaded { "threads" } else { "sequential" }
}

fn benchmarks(c: &mut Criterion) {
    let Some(frames) = frames() else {
        eprintln!("set PRESENCEMUX_JPEG_FRAMES to the directory of the saved camera frames");
        return;
    };
    let mut tables = Tables::new();
    let mut encoder = Encoder::new(80);

    let mut group = c.benchmark_group("decode");
    for threaded in [false, true] {
        for count in PARTS {
            let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
            let mut next = frames.iter().cycle();
            group.bench_function(BenchmarkId::new(mode(threaded), count), |b| {
                b.iter(|| {
                    let frame = Frame::parse(next.next().unwrap(), &mut tables).unwrap();
                    decode_parts(&frame, &mut parts, threaded);
                    black_box(join(&frame, &parts).unwrap());
                });
            });
        }
    }
    group.finish();

    let mut group = c.benchmark_group("encode");
    for threaded in [false, true] {
        for count in PARTS {
            let frame = Frame::parse(&frames[0], &mut tables).unwrap();
            let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
            decode_parts(&frame, &mut parts, false);
            let coefficients = join(&frame, &parts).unwrap();
            let prepared = encoder.prepare(&frame);
            let mut encoded: Vec<Encoded> = (0..count).map(|_| Encoded::new()).collect();
            group.bench_function(BenchmarkId::new(mode(threaded), count), |b| {
                b.iter(|| encode_parts(&prepared, &coefficients, &mut encoded, threaded));
            });
        }
    }
    group.finish();

    let mut group = c.benchmark_group("write");
    for count in PARTS {
        let frame = Frame::parse(&frames[0], &mut tables).unwrap();
        let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
        decode_parts(&frame, &mut parts, false);
        let coefficients = join(&frame, &parts).unwrap();
        let prepared = encoder.prepare(&frame);
        let mut encoded: Vec<Encoded> = (0..count).map(|_| Encoded::new()).collect();
        encode_parts(&prepared, &coefficients, &mut encoded, false);
        let mut out = Vec::new();
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter(|| prepared.write(&encoded, &mut out));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("frame");
    for threaded in [false, true] {
        for count in PARTS {
            let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
            let mut encoded: Vec<Encoded> = (0..count).map(|_| Encoded::new()).collect();
            let mut out = Vec::new();
            let mut next = frames.iter().cycle();
            group.bench_function(BenchmarkId::new(mode(threaded), count), |b| {
                b.iter(|| {
                    let frame = Frame::parse(next.next().unwrap(), &mut tables).unwrap();
                    decode_parts(&frame, &mut parts, threaded);
                    let coefficients = join(&frame, &parts).unwrap();
                    let prepared = encoder.prepare(&frame);
                    encode_parts(&prepared, &coefficients, &mut encoded, threaded);
                    prepared.write(&encoded, &mut out);
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
