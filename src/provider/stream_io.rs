//! Shared transport helpers for non-SSE chunked streams.

use anyhow::{Result, bail};
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

/// Read the next byte chunk or stop immediately if cancellation wins.
///
/// Providers must race inbound stream I/O against `cancel` instead of polling
/// `is_cancelled()` after an awaited read.
pub async fn next_chunk_or_cancel<S, T, E>(
    stream: &mut S,
    cancel: &CancellationToken,
) -> Result<Option<T>>
where
    S: Stream<Item = std::result::Result<T, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        _ = cancel.cancelled() => bail!("Aborted"),
        maybe_chunk = stream.next() => {
            match maybe_chunk {
                Some(Ok(chunk)) => Ok(Some(chunk)),
                Some(Err(err)) => Err(anyhow::Error::new(err)),
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn returns_chunk_when_available() {
        let mut s = stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(b"abc".to_vec())]);
        let cancel = CancellationToken::new();
        let chunk = next_chunk_or_cancel(&mut s, &cancel).await.unwrap();
        assert_eq!(chunk, Some(b"abc".to_vec()));
    }

    #[tokio::test]
    async fn returns_none_on_eof() {
        let mut s = stream::iter(Vec::<Result<Vec<u8>, std::io::Error>>::new());
        let cancel = CancellationToken::new();
        let chunk = next_chunk_or_cancel(&mut s, &cancel).await.unwrap();
        assert_eq!(chunk, None);
    }

    #[tokio::test]
    async fn cancellation_wins_immediately() {
        let mut s = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = next_chunk_or_cancel(&mut s, &cancel).await.unwrap_err();
        assert!(err.to_string().contains("Aborted"));
    }
}
