use anyhow::Result;

use super::page;

pub async fn run(url: &str, timeout_ms: u64) -> Result<()> {
    let (browser, client) = page::open(url, timeout_ms).await?;
    page::teardown(browser, client).await
}
