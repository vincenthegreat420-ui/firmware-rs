#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};

use defmt::{debug, error, info, panic, unwrap};
use embassy_executor::Spawner;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, interrupt, peripherals, timer, usb, Config};
use embassy_stm32::sai::{self, word, BitOrder, ClockStrobe, Sai};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_sync::zerocopy_channel;
use embassy_usb::class::uac1;
use embassy_usb::class::uac1::speaker::{self, Speaker};
use embassy_usb::driver::EndpointError;
use heapless::Vec;
use micromath::F32Ext;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};
use embassy_stm32::Peri;
use crate::sai::FrameSyncOffset;
use crate::sai::FrameSyncPolarity;
use embassy_futures::select::{select, Either};
use defmt::warn;

bind_interrupts!(struct Irqs {
    USB_DRD_FS => usb::InterruptHandler<peripherals::USB>;
    GPDMA1_CHANNEL1 => embassy_stm32::dma::InterruptHandler<peripherals::GPDMA1_CH1>;
});

static TIMER: Mutex<CriticalSectionRawMutex, RefCell<Option<timer::low_level::Timer<peripherals::TIM5>>>> =
Mutex::new(RefCell::new(None));

// A counter signal that is written by the feedback timer, once every `FEEDBACK_REFRESH_PERIOD`.
// At that point, a feedback value is sent to the host.
pub static FEEDBACK_SIGNAL: Signal<CriticalSectionRawMutex, u32> = Signal::new();

// Stereo input
pub const INPUT_CHANNEL_COUNT: usize = 2;

// This example uses a fixed sample rate of 48 kHz.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const FEEDBACK_COUNTER_TICK_RATE: u32 = 12_288_000;

// Use 32 bit samples, which allow for a lot of (software) volume adjustment without degradation of quality.
pub const SAMPLE_WIDTH: uac1::SampleWidth = uac1::SampleWidth::Width4Byte;
pub const SAMPLE_WIDTH_BIT: usize = SAMPLE_WIDTH.in_bit();
pub const SAMPLE_SIZE: usize = SAMPLE_WIDTH as usize;
pub const SAMPLE_SIZE_PER_S: usize = (SAMPLE_RATE_HZ as usize) * INPUT_CHANNEL_COUNT * SAMPLE_SIZE;

// Size of audio samples per 1 ms - for the full-speed USB frame period of 1 ms.
pub const USB_FRAME_SIZE: usize = SAMPLE_SIZE_PER_S.div_ceil(1000);

// Select front left and right audio channels.
pub const AUDIO_CHANNELS: [uac1::Channel; INPUT_CHANNEL_COUNT] = [uac1::Channel::LeftFront, uac1::Channel::RightFront];

// Factor of two as a margin for feedback (this is an excessive amount)
pub const USB_MAX_PACKET_SIZE: usize = 2 * USB_FRAME_SIZE;
pub const USB_MAX_SAMPLE_COUNT: usize = USB_MAX_PACKET_SIZE / SAMPLE_SIZE;

// The data type that is exchanged via the zero-copy channel (a sample vector).
pub type SampleBlock = Vec<u32, USB_MAX_SAMPLE_COUNT>;

// Feedback is provided in 10.14 format for full-speed endpoints.
pub const FEEDBACK_REFRESH_PERIOD: uac1::FeedbackRefresh = uac1::FeedbackRefresh::Period8Frames;
const FEEDBACK_SHIFT: usize = 14;

const TICKS_PER_SAMPLE: f32 = (FEEDBACK_COUNTER_TICK_RATE as f32) / (SAMPLE_RATE_HZ as f32);

/// Resources that are required for instantiating SAI1.
#[allow(missing_docs)]
pub struct SaiResources {
    pub sai: Peri<'static, peripherals::SAI1>,
    pub sck_a: Peri<'static, peripherals::PE5>,
    pub sd_a: Peri<'static, peripherals::PE6>,
    pub fs_a: Peri<'static, peripherals::PE4>,
    pub mclk_a: Peri<'static, peripherals::PE2>,
    pub dma_a: Peri<'static, peripherals::GPDMA1_CH1>,
}

