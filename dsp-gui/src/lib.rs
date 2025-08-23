use dsp_protocol::{Channel, GetChannelEndpoint, Info, InfoEndpoint, SetChannelEndpoint};
use postcard_rpc::{
    header::VarSeqKind,
    host_client::{HostClient, HostErr},
    standard_icd::{PingEndpoint, WireError, ERROR_PATH},
};
use std::convert::Infallible;

pub mod widgets;

pub struct Client {
    pub client: HostClient<WireError>,
}

#[derive(Debug)]
pub enum Error<E> {
    Comms(HostErr<WireError>),
    Endpoint(E),
}

impl<E> From<HostErr<WireError>> for Error<E> {
    fn from(value: HostErr<WireError>) -> Self {
        Self::Comms(value)
    }
}

impl Client {
    pub fn try_new() -> Result<Self, String> {
        let client = HostClient::try_new_raw_nusb(
            |d| d.product_string() == Some("usb-dsp"),
            ERROR_PATH,
            8,
            VarSeqKind::Seq2,
        )?;

        Ok(Self { client })
    }

    pub async fn wait_closed(&self) {
        self.client.wait_closed().await;
    }

    pub async fn ping(&self, id: u32) -> Result<u32, Error<Infallible>> {
        let val = self.client.send_resp::<PingEndpoint>(&id).await?;
        Ok(val)
    }

    pub async fn info(&self) -> Result<Info, Error<Infallible>> {
        let val = self.client.send_resp::<InfoEndpoint>(&()).await?;
        Ok(val)
    }

    pub async fn set_channel(&self, channel: Channel) -> Result<(), Error<Infallible>> {
        self.client.send_resp::<SetChannelEndpoint>(&channel).await?;
        Ok(())
    }

    pub async fn get_channel(&self, id: u32) -> Result<Channel, Error<Infallible>> {
        let val = self.client.send_resp::<GetChannelEndpoint>(&id).await?;
        Ok(val)
    }
}
