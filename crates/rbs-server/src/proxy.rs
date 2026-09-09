//! stdio <-> unix socket bridge (`rbs proxy`, run over ssh by the client).

use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

use crate::error::ServerError;

/// Copy `input` → socket and socket → `output` until either direction hits EOF.
pub async fn bridge<I, O>(socket: &Path, mut input: I, mut output: O) -> Result<(), ServerError>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|source| ServerError::Connect {
            path: socket.to_path_buf(),
            source,
        })?;
    let (mut sock_rd, mut sock_wr) = stream.into_split();
    tokio::select! {
        r = tokio::io::copy(&mut input, &mut sock_wr) => {
            debug!(result = ?r, "proxy: input reached EOF");
            let _ = sock_wr.shutdown().await;
        }
        r = tokio::io::copy(&mut sock_rd, &mut output) => {
            debug!(result = ?r, "proxy: socket reached EOF");
        }
    }
    let _ = output.flush().await;
    Ok(())
}
