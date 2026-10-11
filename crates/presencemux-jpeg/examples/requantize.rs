//! Requantizes one JPEG frame.
//!
//! ```sh
//! cargo run --release -p presencemux-jpeg --example requantize -- IN OUT [QUALITY] [PARTS]
//! ```

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "an example prints its results"
)]

use std::process::ExitCode;

use presencemux_jpeg::{Encoded, Encoder, Error, Frame, Part, Tables, join};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = &args[..] else {
        eprintln!("usage: requantize IN OUT [QUALITY] [PARTS]");
        return ExitCode::FAILURE;
    };
    let quality = rest.first().map_or(Ok(80), |q| q.parse());
    let count = rest.get(1).map_or(Ok(3), |n| n.parse());
    let (Ok(quality), Ok(count @ 1..)) = (quality, count) else {
        eprintln!("QUALITY is 1 to 100, and PARTS is at least 1");
        return ExitCode::FAILURE;
    };
    let data = match std::fs::read(input) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cannot read {input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let out = match requantize(&data, quality, count) {
        Ok(out) => out,
        Err(error) => {
            eprintln!("cannot requantize {input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = std::fs::write(output, &out) {
        eprintln!("cannot write {output}: {error}");
        return ExitCode::FAILURE;
    }
    println!("{} bytes in, {} bytes out", data.len(), out.len());
    ExitCode::SUCCESS
}

/// Requantizes `data` to `quality` in `count` parts. Decodes the frame again
/// with one part if the parts do not synchronize.
fn requantize(data: &[u8], quality: u8, count: usize) -> Result<Vec<u8>, Error> {
    let mut tables = Tables::new();
    let frame = Frame::parse(data, &mut tables)?;
    let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
    for (index, part) in parts.iter_mut().enumerate() {
        part.decode(&frame, index, count);
    }
    let mut count = count;
    if matches!(join(&frame, &parts), Err(Error::NoSync)) {
        parts[0].decode(&frame, 0, 1);
        count = 1;
    }
    let coefficients = join(&frame, &parts[..count])?;
    let mut encoder = Encoder::new(quality);
    let prepared = encoder.prepare(&frame);
    let mut encoded: Vec<Encoded> = (0..count).map(|_| Encoded::new()).collect();
    for (index, out) in encoded.iter_mut().enumerate() {
        prepared.encode(&coefficients, index, out);
    }
    let mut out = Vec::new();
    prepared.write(&encoded, &mut out);
    Ok(out)
}
