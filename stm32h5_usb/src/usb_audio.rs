//! Handles USB audio communication and sample forwarding.
use audio::db_to_linear;
use defmt::{debug, error, info};
use embassy_stm32::{peripherals, usb};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel};
use embassy_usb::{
    class::uac1::{self, speaker},
    driver::EndpointError,
};
use heapless::Vec;

use crate::{
    SampleBlock, AUDIO_CHANNELS, BLINK_SIGNAL, FEEDBACK_REFRESH_PERIOD, FEEDBACK_SHIFT, FEEDBACK_SIGNAL,
    SAMPLE_BLOCK_COUNT, SAMPLE_SIZE, TICKS_PER_SAMPLE, USB_MAX_PACKET_SIZE, VOLUME_SIGNAL,
};

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

/// Sends feedback messages to the host.
async fn feedback_handler<'d, T: usb::Instance + 'd>(
    feedback: &mut speaker::Feedback<'d, usb::Driver<'d, T>>,
    feedback_factor: f64,
) -> Result<(), Disconnected> {
    let mut packet: Vec<u8, 4> = Vec::new();

    // Ignore initial feedback signal, which is incorrect.
    let _ = FEEDBACK_SIGNAL.wait().await;

    // Collects the fractional component of the feedback value that is lost by rounding.
    let mut fractional_accumulator = 0.0_f64;
    let mut value = 0;

    loop {
        if let Some(counter) = FEEDBACK_SIGNAL.try_take() {
            let raw_value = counter as f64 * feedback_factor + fractional_accumulator;
            value = raw_value as u32;
            fractional_accumulator = raw_value - value as f64;
            debug!("Feedback: {}", value);
        }

        packet.clear();

        packet.push(value as u8).unwrap();
        packet.push((value >> 8) as u8).unwrap();
        packet.push((value >> 16) as u8).unwrap();

        feedback.write_packet(&packet).await?;
    }
}

/// Handles streaming of audio data from the host.
async fn stream_handler<'d, T: usb::Instance + 'd>(
    stream: &mut speaker::Stream<'d, usb::Driver<'d, T>>,
    audio_channel_sender: &mut channel::Sender<'static, NoopRawMutex, SampleBlock, SAMPLE_BLOCK_COUNT>,
) -> Result<(), Disconnected> {
    loop {
        let mut usb_data = [0u8; USB_MAX_PACKET_SIZE];
        let data_size = stream.read_packet(&mut usb_data).await?;

        let word_count = data_size / SAMPLE_SIZE;

        if word_count * SAMPLE_SIZE == data_size {
            let mut samples: SampleBlock = Vec::new();

            for w in 0..word_count {
                let byte_offset = w * SAMPLE_SIZE;
                let sample = u32::from_le_bytes(usb_data[byte_offset..byte_offset + SAMPLE_SIZE].try_into().unwrap());

                // Fill the sample buffer with data.
                samples.push(sample).unwrap();
            }
            if audio_channel_sender.try_send(samples).is_err() {
                // error!("USB: Failed to send to channel (consumption too slow)");
                BLINK_SIGNAL.signal(crate::Blink::Yellow);
            }
        } else {
            error!("USB: Invalid USB buffer size of {}, skipped", data_size);
        }
    }
}

/// Receives audio samples from the host.
#[embassy_executor::task]
pub async fn streaming_task(
    mut stream: speaker::Stream<'static, usb::Driver<'static, peripherals::USB>>,
    mut sender: channel::Sender<'static, NoopRawMutex, SampleBlock, SAMPLE_BLOCK_COUNT>,
) {
    loop {
        stream.wait_connection().await;
        info!("USB audio connected.");
        _ = stream_handler(&mut stream, &mut sender).await;
        info!("USB audio disconnected.");
    }
}

/// Sends sample rate feedback to the host.
///
/// The `feedback_factor` scales the feedback timer's counter value so that the result is the number of samples that
/// this device played back or "consumed" during one SOF period (1 ms) - in 10.14 format.
///
/// Ideally, the `feedback_factor` that is calculated below would be an integer for avoiding numerical errors.
/// This is achieved by having `TICKS_PER_SAMPLE` be a power of two. For audio applications at a sample rate of 48 kHz,
/// 24.576 MHz would be one such option.
#[embassy_executor::task]
pub async fn feedback_task(mut feedback: speaker::Feedback<'static, usb::Driver<'static, peripherals::USB>>) {
    let feedback_factor =
        ((1 << FEEDBACK_SHIFT) as f64 / TICKS_PER_SAMPLE) / FEEDBACK_REFRESH_PERIOD.frame_count() as f64;
    info!("Feedback factor: {}", feedback_factor);

    loop {
        feedback.wait_connection().await;
        _ = feedback_handler(&mut feedback, feedback_factor).await;
    }
}

#[embassy_executor::task]
pub async fn device_task(mut usb_device: embassy_usb::UsbDevice<'static, usb::Driver<'static, peripherals::USB>>) {
    usb_device.run().await;
}

/// The USB control task.
///
/// Provides
/// - Volume adjustment
/// - Sample rate adjustment (not used, is fixed)
/// - Sample width adjustment (not used, is fixed)
#[embassy_executor::task]
pub async fn control_task(control_monitor: speaker::ControlMonitor<'static>) {
    loop {
        control_monitor.changed().await;

        let mut usb_gain_left = 0.0_f32;
        let mut usb_gain_right = 0.0_f32;

        for channel in AUDIO_CHANNELS {
            let volume = control_monitor.volume(channel).unwrap();

            let gain = match volume {
                speaker::Volume::Muted => 0.0,
                speaker::Volume::DeciBel(volume_db) => {
                    if volume_db > 0.0 {
                        panic!("Volume must not be positive.")
                    }

                    db_to_linear(volume_db)
                }
            };

            match channel {
                uac1::Channel::LeftFront => {
                    usb_gain_left = gain;
                }
                uac1::Channel::RightFront => {
                    usb_gain_right = gain;
                }
                _ => (),
            }
        }

        VOLUME_SIGNAL.signal((usb_gain_left, usb_gain_right));
    }
}