fn new_sai<'d>(write_buffer: &'d mut [u32], resources: &'d mut SaiResources) -> Sai<'d, peripherals::SAI1, u32> {
    let (sai_a, _) = sai::split_subblocks(resources.sai.reborrow());

    let mut config = sai::Config::default();
    config.bit_order = BitOrder::MsbFirst;
    config.slot_count = word::U4(INPUT_CHANNEL_COUNT as u8);
    config.frame_sync_active_level_length = word::U7(SAMPLE_WIDTH_BIT as u8);
    config.data_size = sai::DataSize::Data32;
    config.frame_length = (INPUT_CHANNEL_COUNT * SAMPLE_WIDTH_BIT) as u16;
    config.master_clock_divider = sai::MasterClockDivider::DIV1;
    config.clock_strobe = ClockStrobe::Falling;
    config.frame_sync_offset = FrameSyncOffset::BeforeFirstBit;
    config.frame_sync_polarity = FrameSyncPolarity::ActiveLow;
    sai::Sai::new_asynchronous_with_mclk(
        sai_a,
        resources.sck_a.reborrow(),
                                         resources.sd_a.reborrow(),
                                         resources.fs_a.reborrow(),
                                         resources.mclk_a.reborrow(),   // Важно: MCLK подключён
                                         resources.dma_a.reborrow(),
                                         write_buffer,
                                         Irqs,
                                         config,
    )
}

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
    feedback_factor: f32,
) -> Result<(), Disconnected> {
    let mut packet: Vec<u8, 4> = Vec::new();

    // Collects the fractional component of the feedback value that is lost by rounding.
    let mut rest = 0.0_f32;

    loop {
        let counter = FEEDBACK_SIGNAL.wait().await;

        packet.clear();

        let raw_value = counter as f32 * feedback_factor + rest;
        let value = raw_value.round();
        rest = raw_value - value;

        let value = value as u32;

        debug!("Feedback value: {}", value);

        packet.push(value as u8).unwrap();
        packet.push((value >> 8) as u8).unwrap();
        packet.push((value >> 16) as u8).unwrap();

        feedback.write_packet(&packet).await?;
    }
}

/// Handles streaming of audio data from the host.
async fn stream_handler<'d, T: usb::Instance + 'd>(
    stream: &mut speaker::Stream<'d, usb::Driver<'d, T>>,
    sender: &mut zerocopy_channel::Sender<'static, NoopRawMutex, SampleBlock>,
) -> Result<(), Disconnected> {
    loop {
        let mut usb_data = [0u8; USB_MAX_PACKET_SIZE];
        let data_size = stream.read_packet(&mut usb_data).await?;

        let word_count = data_size / SAMPLE_SIZE;

        if word_count * SAMPLE_SIZE == data_size {
            // Obtain a buffer from the channel
            let samples = sender.send().await;
            samples.clear();

            for w in 0..word_count {
                let byte_offset = w * SAMPLE_SIZE;
                let sample = u32::from_le_bytes(usb_data[byte_offset..byte_offset + SAMPLE_SIZE].try_into().unwrap());

                // Fill the sample buffer with data.
                samples.push(sample).unwrap();
            }

            sender.send_done();
        } else {
            debug!("Invalid USB buffer size of {}, skipped.", data_size);
        }
    }
}

#[embassy_executor::task]
async fn audio_receiver_task(
    mut usb_audio_receiver: zerocopy_channel::Receiver<'static, NoopRawMutex, SampleBlock>,
    mut resources: SaiResources,
) {
    // Буфер для DMA SAI
    let mut write_buffer = [0u32; 2 * USB_MAX_SAMPLE_COUNT];
    info!("Audio task started. Write buffer: {} samples", write_buffer.len());

    let mut sai = new_sai(&mut write_buffer, &mut resources);

    loop {
        // Ждем либо данные из USB, либо сигнал об аппаратной ошибке SAI
        match select(usb_audio_receiver.receive(), sai.wait_write_error()).await {
            // КЕЙС 1: Получены новые данные из USB
            Either::First(samples) => {
                let data = samples.as_slice();

                // 1. Проверка на четность (защита от смены каналов L/R)
                if !data.is_empty() && (data.len() % 2 == 0) {
                    // 2. Попытка записи в SAI
                    if let Err(error) = sai.write(data).await {
                        error!("SAI write error: {}. Resetting SAI...", error);

                        // Сброс периферии для восстановления синхронизации
                        drop(sai);
                        sai = new_sai(&mut write_buffer, &mut resources);
                    }
                } else if !data.is_empty() {
                    warn!("Dropped packet with odd sample count: {} (Alignment safety)", data.len());
                }

                // Обязательно подтверждаем обработку, чтобы канал не заблокировался
                usb_audio_receiver.receive_done();
            }

            // КЕЙС 2: Аппаратная ошибка SAI (например, FIFO underrun/overrun)
            Either::Second(err) => {
                error!("SAI hardware error detected: {}. Performing reset...", err);

                // Полная переинициализация
                drop(sai);
                sai = new_sai(&mut write_buffer, &mut resources);

                // Опционально: если ошибка критическая, можно очистить текущий пакет в USB канале,
                // чтобы не пытаться воспроизвести "битый" кусок данных после сброса.
                // Это предотвращает циклическую ошибку.
            }
        }
    }
}

