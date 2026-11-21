//! Small helpers shared by modules that run child processes.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Reads at most `cap + 1` bytes (so callers can tell "over the cap"), then
/// drains the rest so the child never blocks on a full pipe.
pub(crate) async fn read_capped(r: impl AsyncRead + Unpin, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut r = r.take(cap + 1);
    r.read_to_end(&mut buf).await?;
    tokio::io::copy(&mut r.into_inner(), &mut tokio::io::sink()).await?;
    Ok(buf)
}
