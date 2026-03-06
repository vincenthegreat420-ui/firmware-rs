# TAS2780 Driver

Driver for the Texas Instruments TAS2780 class-D audio amplifier.

## Features

- I2C communication with the TAS2780 amplifier
- Configurable TDM/I2S audio interface
- Multiple gain settings (11.0 dBV to 21.0 dBV)
- Channel selection (Left, Right, Stereo Mix)
- Power mode configuration
- Noise gate support

## Usage

```rust
use tas2780::tas2780::{Tas2780, Config, Gain, Channel, PowerMode};

let mut amp = Tas2780::new(&mut i2c, 0x39);

amp.init(Config {
    gain: Gain::Gain15_0dBV,
    channel: Channel::Left,
    tdm_slot: 0,
    power_mode: PowerMode::Two,
    ..Default::default()
}).await;

amp.enable();
amp.set_volume(0);  // 0 dB attenuation
```

## Configuration

### TDM Settings

- Word length: 16, 20, 24, or 32 bits
- Time slot length: 16, 24, or 32 bits
- Slot assignment: 0-15

### Power Modes

- **Mode 2**: Recommended for most applications

## Dependencies

- `embassy-time`: For timing delays during initialization
- `embedded-hal`: For I2C communication traits