/// Receives audio samples from the host.
#[embassy_executor::task]
async fn usb_streaming_task(
    mut stream: speaker::Stream<'static, usb::Driver<'static, peripherals::USB>>,
    mut sender: zerocopy_channel::Sender<'static, NoopRawMutex, SampleBlock>,
) {
    loop {
        stream.wait_connection().await;
        info!("USB connected.");
        _ = stream_handler(&mut stream, &mut sender).await;
        info!("USB disconnected.");
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
async fn usb_feedback_task(mut feedback: speaker::Feedback<'static, usb::Driver<'static, peripherals::USB>>) {
    let feedback_factor =
    ((1 << FEEDBACK_SHIFT) as f32 / TICKS_PER_SAMPLE) / FEEDBACK_REFRESH_PERIOD.frame_count() as f32;

    loop {
        feedback.wait_connection().await;
        _ = feedback_handler(&mut feedback, feedback_factor).await;
    }
}

#[embassy_executor::task]
async fn usb_task(mut usb_device: embassy_usb::UsbDevice<'static, usb::Driver<'static, peripherals::USB>>) {
    usb_device.run().await;
}

/// Checks for changes on the control monitor of the class.
///
/// In this case, monitor changes of volume or mute state.
#[embassy_executor::task]
async fn usb_control_task(control_monitor: speaker::ControlMonitor<'static>) {
    loop {
        control_monitor.changed().await;

        for channel in AUDIO_CHANNELS {
            let volume = control_monitor.volume(channel).unwrap();
            info!("Volume changed to {} on channel {}.", volume, channel);
        }
    }
}

/// Feedback value measurement and calculation
///
/// Used for measuring/calculating the number of samples that were received from the host during the
/// `FEEDBACK_REFRESH_PERIOD`.
///
/// Configured in this example with
/// - a refresh period of 8 ms, and
/// - a tick rate of 42 MHz.
///
/// This gives an (ideal) counter value of 336.000 for every update of the `FEEDBACK_SIGNAL`.
#[interrupt]
fn TIM5() {
    static LAST_TICKS: Mutex<CriticalSectionRawMutex, Cell<u32>> = Mutex::new(Cell::new(0));
    static FRAME_COUNT: Mutex<CriticalSectionRawMutex, Cell<usize>> = Mutex::new(Cell::new(0));

    critical_section::with(|cs| {
        // Read timer counter.
        let timer = TIMER.borrow(cs).borrow().as_ref().unwrap().regs_gp32();

        let status = timer.sr().read();

        const CHANNEL_INDEX: usize = 0;
        if status.ccif(CHANNEL_INDEX) {
            let ticks = timer.ccr(CHANNEL_INDEX).read();

            let frame_count = FRAME_COUNT.borrow(cs);
            let last_ticks = LAST_TICKS.borrow(cs);

            frame_count.set(frame_count.get() + 1);
            if frame_count.get() >= FEEDBACK_REFRESH_PERIOD.frame_count() {
                frame_count.set(0);
                FEEDBACK_SIGNAL.signal(ticks.wrapping_sub(last_ticks.get()));
                last_ticks.set(ticks);
            }
        };

        // Clear trigger interrupt flag.
        timer.sr().modify(|r| r.set_tif(false));
    });
}

// If you are trying this and your USB device doesn't connect, the most
// common issues are the RCC config and vbus_detection
//
// See https://embassy.dev/book/#_the_usb_examples_are_not_working_on_my_board_is_there_anything_else_i_need_to_configure
// for more information.
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hsi = None;
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: true }); // needed for USB
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
                              mode: HseMode::Oscillator,
        });
        config.rcc.pll1 = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL125,
            divp: Some(PllDiv::DIV2), // 250 Mhz
                               divq: None,
                               divr: None,
        });
        config.rcc.pll2 = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV5,
            mul: PllMul::MUL192,
            divp: Some(PllDiv::DIV25), // 12.288 MHz for 48 kHz audio
                               divq: None,
                               divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV1;
        config.rcc.apb2_pre = APBPrescaler::DIV1;
        config.rcc.apb3_pre = APBPrescaler::DIV1;
        config.rcc.sys = Sysclk::PLL1_P;
        config.rcc.voltage_scale = VoltageScale::Scale0;
        config.rcc.mux.usbsel = mux::Usbsel::HSI48;
        config.rcc.mux.sai1sel = mux::Saisel::PLL2_P;
    }
    let p = embassy_stm32::init(config);
    let mut core_peri = cortex_m::Peripherals::take().unwrap();
    core_peri.SCB.enable_icache();
    info!("Hello World!");

    // Configure all required buffers in a static way.
    debug!("USB packet size is {} byte", USB_MAX_PACKET_SIZE);
    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    let config_descriptor = CONFIG_DESCRIPTOR.init([0; 256]);

    static BOS_DESCRIPTOR: StaticCell<[u8; 32]> = StaticCell::new();
    let bos_descriptor = BOS_DESCRIPTOR.init([0; 32]);

    const CONTROL_BUF_SIZE: usize = 64;
    static CONTROL_BUF: StaticCell<[u8; CONTROL_BUF_SIZE]> = StaticCell::new();
    let control_buf = CONTROL_BUF.init([0; CONTROL_BUF_SIZE]);

    static STATE: StaticCell<speaker::State> = StaticCell::new();
    let state = STATE.init(speaker::State::new());

    let usb_driver = usb::Driver::new(p.USB, Irqs, p.PA12, p.PA11);

    // Basic USB device configuration
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("USB-audio-speaker example");
    config.serial_number = Some("12345678");

    let mut builder = embassy_usb::Builder::new(
        usb_driver,
        config,
        config_descriptor,
        bos_descriptor,
        &mut [], // no msos descriptors
        control_buf,
    );

    // Create the UAC1 Speaker class components
    let (stream, feedback, control_monitor) = Speaker::new(
        &mut builder,
        state,
        USB_MAX_PACKET_SIZE as u16,
        uac1::SampleWidth::Width4Byte,
        &[SAMPLE_RATE_HZ],
        &AUDIO_CHANNELS,
        FEEDBACK_REFRESH_PERIOD,
    );

    // Create the USB device
    let usb_device = builder.build();

    // Establish a zero-copy channel for transferring received audio samples between tasks
    static SAMPLE_BLOCKS: StaticCell<[SampleBlock; 2]> = StaticCell::new();
    let sample_blocks = SAMPLE_BLOCKS.init([Vec::new(), Vec::new()]);

    static CHANNEL: StaticCell<zerocopy_channel::Channel<'_, NoopRawMutex, SampleBlock>> = StaticCell::new();
    let channel = CHANNEL.init(zerocopy_channel::Channel::new(sample_blocks));
    let (sender, receiver) = channel.split();

    // Initialize SAI resources
    let sai_resources = SaiResources {
        sai: p.SAI1,
        sck_a: p.PE5,
        sd_a: p.PE6,
        fs_a: p.PE4,
        mclk_a: p.PE2,
        dma_a: p.GPDMA1_CH1,
    };

    // Run a timer for counting between SOF interrupts.
    let mut tim5 = timer::low_level::Timer::new(p.TIM5);
    tim5.set_tick_freq(Hertz(FEEDBACK_COUNTER_TICK_RATE));
    tim5.set_trigger_source(timer::low_level::TriggerSource::ITR12);

    const TIMER_CHANNEL: timer::Channel = timer::Channel::Ch1;
    tim5.set_input_ti_selection(TIMER_CHANNEL, timer::low_level::InputTISelection::TRC);
    tim5.set_input_capture_prescaler(TIMER_CHANNEL, 0);
    tim5.set_input_capture_filter(TIMER_CHANNEL, timer::low_level::FilterValue::FCK_INT_N2);

    // Reset all interrupt flags.
    tim5.regs_gp32().sr().write(|r| r.0 = 0);

    tim5.enable_channel(TIMER_CHANNEL, true);
    tim5.enable_input_interrupt(TIMER_CHANNEL, true);

    tim5.start();

    TIMER.lock(|p| p.borrow_mut().replace(tim5));

    // Unmask the TIM5 interrupt.
    unsafe {
        cortex_m::peripheral::NVIC::unmask(interrupt::TIM5);
    }

    // Launch USB audio tasks.
    spawner.spawn(unwrap!(usb_control_task(control_monitor)));
    spawner.spawn(unwrap!(usb_streaming_task(stream, sender)));
    spawner.spawn(unwrap!(usb_feedback_task(feedback)));
    spawner.spawn(unwrap!(usb_task(usb_device)));
    spawner.spawn(unwrap!(audio_receiver_task(receiver, sai_resources)));
}
