//! Uniformly partitioned overlap-save FFT convolution (UPOLS).
//!
//! A multi-input / multi-output convolver: every output is
//! `Σ_in input[in] ∗ filter[in][out]`. Each filter is split into partitions
//! of `block` samples whose spectra (FFT size `2·block`) are precomputed.
//! Every input keeps a frequency-domain delay line of its last `P` block
//! spectra, so one block costs one forward FFT per input, `P` complex
//! multiply-accumulates per (input, output) pair, and one inverse FFT per
//! output — independent of filter length, which is what makes long HRIRs and
//! many ambisonic channels cheap. Latency is exactly `block` samples.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub struct PartitionedConvolver {
    block: usize,
    inputs: usize,
    outputs: usize,
    partitions: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// `filters[(in * outputs + out) * partitions + p]` → spectrum (2·block bins).
    filters: Vec<Vec<Complex32>>,
    /// Per input: ring of `partitions` spectra; `fdl_pos` is the newest.
    fdl: Vec<Vec<Vec<Complex32>>>,
    fdl_pos: usize,
    /// Per input: previous block + current block (time domain, 2·block).
    in_time: Vec<Vec<f32>>,
    /// Samples collected into the current block (per input).
    fill: usize,
    /// Per output: the last computed block, emitted while the next fills.
    out_block: Vec<Vec<f32>>,
    scratch: Vec<Complex32>,
    acc: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
}

impl PartitionedConvolver {
    /// `filters[in][out]` are impulse responses (any lengths).
    pub fn new(block: usize, filters: &[Vec<Vec<f32>>]) -> Self {
        let block = block.max(1).next_power_of_two();
        let inputs = filters.len();
        let outputs = filters.first().map_or(0, |f| f.len());
        let max_len = filters
            .iter()
            .flatten()
            .map(|h| h.len())
            .max()
            .unwrap_or(1)
            .max(1);
        let partitions = max_len.div_ceil(block);
        let n = 2 * block;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        let mut fft_scratch = vec![
            Complex32::default();
            fft.get_inplace_scratch_len()
                .max(ifft.get_inplace_scratch_len())
        ];
        let mut specs = Vec::with_capacity(inputs * outputs * partitions);
        for per_in in filters {
            assert_eq!(
                per_in.len(),
                outputs,
                "every input needs one filter per output"
            );
            for h in per_in {
                for p in 0..partitions {
                    let mut buf = vec![Complex32::default(); n];
                    let seg = h.iter().skip(p * block).take(block);
                    for (b, &v) in buf.iter_mut().zip(seg) {
                        b.re = v;
                    }
                    fft.process_with_scratch(&mut buf, &mut fft_scratch);
                    specs.push(buf);
                }
            }
        }
        PartitionedConvolver {
            block,
            inputs,
            outputs,
            partitions,
            fft,
            ifft,
            filters: specs,
            fdl: vec![vec![vec![Complex32::default(); n]; partitions]; inputs],
            fdl_pos: 0,
            in_time: vec![vec![0.0; n]; inputs],
            fill: 0,
            out_block: vec![vec![0.0; block]; outputs],
            scratch: vec![Complex32::default(); n],
            acc: vec![Complex32::default(); n],
            fft_scratch,
        }
    }

    pub fn block(&self) -> usize {
        self.block
    }

    /// Processing latency in samples.
    pub fn latency(&self) -> usize {
        self.block
    }

    pub fn reset(&mut self) {
        for d in &mut self.fdl {
            for s in d {
                s.fill(Complex32::default());
            }
        }
        for t in &mut self.in_time {
            t.fill(0.0);
        }
        for o in &mut self.out_block {
            o.fill(0.0);
        }
        self.fill = 0;
    }

