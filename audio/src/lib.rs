//! Audio processing library for embedded firmware.
//!
//! Provides audio filter chains, sample conversion utilities, and audio source
//! detection for USB audio applications.
#![no_std]
#![warn(missing_docs)]

pub mod audio_filter;

use micromath::F32Ext;

/// Errors that can occur during audio processing or device control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// I2C communication error with device.
    #[error("I2C communication error")]
    I2cError,
    /// Buffer overflow in audio processing.
    #[error("Audio buffer overflow")]
    BufferOverflow,
    /// Invalid sample rate requested.
    #[error("Invalid sample rate")]
    InvalidSampleRate,
    /// Invalid configuration provided.
    #[error("Invalid configuration")]
    InvalidConfiguration,
    /// Device initialization failed.
    #[error("Device initialization failed")]
    InitializationFailed,
}

/// Audio source type enumeration.
#[derive(Clone, Copy, PartialEq, Debug, defmt::Format)]
pub enum AudioSource {
    /// No active source.
    None,
    /// USB audio input.
    Usb,
    /// S/PDIF input.
    Spdif,
    /// External input.
    Ext,
    /// Raspberry Pi input.
    Rpi,
}

/// Type alias for biquad filter implementation.
pub type BiquadType = biquad::DirectForm2Transposed<f32>;

/// Type alias for audio filter with biquad implementation.
pub type AudioFilter<'d> = audio_filter::Filter<'d, BiquadType>;

/// Convert a gain in dB to linear scale.
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
