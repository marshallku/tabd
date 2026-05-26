use anyhow::Result;
use serde_json::Value;

pub struct CdpClient {}

impl CdpClient {
    pub async fn connect(_ws_url: &str) -> Result<Self> {
        anyhow::bail!("cdp::CdpClient::connect — not implemented (task #16)")
    }

    pub async fn send(&self, _method: &str, _params: Value) -> Result<Value> {
        anyhow::bail!("cdp::CdpClient::send — not implemented (task #16)")
    }

    pub async fn close(self) -> Result<()> {
        anyhow::bail!("cdp::CdpClient::close — not implemented (task #16)")
    }
}