    /// Process planar input (`input[in][i]`, all the same length) into
    /// planar output (`output[out]`, resized to the input length).
    pub fn process(&mut self, input: &[&[f32]], output: &mut [Vec<f32>]) {
        assert_eq!(input.len(), self.inputs);
        assert_eq!(output.len(), self.outputs);
        let len = input.first().map_or(0, |x| x.len());
        for o in output.iter_mut() {
            o.clear();
            o.reserve(len);
        }
        let b = self.block;
        let mut i = 0;
        while i < len {
            let take = (b - self.fill).min(len - i);
            for (k, x) in input.iter().enumerate() {
                self.in_time[k][b + self.fill..b + self.fill + take]
                    .copy_from_slice(&x[i..i + take]);
            }
            for (o, out) in output.iter_mut().enumerate() {
                out.extend_from_slice(&self.out_block[o][self.fill..self.fill + take]);
            }
            self.fill += take;
            i += take;
            if self.fill == b {
                self.compute_block();
                self.fill = 0;
            }
        }
    }

    fn compute_block(&mut self) {
        let b = self.block;
        let n = 2 * b;
        let p_count = self.partitions;
        self.fdl_pos = (self.fdl_pos + 1) % p_count;
        for k in 0..self.inputs {
            for (s, &t) in self.scratch.iter_mut().zip(&self.in_time[k]) {
                *s = Complex32::new(t, 0.0);
            }
            self.fft
                .process_with_scratch(&mut self.scratch, &mut self.fft_scratch);
            self.fdl[k][self.fdl_pos].copy_from_slice(&self.scratch);
            // Slide: current block becomes the "previous" half.
            self.in_time[k].copy_within(b.., 0);
        }
        let scale = 1.0 / n as f32;
        for o in 0..self.outputs {
            self.acc.fill(Complex32::default());
            for k in 0..self.inputs {
                for p in 0..p_count {
                    let x = &self.fdl[k][(self.fdl_pos + p_count - p) % p_count];
                    let h = &self.filters[(k * self.outputs + o) * p_count + p];
                    for ((a, xv), hv) in self.acc.iter_mut().zip(x).zip(h) {
                        *a += xv * hv;
                    }
                }
            }
            self.ifft
                .process_with_scratch(&mut self.acc, &mut self.fft_scratch);
            // Overlap-save: the second half is the valid linear convolution.
            for (dst, src) in self.out_block[o].iter_mut().zip(&self.acc[b..]) {
                *dst = src.re * scale;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(x: &[f32], h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0; x.len()];
        for (n, yn) in y.iter_mut().enumerate() {
            for (k, &hk) in h.iter().enumerate() {
                if n >= k {
                    *yn += hk * x[n - k];
                }
            }
        }
        y
    }

    #[test]
    fn matches_direct_convolution_mimo() {
        let mut seed = 1u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let x0: Vec<f32> = (0..1000).map(|_| rnd()).collect();
        let x1: Vec<f32> = (0..1000).map(|_| rnd()).collect();
        let h: Vec<Vec<Vec<f32>>> = (0..2)
            .map(|_| (0..2).map(|_| (0..150).map(|_| rnd()).collect()).collect())
            .collect();
        let mut c = PartitionedConvolver::new(32, &h);
        let mut out = vec![Vec::new(), Vec::new()];
        let mut got = [Vec::new(), Vec::new()];
        // Feed in odd-sized chunks to exercise block buffering.
        let mut i = 0;
        for chunk in [7usize, 100, 1, 300, 592] {
            c.process(&[&x0[i..i + chunk], &x1[i..i + chunk]], &mut out);
            for (g, o) in got.iter_mut().zip(&out) {
                g.extend_from_slice(o);
            }
            i += chunk;
        }
        for o in 0..2 {
            let a = direct(&x0, &h[0][o]);
            let b = direct(&x1, &h[1][o]);
            let lat = c.latency();
            for n in 0..1000 - lat {
                let want = a[n] + b[n];
                assert!(
                    (got[o][n + lat] - want).abs() < 1e-4,
                    "out {o} n {n}: {} vs {want}",
                    got[o][n + lat]
                );
            }
        }
    }

    #[test]
    fn identity_filter_delays_by_block() {
        let mut c = PartitionedConvolver::new(64, &[vec![vec![1.0]]]);
        let x: Vec<f32> = (0..256).map(|i| i as f32).collect();
        let mut out = vec![Vec::new()];
        c.process(&[&x], &mut out);
        assert!(out[0][..64].iter().all(|&v| v == 0.0));
        for (i, &v) in out[0].iter().enumerate().skip(64) {
            assert!((v - (i - 64) as f32).abs() < 1e-3);
        }
    }
}
