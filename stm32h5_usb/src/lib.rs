//! Audio playback libraries.
#![no_std]

use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex},
    signal::Signal,
};
use embassy_usb::class::uac1;
use heapless::Vec;

pub mod audio_routing;
pub mod usb_audio;

/// The number of sample blocks that exist.
pub const SAMPLE_BLOCK_COUNT: usize = 2;

// A counter signal that is written by the feedback timer, once every `FEEDBACK_REFRESH_PERIOD`.
// At that point, a feedback value is sent to the host.
pub static FEEDBACK_SIGNAL: Signal<CriticalSectionRawMutex, u32> = Signal::new();

/// Signals volume changes.
pub static VOLUME_SIGNAL: Signal<ThreadModeRawMutex, (f32, f32)> = Signal::new();

/// Signals the LED to blink.
pub enum Blink {
    Red,
    Yellow,
    Green,
}
pub static BLINK_SIGNAL: Signal<ThreadModeRawMutex, Blink> = Signal::new();

// Stereo
pub const CHANNEL_COUNT: usize = 2;

// This example uses a fixed sample rate of 48 kHz.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const FEEDBACK_COUNTER_TICK_RATE: u32 = 24_576_000;

// Use 32 bit samples, which allow for a lot of (software) volume adjustment without degradation of quality.
pub const SAMPLE_WIDTH: uac1::SampleWidth = uac1::SampleWidth::Width4Byte;
pub const SAMPLE_WIDTH_BIT: usize = SAMPLE_WIDTH.in_bit();
pub const SAMPLE_SIZE: usize = SAMPLE_WIDTH as usize;
pub const SAMPLE_SIZE_PER_S: usize = (SAMPLE_RATE_HZ as usize) * CHANNEL_COUNT * SAMPLE_SIZE;

// Size of audio samples per 1 ms - for the full-speed USB frame period of 1 ms.
pub const USB_FRAME_SIZE: usize = SAMPLE_SIZE_PER_S.div_ceil(1000);

// Select front left and right audio channels.
pub const AUDIO_CHANNELS: [uac1::Channel; CHANNEL_COUNT] = [uac1::Channel::LeftFront, uac1::Channel::RightFront];

// Some margin for feedback (8 samples).
pub const USB_MAX_PACKET_SIZE: usize = 8 * CHANNEL_COUNT * SAMPLE_SIZE + USB_FRAME_SIZE;
pub const USB_MAX_SAMPLE_COUNT: usize = USB_MAX_PACKET_SIZE / SAMPLE_SIZE;

// The data type that is exchanged via the zero-copy channel (a sample vector).
pub type SampleBlock = Vec<u32, USB_MAX_SAMPLE_COUNT>;

// Feedback is provided in 10.14 format for full-speed endpoints.
pub const FEEDBACK_REFRESH_PERIOD: uac1::FeedbackRefresh = uac1::FeedbackRefresh::Period32Frames;
const FEEDBACK_SHIFT: usize = 14;

const TICKS_PER_SAMPLE: f64 = (FEEDBACK_COUNTER_TICK_RATE as f64) / (SAMPLE_RATE_HZ as f64);
