#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};

use defmt::{debug, info, unwrap};
use embassy_executor::Spawner;
use embassy_stm32::gpio::Output;
use embassy_stm32::pac::gpio::vals::{Moder, Ospeedr, Ot, Pupdr};
use embassy_stm32::pac::GPIOA;

use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, interrupt, peripherals, timer, usb, Config};
use embassy_stm32h5_examples::audio_routing::SaiResources;
use embassy_stm32h5_examples::{
    audio_routing, usb_audio, Blink, SampleBlock, AUDIO_CHANNELS, BLINK_SIGNAL, FEEDBACK_COUNTER_TICK_RATE,
    FEEDBACK_REFRESH_PERIOD, FEEDBACK_SIGNAL, SAMPLE_BLOCK_COUNT, SAMPLE_RATE_HZ, USB_MAX_PACKET_SIZE,
};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel;
use embassy_time::Timer;
use embassy_usb::class::uac1;
use embassy_usb::class::uac1::speaker::{self, Speaker};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USB_DRD_FS => usb::InterruptHandler<peripherals::USB>;
});

static TIMER: Mutex<CriticalSectionRawMutex, RefCell<Option<timer::low_level::Timer<peripherals::TIM2>>>> =
    Mutex::new(RefCell::new(None));

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

#[embassy_executor::task]
async fn blinky_task(mut led_red: Output<'static>, mut led_yellow: Output<'static>, mut led_green: Output<'static>) {
    // Say hi with LEDs.
    for led in [&mut led_red, &mut led_yellow, &mut led_green] {
        led.set_high();
        Timer::after_millis(100).await;
        led.set_low();
    }

    loop {
        let led = match BLINK_SIGNAL.wait().await {
            Blink::Red => &mut led_red,
            Blink::Yellow => &mut led_yellow,
            Blink::Green => &mut led_green,
        };

        led.set_high();
        Timer::after_secs(1).await;
        led.set_low();
    }
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

    // Enable instruction cache.
    core_peri.SCB.enable_icache();

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

    // Required for windows compatibility.
    // https://developer.nordicsemi.com/nRF_Connect_SDK/doc/1.9.1/kconfig/CONFIG_CDC_ACM_IAD.html#help
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

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

    // Establish a channel for transferring received audio samples.
    static AUDIO_CHANNEL: StaticCell<channel::Channel<NoopRawMutex, SampleBlock, SAMPLE_BLOCK_COUNT>> =
        StaticCell::new();
    let audio_channel = AUDIO_CHANNEL.init(channel::Channel::new());

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
        sck_b: p.PB3,
        sd_b: p.PB5,
        fs_b: p.PB4,
        dma_b: p.GPDMA1_CH1,
    };

    unwrap!(spawner.spawn(blinky_task(
        Output::new(p.PG4, embassy_stm32::gpio::Level::Low, embassy_stm32::gpio::Speed::Low),
        Output::new(p.PF4, embassy_stm32::gpio::Level::Low, embassy_stm32::gpio::Speed::Low),
        Output::new(p.PB0, embassy_stm32::gpio::Level::Low, embassy_stm32::gpio::Speed::Low)
    )));

    // Launch USB audio tasks.
    unwrap!(spawner.spawn(usb_audio::control_task(control_monitor)));
    unwrap!(spawner.spawn(usb_audio::streaming_task(stream, audio_channel.sender())));
    unwrap!(spawner.spawn(usb_audio::feedback_task(feedback)));
    unwrap!(spawner.spawn(usb_audio::device_task(usb_device)));
    unwrap!(spawner.spawn(audio_routing::audio_receiver_task(
        audio_channel.receiver(),
        sai_resources
    )));
}
