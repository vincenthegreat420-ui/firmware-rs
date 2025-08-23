#![no_std]

use biquad::{self, ToHertz, Type};
use dsp_protocol::{Filter, Variant};

impl TryFrom<Filter> for _ {
    type Error = biquad::Errors;
    fn try_from(filter: Filter) -> Result<Self, Self::Error> {
        let filter_type = match filter.variant {
            Variant::AllPass => Type::AllPass,
            Variant::HighPass => Type::HighPass,
            Variant::LowPass => Type::LowPass,
            Variant::HighShelf => Type::HighShelf(filter.level_db),
            Variant::LowShelf => Type::LowShelf(filter.level_db),
            Variant::Peak => Type::PeakingEQ(filter.level_db),
        };

        B::new(biquad::Coefficients<f32>::from_params(filter_type, filter.fs_hz.hz(), filter.f0_hz.hz(), filter.q_value)).map(|b| Biquad(b))
    }
}
