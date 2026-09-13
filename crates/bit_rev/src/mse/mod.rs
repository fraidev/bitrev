//! Message Stream Encryption / Protocol Encryption (Vuze MSE/PE).
//!
//! This is DPI obfuscation, not confidentiality. RC4 with a DH-derived key
//! has no modern security guarantees.

mod connector;
mod dh;
mod handshake;
mod hash;
mod rc4;
mod stream;

pub use connector::MseConnector;
pub use handshake::{
    initiate, lookup_skey, respond, HandshakeOutcome, MseError, DH_WINDOW, HANDSHAKE_TIMEOUT,
    MAX_IA, MAX_PAD,
};
pub use stream::EncryptedStream;

use crate::config::EncryptionMode;

pub const CRYPTO_PLAINTEXT: u32 = 0x01;
pub const CRYPTO_RC4: u32 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoMethod {
    Plaintext,
    Rc4,
}

impl CryptoMethod {
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Plaintext => CRYPTO_PLAINTEXT,
            Self::Rc4 => CRYPTO_RC4,
        }
    }

    pub fn is_rc4(self) -> bool {
        matches!(self, Self::Rc4)
    }
}

/// Session-level MSE policy. Same variants as [`EncryptionMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionPolicy {
    Disabled,
    PreferPlaintext,
    #[default]
    PreferEncrypted,
    RequireEncrypted,
}

impl EncryptionPolicy {
    pub fn crypto_provide(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::RequireEncrypted => CRYPTO_RC4,
            Self::PreferPlaintext | Self::PreferEncrypted => CRYPTO_PLAINTEXT | CRYPTO_RC4,
        }
    }

    pub fn select(self, provided: u32) -> Option<CryptoMethod> {
        let plain = provided & CRYPTO_PLAINTEXT != 0;
        let rc4 = provided & CRYPTO_RC4 != 0;
        match self {
            Self::Disabled => None,
            Self::RequireEncrypted => rc4.then_some(CryptoMethod::Rc4),
            Self::PreferEncrypted => {
                if rc4 {
                    Some(CryptoMethod::Rc4)
                } else if plain {
                    Some(CryptoMethod::Plaintext)
                } else {
                    None
                }
            }
            Self::PreferPlaintext => {
                if plain {
                    Some(CryptoMethod::Plaintext)
                } else if rc4 {
                    Some(CryptoMethod::Rc4)
                } else {
                    None
                }
            }
        }
    }

    pub fn allows_plaintext(self) -> bool {
        !matches!(self, Self::RequireEncrypted)
    }

    pub fn allows_mse(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn allows_fallback(self) -> bool {
        matches!(self, Self::PreferEncrypted | Self::PreferPlaintext)
    }

    pub fn prefer_mse_first(self) -> bool {
        matches!(self, Self::PreferEncrypted | Self::RequireEncrypted)
    }
}

impl From<EncryptionMode> for EncryptionPolicy {
    fn from(mode: EncryptionMode) -> Self {
        match mode {
            EncryptionMode::Disabled => Self::Disabled,
            EncryptionMode::PreferPlaintext => Self::PreferPlaintext,
            EncryptionMode::PreferEncrypted => Self::PreferEncrypted,
            EncryptionMode::RequireEncrypted => Self::RequireEncrypted,
        }
    }
}

