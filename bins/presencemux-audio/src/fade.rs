//! The gain ramp of the gate. It runs in the real-time callback. It does not
//! allocate.

/// The length of a ramp in samples, 5 ms at 48 kHz.
pub(crate) const RAMP: u32 = 240;

/// What the fader does in one cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The gain rises to 1 with the ramp.
    Open,
    /// The gain falls to 0 with the ramp.
    Close,
    /// The gain is 0 at once.
    Cut,
}

/// A gain from 0 to 1 that follows the gate with a linear ramp.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fader {
    /// The position in the ramp. 0 is closed, [`RAMP`] is open.
    position: u32,
}

impl Fader {
    /// Writes `input` to `output` with the gain of `mode`. A gain of 0 writes
    /// `0.0`. Samples of `output` after the end of `input` get `0.0`.
    pub(crate) fn process(&mut self, input: &[f32], output: &mut [f32], mode: Mode) {
        let (output, rest) = output.split_at_mut(input.len().min(output.len()));
        rest.fill(0.0);
        let open = match mode {
            Mode::Open => true,
            Mode::Close => false,
            Mode::Cut => {
                self.silence(output);
                return;
            }
        };
        match (open, self.position) {
            (true, RAMP) => output.copy_from_slice(&input[..output.len()]),
            (false, 0) => output.fill(0.0),
            _ => {
                for (out, &sample) in output.iter_mut().zip(input) {
                    self.step(open);
                    *out = if self.position == 0 {
                        0.0
                    } else {
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "the position is at most RAMP, which f32 holds exactly"
                        )]
                        let gain = self.position as f32 / RAMP as f32;
                        sample * gain
                    };
                }
            }
        }
    }

    /// Returns whether the gain is 0.
    pub(crate) fn is_closed(self) -> bool {
        self.position == 0
    }

    /// Writes `0.0` to `output` and sets the gain to 0 at once.
    pub(crate) fn silence(&mut self, output: &mut [f32]) {
        self.position = 0;
        output.fill(0.0);
    }

    /// Moves the position one sample toward open or closed.
    fn step(&mut self, open: bool) {
        self.position = if open {
            (self.position + 1).min(RAMP)
        } else {
            self.position.saturating_sub(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number of samples in one test buffer.
    const SAMPLES: u16 = 512;

    /// Returns whether each sample has the bits of `value`. `0.0` does not
    /// match `-0.0`.
    fn all_bits(samples: &[f32], value: f32) -> bool {
        samples
            .iter()
            .all(|sample| sample.to_bits() == value.to_bits())
    }

    fn filled(value: f32) -> Vec<f32> {
        vec![value; usize::from(SAMPLES)]
    }

    /// Returns a fader after `samples` samples while open.
    fn opened(samples: usize) -> Fader {
        let mut fader = Fader::default();
        let mut output = vec![0.0; samples];
        fader.process(&vec![1.0; samples], &mut output, Mode::Open);
        fader
    }

    #[test]
    fn closed_fader_writes_exact_zeros() {
        let mut fader = Fader::default();
        let mut output = filled(9.0);

        fader.process(&filled(1.0), &mut output, Mode::Close);

        assert!(all_bits(&output, 0.0));
    }

    #[test]
    fn closed_fader_writes_positive_zeros_for_negative_input() {
        let mut fader = Fader::default();
        let mut output = filled(9.0);

        fader.process(&filled(-1.0), &mut output, Mode::Close);

        assert!(all_bits(&output, 0.0));
    }

    #[test]
    fn closed_fader_writes_zeros_for_nan_input() {
        let mut fader = Fader::default();
        let mut output = filled(9.0);

        fader.process(&filled(f32::NAN), &mut output, Mode::Close);

        assert!(all_bits(&output, 0.0));
    }

    #[test]
    fn opening_fader_follows_the_ramp_up() {
        let mut fader = Fader::default();
        let mut output = [0.0; 240];

        fader.process(&[1.0; 240], &mut output, Mode::Open);

        assert_eq!(
            [output[0], output[119], output[239]],
            [1.0 / 240.0, 0.5, 1.0]
        );
    }

    #[test]
    fn open_fader_after_the_ramp_copies_the_input() {
        let mut fader = opened(usize::from(SAMPLES));
        let input: Vec<f32> = (0..SAMPLES).map(|n| f32::from(n).sin()).collect();
        let mut output = filled(0.0);

        fader.process(&input, &mut output, Mode::Open);

        assert_eq!(output, input);
    }

    #[test]
    fn closing_fader_follows_the_ramp_down() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = filled(9.0);

        fader.process(&filled(1.0), &mut output, Mode::Close);

        assert_eq!(
            [output[0], output[119], output[238]],
            [239.0 / 240.0, 0.5, 1.0 / 240.0]
        );
    }

    #[test]
    fn closing_fader_writes_exact_zeros_after_the_ramp() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = filled(9.0);

        fader.process(&filled(1.0), &mut output, Mode::Close);

        assert!(all_bits(&output[239..], 0.0));
    }

    #[test]
    fn close_during_the_opening_ramp_falls_from_the_current_gain() {
        let mut fader = opened(100);
        let mut output = [9.0];

        fader.process(&[1.0], &mut output, Mode::Close);

        assert_eq!(output, [99.0 / 240.0]);
    }

    #[test]
    fn cut_on_an_open_fader_writes_exact_zeros_at_once() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = filled(9.0);

        fader.process(&filled(1.0), &mut output, Mode::Cut);

        assert!(all_bits(&output, 0.0));
    }

    #[test]
    fn opening_after_a_cut_starts_the_ramp_again() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = [9.0];
        fader.process(&[1.0], &mut output, Mode::Cut);

        fader.process(&[1.0], &mut output, Mode::Open);

        assert_eq!(output, [1.0 / 240.0]);
    }

    #[test]
    fn opening_ramp_continues_across_buffers() {
        let mut fader = opened(100);
        let mut output = [0.0];

        fader.process(&[1.0], &mut output, Mode::Open);

        assert_eq!(output, [101.0 / 240.0]);
    }

    #[test]
    fn output_longer_than_the_input_ends_with_zeros() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = filled(9.0);

        fader.process(&filled(1.0)[..100], &mut output, Mode::Open);

        assert!(all_bits(&output[100..], 0.0));
    }

    #[test]
    fn output_shorter_than_the_input_gets_only_its_length() {
        let mut fader = opened(usize::from(SAMPLES));
        let mut output = [9.0; 4];

        fader.process(&[1.0; 8], &mut output, Mode::Open);

        assert_eq!(output, [1.0; 4]);
    }
}
