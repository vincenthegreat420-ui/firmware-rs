use audio::audio_filter::{sample_to_f32, sample_to_u32};
use defmt::info;
use embassy_stm32::{peripherals, sai};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::zerocopy_channel;
use embassy_time::{Duration, WithTimeout as _};

use crate::*;

#[allow(unused)]
pub struct SaiResources<'d> {
    pub sai: peripherals::SAI1,

    pub sck: peripherals::PA8,
    pub sd: peripherals::PA10,
    pub fs: peripherals::PA9,
    pub dma: peripherals::DMA1_CH1,
    pub dma_buf: &'d mut [u32],
}

fn new_sai<'d>(resources: &'d mut SaiResources) -> sai::Sai<'d, peripherals::SAI1, u32> {
    let mut config = sai::Config::default();
    config.master_clock_divider = sai::MasterClockDivider::Div12;
    config.bit_order = sai::BitOrder::MsbFirst;
    config.data_size = sai::DataSize::Data32;
    config.frame_sync_active_level_length = sai::word::U7(32);
    config.frame_length = 64;

    let (sai_a, _sai_b) = sai::split_subblocks(&mut resources.sai);

    let sai = sai::Sai::new_asynchronous(
        sai_a,
        &mut resources.sck,
        &mut resources.sd,
        &mut resources.fs,
        &mut resources.dma,
        resources.dma_buf,
        config,
    );
    sai
}

#[embassy_executor::task]
pub async fn audio_routing_task(
    mut sai_resources: SaiResources<'static>,
    mut usb_audio_receiver: zerocopy_channel::Receiver<'static, NoopRawMutex, UsbSampleBlock>,
) {
    let mut volume = (0.0, 0.0);
    let mut sai_dac = new_sai(&mut sai_resources);

    loop {
        // Data should arrive at least once every millisecond.
        let result = usb_audio_receiver
            .receive()
            .with_timeout(Duration::from_millis(2))
            .await;

        if let Some(new_volume) = VOLUME_SIGNAL.try_take() {
            volume = new_volume;
        }

        let error = if let Ok(samples) = result {
            let mut processed_samples: Vec<u32, { USB_MAX_SAMPLE_COUNT }> = Vec::new();

            for (index, sample) in samples.iter().enumerate() {
                let sample_f32 = sample_to_f32(*sample);

                let sample_f32 = if index % 2 == 0 {
                    sample_f32 * volume.0
                } else {
                    sample_f32 * volume.1
                };

                let sample = sample_to_u32(sample_f32);

                processed_samples.push(sample).unwrap();
            }

            let result = sai_dac.write(&processed_samples).await;

            // Notify the channel that the buffer is now ready to be reused
            usb_audio_receiver.receive_done();

            result.is_err()
        } else {
            false
        };

        // Stop SAI in case of errors or stopped streaming.
        if error {
            info!("Stop SAI");

            drop(sai_dac);
            sai_dac = new_sai(&mut sai_resources);
        }
    }
}
