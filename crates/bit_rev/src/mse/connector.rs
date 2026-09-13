use std::io;
use std::sync::Arc;

use tracing::debug;

use crate::transport::{
    boxed_stream, Connector, DialFuture, PeerConnected, PeerDial, PeerDialFuture,
};

use super::handshake::{initiate, MseError};
use super::{EncryptionPolicy, CRYPTO_PLAINTEXT, CRYPTO_RC4};

/// Wraps an inner [`Connector`] and applies the session encryption policy.
pub struct MseConnector {
    inner: Arc<dyn Connector>,
    policy: EncryptionPolicy,
}

impl MseConnector {
    pub fn new(inner: Arc<dyn Connector>, policy: EncryptionPolicy) -> Self {
        Self { inner, policy }
    }

    pub fn policy(&self) -> EncryptionPolicy {
        self.policy
    }
}

impl Connector for MseConnector {
    fn dial(&self, addr: std::net::SocketAddr) -> DialFuture<'_> {
        self.inner.dial(addr)
    }

    fn dial_peer(&self, req: PeerDial) -> PeerDialFuture<'_> {
        Box::pin(async move {
            match self.policy {
                EncryptionPolicy::Disabled => self.inner.dial_peer(req).await,
                EncryptionPolicy::PreferPlaintext => self.inner.dial_peer(req).await,
                EncryptionPolicy::RequireEncrypted => self.try_mse(req, CRYPTO_RC4, false).await,
                EncryptionPolicy::PreferEncrypted => {
                    match self
                        .try_mse(req.clone(), CRYPTO_PLAINTEXT | CRYPTO_RC4, true)
                        .await
                    {
                        Ok(connected) => Ok(connected),
                        Err(err) => {
                            debug!(addr = %req.addr, error = %err, "mse handshake failed, falling back to plaintext");
                            self.inner.dial_peer(req).await
                        }
                    }
                }
            }
        })
    }
}

impl MseConnector {
    async fn try_mse(
        &self,
        req: PeerDial,
        provide: u32,
        allow_plaintext: bool,
    ) -> io::Result<PeerConnected> {
        let stream = self.inner.dial(req.addr).await?;
        let outcome = initiate(
            stream,
            req.info_hash,
            provide,
            &req.initial_payload,
            allow_plaintext,
        )
        .await
        .map_err(io::Error::from)?;
        if !outcome.selected.is_rc4() && !self.policy.allows_plaintext() {
            return Err(MseError::PlaintextNotAllowed.into());
        }
        Ok(PeerConnected {
            encrypted: outcome.selected.is_rc4(),
            sent_initial_payload: true,
            stream: boxed_stream(outcome.stream),
        })
    }
}
