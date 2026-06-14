//! Audio playback and routing.
use audio::{
    audio_filter::{sample_to_f32, sample_to_u32},
    AudioFilter,
};
use defmt::{error, info};
use embassy_futures::select::{select, Either};
use embassy_stm32::{
    peripherals,
    sai::{self, word, BitOrder, ClockStrobe, Sai},
    Peri,
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel};
use heapless::Vec;
use static_cell::StaticCell;

use crate::{
    SampleBlock, BLINK_SIGNAL, CHANNEL_COUNT, SAMPLE_BLOCK_COUNT, SAMPLE_WIDTH_BIT, USB_MAX_SAMPLE_COUNT, VOLUME_SIGNAL,
};

static _FILTERS: StaticCell<Vec<AudioFilter<'_>, 10>> = StaticCell::new();

/// Resources that are required for instantiating SAI1.
#[allow(missing_docs)]
pub struct SaiResources {
    pub sai: Peri<'static, peripherals::SAI1>,
    pub sck_a: Peri<'static, peripherals::PE5>,
    pub sd_a: Peri<'static, peripherals::PE6>,
    pub fs_a: Peri<'static, peripherals::PE4>,
    pub dma_a: Peri<'static, peripherals::GPDMA1_CH1>,
}

fn new_sai<'d>(write_buffer: &'d mut [u32], resources: &'d mut SaiResources) -> Sai<'d, peripherals::SAI1, u32> {
    let (sai_a, _) = sai::split_subblocks(resources.sai.reborrow());
    // I2S compatible.
    let mut config = sai::Config::default();
    config.bit_order = BitOrder::MsbFirst;
    config.slot_count = sai::word::U4(CHANNEL_COUNT as u8);
    config.frame_sync_active_level_length = word::U7(SAMPLE_WIDTH_BIT as u8);
    config.data_size = sai::DataSize::Data32;
    config.frame_length = (CHANNEL_COUNT * SAMPLE_WIDTH_BIT) as u8;
    config.master_clock_divider = sai::MasterClockDivider::Div2;
    config.clock_strobe = ClockStrobe::Falling;

    sai::Sai::new_asynchronous(
        sai,
        resources.sck_a.reborrow(),
        resources.sd_a.reborrow(),
        resources.fs_a.reborrow(),
        resources.dma_a.reborrow(),
        write_buffer,
        config,
    )
}

/// Receives audio samples from the USB streaming task and can play them back.
#[embassy_executor::task]
pub async fn audio_receiver_task(
    receiver: channel::Receiver<'static, NoopRawMutex, SampleBlock, SAMPLE_BLOCK_COUNT>,
    mut resources: SaiResources,
) {
    let mut write_buffer = [0u32; 2 * USB_MAX_SAMPLE_COUNT];
    info!("Write buffer: {} samples", write_buffer.len());

    let mut sai = new_sai(&mut write_buffer, &mut resources);
    let mut volume = (1.0, 1.0);

    loop {
        if let Some(new_volume) = VOLUME_SIGNAL.try_take() {
            volume = new_volume;
        }

        let samples = match select(receiver.receive(), sai.wait_write_error()).await {
            Either::First(samples) => Some(samples),
            Either::Second(_) => None,
        };

        let Some(mut samples) = samples else {
            BLINK_SIGNAL.signal(crate::Blink::Red);
            info!("Renew SAI");

            drop(sai);
            sai = new_sai(&mut write_buffer, &mut resources);

            continue;
        };

        for chunk in samples.chunks_exact_mut(2) {
            chunk[0] = sample_to_u32(sample_to_f32(chunk[0]) * volume.0);
            chunk[1] = sample_to_u32(sample_to_f32(chunk[1]) * volume.1);
        }

        if let Err(error) = sai.write(samples.as_slice()).await {
            error!("Unexpected write error: {}", error);
        };
    }
}