impl From<EncryptionPolicy> for EncryptionMode {
    fn from(policy: EncryptionPolicy) -> Self {
        match policy {
            EncryptionPolicy::Disabled => Self::Disabled,
            EncryptionPolicy::PreferPlaintext => Self::PreferPlaintext,
            EncryptionPolicy::PreferEncrypted => Self::PreferEncrypted,
            EncryptionPolicy::RequireEncrypted => Self::RequireEncrypted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SKEY: [u8; 20] = *b"0123456789infohash!!";
    const IA: &[u8] = b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00";

    async fn loopback(
        provide: u32,
        select: CryptoMethod,
        allow_plaintext: bool,
    ) -> (CryptoMethod, CryptoMethod) {
        let (a, b) = tokio::io::duplex(8 * 1024);
        let responder = tokio::spawn(async move {
            respond(
                b,
                |req2| lookup_skey(req2, [&SKEY]),
                |provided| {
                    let policy = match select {
                        CryptoMethod::Rc4 => EncryptionPolicy::PreferEncrypted,
                        CryptoMethod::Plaintext => EncryptionPolicy::PreferPlaintext,
                    };
                    let chosen = policy.select(provided)?;
                    (chosen == select).then_some(select)
                },
            )
            .await
        });
        let init = initiate(a, SKEY, provide, IA, allow_plaintext)
            .await
            .expect("initiator");
        let resp = responder.await.expect("join").expect("responder");
        assert_eq!(init.selected, select);
        assert_eq!(resp.selected, select);
        assert_eq!(resp.skey, SKEY);

        let mut init_stream = init.stream;
        let mut resp_stream = resp.stream;
        let mut ia_buf = vec![0u8; IA.len()];
        resp_stream.read_exact(&mut ia_buf).await.unwrap();
        assert_eq!(ia_buf, IA);

        init_stream.write_all(b"ping").await.unwrap();
        init_stream.flush().await.unwrap();
        let mut ping = [0u8; 4];
        resp_stream.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");

        resp_stream.write_all(b"pong").await.unwrap();
        resp_stream.flush().await.unwrap();
        let mut pong = [0u8; 4];
        init_stream.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");

        (init.selected, resp.selected)
    }

    #[tokio::test]
    async fn provide_plaintext_select_plaintext() {
        let (a, b) = loopback(CRYPTO_PLAINTEXT, CryptoMethod::Plaintext, true).await;
        assert_eq!(a, CryptoMethod::Plaintext);
        assert_eq!(b, CryptoMethod::Plaintext);
    }

    #[tokio::test]
    async fn provide_rc4_select_rc4() {
        let (a, b) = loopback(CRYPTO_RC4, CryptoMethod::Rc4, false).await;
        assert_eq!(a, CryptoMethod::Rc4);
        assert_eq!(b, CryptoMethod::Rc4);
    }

    #[tokio::test]
    async fn provide_both_select_plaintext() {
        let (a, b) = loopback(CRYPTO_PLAINTEXT | CRYPTO_RC4, CryptoMethod::Plaintext, true).await;
        assert_eq!(a, CryptoMethod::Plaintext);
        assert_eq!(b, CryptoMethod::Plaintext);
    }

    #[tokio::test]
    async fn provide_both_select_rc4() {
        let (a, b) = loopback(CRYPTO_PLAINTEXT | CRYPTO_RC4, CryptoMethod::Rc4, true).await;
        assert_eq!(a, CryptoMethod::Rc4);
        assert_eq!(b, CryptoMethod::Rc4);
    }

    #[tokio::test]
    async fn wrong_skey_fails_cleanly() {
        let (a, b) = tokio::io::duplex(8 * 1024);
        let other = *b"other-info-hash!!!!!";
        let responder = tokio::spawn(async move {
            respond(
                b,
                |req2| lookup_skey(req2, [&other]),
                |_| Some(CryptoMethod::Rc4),
            )
            .await
        });
        let init = initiate(a, SKEY, CRYPTO_RC4, IA, false).await;
        let resp = responder.await.expect("join");
        assert!(init.is_err() || resp.is_err());
        if let Err(err) = resp {
            assert!(matches!(err, MseError::UnknownTorrent | MseError::Timeout));
        }
    }

    #[tokio::test]
    async fn require_encrypted_refuses_plaintext_select() {
        let (a, b) = tokio::io::duplex(8 * 1024);
        let responder = tokio::spawn(async move {
            respond(
                b,
                |req2| lookup_skey(req2, [&SKEY]),
                |_| Some(CryptoMethod::Plaintext),
            )
            .await
        });
        let init = initiate(a, SKEY, CRYPTO_PLAINTEXT | CRYPTO_RC4, IA, false).await;
        let _ = responder.await;
        assert!(matches!(init, Err(MseError::PlaintextNotAllowed)));
    }

    #[tokio::test]
    async fn initiator_detects_plaintext_peer() {
        let (a, mut b) = tokio::io::duplex(8 * 1024);
        tokio::spawn(async move {
            let hs = crate::handshake::Handshake::new(SKEY, *b"-LC0001-0123456789ab");
            let _ = b.write_all(&hs.serialize()).await;
        });
        let err = match initiate(a, SKEY, CRYPTO_RC4, IA, false).await {
            Ok(_) => panic!("plaintext peer must fail mse"),
            Err(err) => err,
        };
        assert!(matches!(err, MseError::PlaintextPeer | MseError::Timeout));
    }
}
