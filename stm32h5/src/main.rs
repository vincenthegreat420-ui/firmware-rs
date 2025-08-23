#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};

use defmt::{panic, *};
use embassy_executor::Spawner;
use embassy_stm32::pac::gpio::vals::{Moder, Ospeedr, Ot, Pupdr};
use embassy_stm32::pac::GPIOA;
use embassy_stm32::sai::{word, BitOrder, ClockStrobe, Sai};

use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, interrupt, peripherals, sai, timer, usb, Config};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex, ThreadModeRawMutex};
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

bind_interrupts!(struct Irqs {
    USB_DRD_FS => usb::InterruptHandler<peripherals::USB>;
});

pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn clip_q63_to_q31(sample: i64) -> i32 {
    if (sample >> 32) as i32 != (sample as i32) >> 31 {
        0x7FFFFFFF ^ ((sample >> 63) as i32)
    } else {
        sample as i32
    }
}

const Q31_SCALING_FACTOR: f32 = 2147483648.0;

/// Convert a float sample to its 1q31 representation. Clips to [-1, 1).
pub fn sample_to_u32(sample: f32) -> u32 {
    clip_q63_to_q31((sample * Q31_SCALING_FACTOR) as i64) as u32
}

/// Convert a 1q31 sample to float in the range [-1, 1)
pub fn sample_to_f32(sample: u32) -> f32 {
    (sample as i32 as f32) / Q31_SCALING_FACTOR
}

static TIMER: Mutex<CriticalSectionRawMutex, RefCell<Option<timer::low_level::Timer<peripherals::TIM2>>>> =
    Mutex::new(RefCell::new(None));

// A counter signal that is written by the feedback timer, once every `FEEDBACK_REFRESH_PERIOD`.
// At that point, a feedback value is sent to the host.
pub static FEEDBACK_SIGNAL: Signal<CriticalSectionRawMutex, u32> = Signal::new();

/// Signals volume changes.
pub static VOLUME_SIGNAL: Signal<ThreadModeRawMutex, (f32, f32)> = Signal::new();

// Stereo
pub const CHANNEL_COUNT: usize = 2;

// This example uses a fixed sample rate of 48 kHz.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const FEEDBACK_COUNTER_TICK_RATE: u32 = 62_500_000;

// Use 32 bit samples, which allow for a lot of (software) volume adjustment without degradation of quality.
pub const SAMPLE_WIDTH: uac1::SampleWidth = uac1::SampleWidth::Width4Byte;
pub const SAMPLE_WIDTH_BIT: usize = SAMPLE_WIDTH.in_bit();
pub const SAMPLE_SIZE: usize = SAMPLE_WIDTH as usize;
pub const SAMPLE_SIZE_PER_S: usize = (SAMPLE_RATE_HZ as usize) * CHANNEL_COUNT * SAMPLE_SIZE;

// Size of audio samples per 1 ms - for the full-speed USB frame period of 1 ms.
pub const USB_FRAME_SIZE: usize = SAMPLE_SIZE_PER_S.div_ceil(1000);

// Select front left and right audio channels.
pub const AUDIO_CHANNELS: [uac1::Channel; CHANNEL_COUNT] = [uac1::Channel::LeftFront, uac1::Channel::RightFront];

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
    pub sai: peripherals::SAI1,
    pub sck_a: peripherals::PF8,
    pub sd_a: peripherals::PE3,
    pub fs_a: peripherals::PF9,
    pub dma_a: peripherals::GPDMA1_CH1,
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

fn new_sai<'d>(write_buffer: &'d mut [u32], resources: &'d mut SaiResources) -> Sai<'d, peripherals::SAI1, u32> {
    let (_, sai) = sai::split_subblocks(&mut resources.sai);

    // I2S compatible.
    let mut config = sai::Config::default();
    config.bit_order = BitOrder::MsbFirst;
    config.slot_count = sai::word::U4(CHANNEL_COUNT as u8);
    config.frame_sync_active_level_length = word::U7(SAMPLE_WIDTH_BIT as u8);
    config.data_size = sai::DataSize::Data32;
    config.frame_length = (CHANNEL_COUNT * SAMPLE_WIDTH_BIT) as u8;
    config.master_clock_divider = sai::MasterClockDivider::Div1;
    config.clock_strobe = ClockStrobe::Falling;

    sai::Sai::new_asynchronous(
        sai,
        &mut resources.sck_a,
        &mut resources.sd_a,
        &mut resources.fs_a,
        &mut resources.dma_a,
        write_buffer,
        config,
    )
}

