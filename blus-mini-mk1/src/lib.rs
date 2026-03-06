//! Blus Mini Mk1 firmware library.
//!
//! Provides USB audio class 1.0 implementation and I2S audio routing
//! for the Blus Mini Mk1 hardware platform.
#![no_std]
#![warn(missing_docs)]

pub mod audio_routing;
pub mod usb_audio;

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::signal::Signal;
use embassy_usb::class::uac1;
use heapless::Vec;

/// Number of input audio channels (stereo)
pub const INPUT_CHANNEL_COUNT: usize = 2;

/// Audio sample rate in Hz
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Feedback counter tick rate (derived from external oscillator)
pub const FEEDBACK_COUNTER_TICK_RATE: u32 = 24_576_000 / 2;

/// Sample width for USB audio
pub const SAMPLE_WIDTH: uac1::SampleWidth = uac1::SampleWidth::Width4Byte;

/// Sample width in bits
pub const SAMPLE_WIDTH_BIT: usize = SAMPLE_WIDTH.in_bit();

/// Sample size in bytes
pub const SAMPLE_SIZE: usize = SAMPLE_WIDTH as usize;

/// Total sample size per second
pub const SAMPLE_SIZE_PER_S: usize = (SAMPLE_RATE_HZ as usize) * INPUT_CHANNEL_COUNT * SAMPLE_SIZE;

/// Sample size per millisecond
pub const SAMPLE_SIZE_PER_MS: usize = SAMPLE_SIZE_PER_S.div_ceil(1000);

/// Audio channel enumeration for USB
pub const AUDIO_CHANNELS: [uac1::Channel; INPUT_CHANNEL_COUNT] = [uac1::Channel::LeftFront, uac1::Channel::RightFront];

/// Size of audio samples per 1 ms - suitable for full-speed USB
pub const USB_FRAME_SIZE: usize = SAMPLE_SIZE_PER_MS;

/// Feedback refresh period (8 ms)
pub const FEEDBACK_REFRESH_PERIOD: uac1::FeedbackRefresh = uac1::FeedbackRefresh::Period8Frames;

/// Maximum USB packet size (factor of two as margin)
pub const USB_MAX_PACKET_SIZE: usize = 2 * USB_FRAME_SIZE;

/// Maximum sample count per USB packet
pub const USB_MAX_SAMPLE_COUNT: usize = USB_MAX_PACKET_SIZE / SAMPLE_SIZE;

/// Signal for USB feedback timing
pub static FEEDBACK_SIGNAL: Signal<CriticalSectionRawMutex, u32> = Signal::new();

/// Signal for I2S active state
pub static I2S_ACTIVE_SIGNAL: Signal<ThreadModeRawMutex, bool> = Signal::new();

/// Signal for volume changes from USB host
pub static VOLUME_SIGNAL: Signal<ThreadModeRawMutex, (f32, f32)> = Signal::new();

/// Type alias for USB sample block
pub type UsbSampleBlock = Vec<u32, USB_MAX_SAMPLE_COUNT>;
