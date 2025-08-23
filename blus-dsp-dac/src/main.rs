#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};

use blus_dsp_dac::audio_routing::SaiResources;
use blus_dsp_dac::{
    audio_routing, usb_audio, UsbSampleBlock, AUDIO_CHANNELS, FEEDBACK_COUNTER_TICK_RATE, FEEDBACK_REFRESH_PERIOD,
    FEEDBACK_SIGNAL, SAMPLE_RATE_HZ, USB_MAX_PACKET_SIZE, USB_MAX_SAMPLE_COUNT,
};
use defmt::{debug, info, unwrap};
use embassy_executor::Spawner;
use embassy_stm32::gpio::Output;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, interrupt, peripherals, timer, usb};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::zerocopy_channel;
use embassy_usb::class::uac1;
use embassy_usb::class::uac1::speaker::{self, Speaker};
use heapless::Vec;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USB_LP => usb::InterruptHandler<peripherals::USB>;
});

static TIMER: Mutex<CriticalSectionRawMutex, RefCell<Option<timer::low_level::Timer<peripherals::TIM2>>>> =
    Mutex::new(RefCell::new(None));
static DMA_BUFFER: StaticCell<[u32; 2 * USB_MAX_SAMPLE_COUNT]> = StaticCell::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hi.");

    let mut peripheral_config = embassy_stm32::Config::default();
    {
        use embassy_stm32::rcc::*;
        peripheral_config.rcc.hse = Some(Hse {
            freq: Hertz(16_000_000),
            mode: HseMode::Bypass,
        });
        peripheral_config.rcc.sys = Sysclk::PLL1_R;

        peripheral_config.rcc.ahb_pre = AHBPrescaler::DIV1;
        peripheral_config.rcc.apb1_pre = APBPrescaler::DIV1;
        peripheral_config.rcc.apb2_pre = APBPrescaler::DIV1;

        peripheral_config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: true });
        peripheral_config.rcc.mux.clk48sel = mux::Clk48sel::HSI48;

        peripheral_config.rcc.pll = Some(Pll {
            source: PllSource::HSI,
            prediv: PllPreDiv::DIV5,
            mul: PllMul::MUL92,
            divp: Some(PllPDiv::DIV2),
            divq: Some(PllQDiv::DIV4),
            divr: Some(PllRDiv::DIV2),
        });
    }
    let p = embassy_stm32::init(peripheral_config);

    let mut core_peri = cortex_m::Peripherals::take().unwrap();

    // Enable instruction cache.
    core_peri.SCB.enable_icache();

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

    // Initialize driver for high-speed external PHY.
    let usb_driver = usb::Driver::new(p.USB, Irqs, p.PA12, p.PA11);

    // Basic USB device configuration
    let mut config = embassy_usb::Config::new(0x1209, 0xaf04);
    config.manufacturer = Some("elagil");
    config.product = Some("blus-dsp-dac");

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
    let (stream, feedback, control_changed) = Speaker::new(
        &mut builder,
        state,
        USB_MAX_PACKET_SIZE as u16,
        uac1::SampleWidth::Width4Byte,
        &[SAMPLE_RATE_HZ],
        &AUDIO_CHANNELS,
        FEEDBACK_REFRESH_PERIOD,
    );

    // Build and run the USB device
    let usb_device = builder.build();

    let dma_buffer = DMA_BUFFER.init([0x00_u32; USB_MAX_SAMPLE_COUNT * 2]);
    let sai_resources = SaiResources {
        sai: p.SAI1,
        sck: p.PA8,
        sd: p.PA10,
        fs: p.PA9,
        dma: p.DMA1_CH1,
        dma_buf: dma_buffer,
    };

    // Establish a zero-copy channel for transferring received audio samples from the USB audio task.
    static USB_SAMPLE_BLOCKS: StaticCell<[UsbSampleBlock; 2]> = StaticCell::new();
    let usb_sample_blocks = USB_SAMPLE_BLOCKS.init([Vec::new(), Vec::new()]);

    static USB_CHANNEL: StaticCell<zerocopy_channel::Channel<'_, NoopRawMutex, UsbSampleBlock>> = StaticCell::new();
    let usb_channel = USB_CHANNEL.init(zerocopy_channel::Channel::new(usb_sample_blocks));
    let (usb_sender, usb_receiver) = usb_channel.split();

    // Trigger on USB SOF (internal signal)
    let mut tim2 = timer::low_level::Timer::new(p.TIM2);
    tim2.set_tick_freq(Hertz(FEEDBACK_COUNTER_TICK_RATE));
    // tim2.set_trigger_source(timer::low_level::TriggerSource::ITR11);

    const CHANNEL: timer::Channel = timer::Channel::Ch1;
    tim2.set_input_ti_selection(CHANNEL, timer::low_level::InputTISelection::TRC);
    tim2.set_input_capture_prescaler(CHANNEL, 0);
    tim2.set_input_capture_filter(CHANNEL, timer::low_level::FilterValue::NO_FILTER);

    // Reset all interrupt flags.
    tim2.regs_gp32().sr().write(|r| r.0 = 0);

    tim2.enable_channel(CHANNEL, true);
    tim2.enable_input_interrupt(CHANNEL, true);

    tim2.start();

    TIMER.lock(|p| p.borrow_mut().replace(tim2));

    unsafe {
        cortex_m::peripheral::NVIC::unmask(interrupt::TIM2);
    }

    // Launch USB audio tasks.
    unwrap!(spawner.spawn(usb_audio::control_task(control_changed)));
    unwrap!(spawner.spawn(usb_audio::streaming_task(stream, usb_sender)));
    unwrap!(spawner.spawn(usb_audio::feedback_task(feedback)));
    unwrap!(spawner.spawn(usb_audio::usb_task(usb_device)));

    // Launch audio routing.
    unwrap!(spawner.spawn(audio_routing::audio_routing_task(sai_resources, usb_receiver)));
}

#[interrupt]
fn TIM2() {
    static LAST_TICKS: Mutex<CriticalSectionRawMutex, Cell<u32>> = Mutex::new(Cell::new(0));
    static FRAME_COUNT: Mutex<CriticalSectionRawMutex, Cell<usize>> = Mutex::new(Cell::new(0));

    unsafe {
        let pin = embassy_stm32::peripherals::PB12::steal();
        let mut pin = Output::new(
            pin,
            embassy_stm32::gpio::Level::Low,
            embassy_stm32::gpio::Speed::VeryHigh,
        );
        pin.set_high();
    }

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
        timer.sr().write(|r| r.0 = 0);
    });
}