/// Receives audio samples from the USB streaming task and can play them back.
#[embassy_executor::task]
async fn audio_receiver_task(
    mut usb_audio_receiver: zerocopy_channel::Receiver<'static, NoopRawMutex, SampleBlock>,
    mut resources: SaiResources,
) {
    let mut write_buffer = [0u32; 2 * USB_MAX_SAMPLE_COUNT];
    let mut sai = new_sai(&mut write_buffer, &mut resources);
    let mut volume = (1.0, 1.0);

    loop {
        if let Some(new_volume) = VOLUME_SIGNAL.try_take() {
            volume = new_volume;
        }

        let samples = usb_audio_receiver.receive().await;
        for chunk in samples.chunks_exact_mut(2) {
            chunk[0] = sample_to_u32(sample_to_f32(chunk[0]) * volume.0);
            chunk[1] = sample_to_u32(sample_to_f32(chunk[1]) * volume.1);
        }

        // Use the samples, for example play back via the SAI peripheral.
        if let Err(error) = sai.write(samples).await {
            info!("Renew SAI: {}", error);

            drop(sai);
            sai = new_sai(&mut write_buffer, &mut resources);
        }

        // Notify the channel that the buffer is now ready to be reused
        usb_audio_receiver.receive_done();
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

/// The USB control task.
///
/// Provides
/// - Volume adjustment
/// - Sample rate adjustment (not used, is fixed)
/// - Sample width adjustment (not used, is fixed)
#[embassy_executor::task]
pub async fn usb_control_task(control_monitor: speaker::ControlMonitor<'static>) {
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

/// Feedback value measurement and calculation
///
/// Used for measuring/calculating the number of samples that were received from the host during the
/// `FEEDBACK_REFRESH_PERIOD`.
#[interrupt]
fn TIM2() {
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
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: true });
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::BypassDigital,
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
        config.rcc.pll3 = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL120,
            divp: None,
            divq: Some(PllDiv::DIV10), // 48 MHz for USB
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV1;
        config.rcc.apb2_pre = APBPrescaler::DIV1;
        config.rcc.apb3_pre = APBPrescaler::DIV1;
        config.rcc.sys = Sysclk::PLL1_P;
        config.rcc.voltage_scale = VoltageScale::Scale0;
        config.rcc.mux.usbsel = mux::Usbsel::PLL3_Q;
        config.rcc.mux.sai1sel = mux::Saisel::PLL2_P;
        config.rcc.mux.sai2sel = mux::Saisel::PLL2_P;
    }
    let p = embassy_stm32::init(config);

    info!("Hi");

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

    let usb_driver = usb::Driver::new_with_sof(p.USB, Irqs, p.PA12, p.PA11, p.PA8);

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

    // Run a timer for measuring time between SOF edges.
    // SOF is output by the USB peripheral and used for input capture on TIM2 CH1.
    let sof_input_af = 1;
    let sof_input_pin = 5;

    GPIOA
        .afr(sof_input_pin / 8)
        .modify(|w| w.set_afr(sof_input_pin % 8, sof_input_af));
    GPIOA.pupdr().modify(|w| w.set_pupdr(sof_input_pin, Pupdr::FLOATING));
    GPIOA.otyper().modify(|w| w.set_ot(sof_input_pin, Ot::PUSH_PULL));
    GPIOA
        .ospeedr()
        .modify(|w| w.set_ospeedr(sof_input_pin, Ospeedr::VERY_HIGH_SPEED));
    GPIOA.moder().modify(|w| w.set_moder(sof_input_pin, Moder::ALTERNATE));

    // Set up TIM2 for input capture.
    let mut tim2 = timer::low_level::Timer::new(p.TIM2);
    tim2.set_tick_freq(Hertz(FEEDBACK_COUNTER_TICK_RATE));
    tim2.set_trigger_source(timer::low_level::TriggerSource::ETRF);

    const TIMER_CHANNEL: timer::Channel = timer::Channel::Ch1;
    tim2.set_input_ti_selection(TIMER_CHANNEL, timer::low_level::InputTISelection::Normal);
    tim2.set_input_capture_prescaler(TIMER_CHANNEL, 0);
    tim2.set_input_capture_filter(TIMER_CHANNEL, timer::low_level::FilterValue::FCK_INT_N2);

    // Reset all interrupt flags.
    tim2.regs_gp32().sr().write(|r| r.0 = 0);

    tim2.enable_channel(TIMER_CHANNEL, true);
    tim2.enable_input_interrupt(TIMER_CHANNEL, true);

    tim2.start();

    TIMER.lock(|p| p.borrow_mut().replace(tim2));

    // Unmask the TIM2 interrupt.
    unsafe {
        cortex_m::peripheral::NVIC::unmask(interrupt::TIM2);
    }

    let sai_resources = SaiResources {
        sai: p.SAI1,
        sck_a: p.PF8,
        sd_a: p.PE3,
        fs_a: p.PF9,
        dma_a: p.GPDMA1_CH1,
    };

    // Launch USB audio tasks.
    unwrap!(spawner.spawn(usb_control_task(control_monitor)));
    unwrap!(spawner.spawn(usb_streaming_task(stream, sender)));
    unwrap!(spawner.spawn(usb_feedback_task(feedback)));
    unwrap!(spawner.spawn(usb_task(usb_device)));
    unwrap!(spawner.spawn(audio_receiver_task(receiver, sai_resources)));
}
