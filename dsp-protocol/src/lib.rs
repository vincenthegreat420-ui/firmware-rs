#![cfg_attr(not(feature = "use-std"), no_std)]

use postcard_rpc::{endpoints, topics, TopicDirection};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Schema, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Variant {
    Peak,
    LowPass,
    HighPass,
    LowShelf,
    HighShelf,
    AllPass,
}

#[cfg(feature = "use-std")]
impl From<Variant> for &str {
    fn from(value: Variant) -> Self {
        match value {
            Variant::Peak => "Peak",
            Variant::LowPass => "Low pass",
            Variant::HighPass => "High pass",
            Variant::LowShelf => "Low shelf",
            Variant::HighShelf => "High shelf",
            Variant::AllPass => "All pass",
        }
    }
}

#[derive(Debug, Clone, Copy, Schema, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Filter {
    pub id: u32,
    pub variant: Variant,
    pub level_db: f32,
    pub f0_hz: f32,
    pub fs_hz: f32,
    pub q_value: f32,
    pub is_muted: bool,
}

impl Filter {
    /// Create a new filter with a given ID.
    pub fn new(id: u32) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            id: 0,
            variant: Variant::Peak,
            level_db: 0.0,
            fs_hz: 48000.0,
            f0_hz: 1000.0,
            q_value: 0.71,
            is_muted: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Schema, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Channel {
    pub id: u32,
    pub input_id: u32,
    pub filters: [Filter; 10],
}

#[derive(Debug, Clone, Copy, Schema, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Info {
    pub channel_count: u32,
    pub input_count: u32,
    pub sample_rate_hz: u32,
}

endpoints! {
    list = ENDPOINT_LIST;
    omit_std = true;
    | EndpointTy                | RequestTy     | ResponseTy            | Path              |
    | ----------                | ---------     | ----------            | ----              |
    | PingEndpoint              | u32           | u32                   | "ping"            |
    | InfoEndpoint              | ()            | Info                  | "info"            |
    | GetChannelEndpoint        | u32           | Channel               | "channel/get"     |
    | SetChannelEndpoint        | Channel       | ()                    | "channel/set"     |
}

topics! {
    list = TOPICS_IN_LIST;
    direction = TopicDirection::ToServer;
    | TopicTy                   | MessageTy     | Path              |
    | -------                   | ---------     | ----              |
}

topics! {
    list = TOPICS_OUT_LIST;
    direction = TopicDirection::ToClient;
    | TopicTy                   | MessageTy     | Path              | Cfg                           |
    | -------                   | ---------     | ----              | ---                           |
}
