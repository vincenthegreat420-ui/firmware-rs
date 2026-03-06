# Audio Library

Audio processing utilities for embedded firmware.

## Features

- **Audio Filter Chain**: Configurable biquad filter chains with gain and delay
- **Sample Conversion**: Convert between f32 and fixed-point audio samples
- **Audio Source Detection**: Enum for different audio input sources (USB, SPDIF, etc.)

## Usage

```rust
use audio::{AudioFilter, AudioSource, db_to_linear};
use audio::audio_filter::Filter;

// Create a simple low-pass filter
let filter = AudioFilter::new(1.0, 0, biquads);

// Process samples
let output = filter.run(input_sample);

// Convert dB to linear gain
let gain = db_to_linear(-6.0);  // Returns 0.5
```

## Dependencies

- `biquad`: For biquad filter implementations
- `micromath`: For fast floating-point math operations