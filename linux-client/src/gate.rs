//! Mono 48 kHz noise gate. Runs before Opus; no locks or allocations per sample.
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

const RATE: usize = 48_000;
const WINDOW: usize = RATE / 200; // 5 ms RMS detector
const HOLD: usize = RATE * 150 / 1000;
const ATTACK_STEP: f32 = 1.0 / (RATE as f32 * 0.005);
const RELEASE_STEP: f32 = 1.0 / (RATE as f32 * 0.050);

pub struct Settings {
    enabled: AtomicBool,
    threshold_db: AtomicI32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            threshold_db: AtomicI32::new(-45),
        }
    }
}

impl Settings {
    pub fn enable(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn threshold(&self, db: i32) {
        self.threshold_db
            .store(db.clamp(-80, -10), Ordering::Relaxed);
    }
}

pub struct Gate {
    powers: [f64; WINDOW],
    position: usize,
    total: f64,
    hold: usize,
    gain: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            powers: [0.0; WINDOW],
            position: 0,
            total: 0.0,
            hold: 0,
            gain: 0.0,
        }
    }
}

impl Gate {
    /// Audio caps force little-endian f32; sample history survives buffer boundaries.
    pub fn process(&mut self, bytes: &mut [u8], settings: &Settings) {
        if !settings.enabled.load(Ordering::Relaxed) {
            self.powers.fill(0.0);
            self.total = 0.0;
            self.hold = 0;
            self.gain = 1.0;
            return;
        }
        let db = settings.threshold_db.load(Ordering::Relaxed);
        let open_power = 10_f64.powf(db as f64 / 10.0);
        let close_power = open_power * 10_f64.powf(-3.0 / 10.0); // hysteresis
        for chunk in bytes.chunks_exact_mut(4) {
            let input = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let input = if input.is_finite() { input } else { 0.0 };
            let power = (input as f64).powi(2);
            self.total = (self.total + power - self.powers[self.position]).max(0.0);
            self.powers[self.position] = power;
            self.position = (self.position + 1) % WINDOW;
            let level = self.total / WINDOW as f64;
            let threshold = if self.hold > 0 {
                close_power
            } else {
                open_power
            };
            if level >= threshold {
                self.hold = HOLD;
            } else {
                self.hold = self.hold.saturating_sub(1);
            }
            if self.hold > 0 {
                self.gain = (self.gain + ATTACK_STEP).min(1.0);
            } else {
                self.gain = (self.gain - RELEASE_STEP).max(0.0);
            }
            chunk.copy_from_slice(&(input * self.gain).to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(amplitude: f32, length: usize) -> Vec<u8> {
        (0..length)
            .flat_map(|n| (amplitude * (n as f32 * 0.13).sin()).to_le_bytes())
            .collect()
    }
    fn peak(bytes: &[u8]) -> f32 {
        bytes
            .chunks_exact(4)
            .map(|s| f32::from_le_bytes(s.try_into().unwrap()).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn disabled_is_bit_exact_and_quiet_noise_is_suppressed() {
        let settings = Settings::default();
        let mut gate = Gate::default();
        let mut noise = audio(0.001, RATE / 2);
        let original = noise.clone();
        gate.process(&mut noise, &settings);
        assert_eq!(noise, original);
        settings.enable(true);
        gate.process(&mut noise, &settings);
        assert_eq!(peak(&noise[4 * RATE / 10..]), 0.0);
    }

    #[test]
    fn speech_opens_gate_and_hold_prevents_chopping() {
        let settings = Settings::default();
        settings.enable(true);
        let mut gate = Gate::default();
        let mut speech = audio(0.2, RATE / 10);
        gate.process(&mut speech, &settings);
        assert!(peak(&speech[4 * RATE / 100..]) > 0.19);
        let mut quiet = audio(0.001, RATE / 2);
        gate.process(&mut quiet, &settings);
        assert!(peak(&quiet[..4 * RATE / 10]) > 0.0009);
        assert_eq!(peak(&quiet[4 * RATE * 3 / 10..]), 0.0);
        // No discontinuous gain jumps during release.
        let mut previous = 1.0;
        let mut closing = Gate::default();
        closing.gain = 1.0;
        let mut input = vec![0.001_f32.to_le_bytes(); RATE / 10].concat();
        closing.process(&mut input, &settings);
        for sample in input.chunks_exact(4) {
            let gain = f32::from_le_bytes(sample.try_into().unwrap()) / 0.001;
            assert!((gain - previous).abs() < 0.001);
            previous = gain;
        }
    }

    #[test]
    fn live_threshold_change_and_buffer_splitting() {
        let settings = Settings::default();
        settings.enable(true);
        settings.threshold(-10);
        let original = audio(0.02, RATE / 2);
        let mut gate = Gate::default();
        let mut blocked = original.clone();
        gate.process(&mut blocked, &settings);
        assert_eq!(peak(&blocked), 0.0);
        settings.threshold(-60);
        let mut loud = original.clone();
        gate.process(&mut loud, &settings);
        assert!(peak(&loud) > 0.019);
        let mut whole = original.clone();
        let mut split = original;
        Gate::default().process(&mut whole, &settings);
        let mut other = Gate::default();
        for chunk in split.chunks_mut(4 * 127) {
            other.process(chunk, &settings);
        }
        assert_eq!(split, whole);
    }
}
