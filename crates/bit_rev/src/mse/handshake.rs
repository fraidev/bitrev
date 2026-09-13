use std::io::{self, ErrorKind};
use std::time::Duration;

use rand::rngs::OsRng;
use rand::RngCore;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::transport::{PrefixedStream, BT_HANDSHAKE_HEAD};

use super::dh::{DhKeys, PUBLIC_KEY_LEN};
use super::hash::{find_slice, hash_req1, hash_req2, hash_req3, key_a, key_b, xor20};
use super::rc4::Rc4;
use super::stream::EncryptedStream;
use super::{CryptoMethod, CRYPTO_PLAINTEXT, CRYPTO_RC4};

pub const MAX_PAD: usize = 512;
pub const VC_LEN: usize = 8;
pub const MAX_IA: usize = 65535;
/// Pubkey (96) + max DH padding (512). Payload is read after this window.
pub const DH_WINDOW: usize = PUBLIC_KEY_LEN + MAX_PAD;
/// PadB (512) + encrypted VC (8).
const INITIATOR_SYNC_MAX: usize = MAX_PAD + VC_LEN;
/// PadA (512) + HASH('req1', S) (20).
const RESPONDER_SYNC_MAX: usize = MAX_PAD + 20;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

const VC: [u8; VC_LEN] = [0; VC_LEN];

#[derive(Debug, Error)]
pub enum MseError {
    #[error("mse handshake timed out")]
    Timeout,
    #[error("mse i/o: {0}")]
    Io(#[from] io::Error),
    #[error("mse sync marker not found")]
    SyncNotFound,
    #[error("mse unknown torrent")]
    UnknownTorrent,
    #[error("mse crypto negotiation failed")]
    CryptoNegotiation,
    #[error("mse invalid verification constant")]
    InvalidVc,
    #[error("mse padding too large ({0})")]
    PaddingTooLarge(u16),
    #[error("mse initial payload too large ({0})")]
    InitialPayloadTooLarge(u16),
    #[error("mse plaintext not allowed")]
    PlaintextNotAllowed,
    #[error("mse peer sent a plaintext bittorrent handshake")]
    PlaintextPeer,
}

impl From<MseError> for io::Error {
    fn from(err: MseError) -> Self {
        match err {
            MseError::Io(e) => e,
            MseError::Timeout => io::Error::new(ErrorKind::TimedOut, err),
            other => io::Error::new(ErrorKind::InvalidData, other),
        }
    }
}

pub struct HandshakeOutcome<S> {
    pub stream: EncryptedStream<S>,
    pub selected: CryptoMethod,
    pub skey: [u8; 20],
}

pub async fn initiate<S>(
    stream: S,
    skey: [u8; 20],
    provide: u32,
    ia: &[u8],
    allow_plaintext: bool,
) -> Result<HandshakeOutcome<S>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    with_timeout(initiate_inner(stream, skey, provide, ia, allow_plaintext)).await
}

pub async fn respond<S, Lookup, Select>(
    stream: S,
    lookup: Lookup,
    select: Select,
) -> Result<HandshakeOutcome<S>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Lookup: FnMut([u8; 20]) -> Option<[u8; 20]>,
    Select: FnOnce(u32) -> Option<CryptoMethod>,
{
    with_timeout(respond_inner(stream, lookup, select)).await
}

async fn with_timeout<F, T>(fut: F) -> Result<T, MseError>
where
    F: std::future::Future<Output = Result<T, MseError>>,
{
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await {
        Ok(result) => result,
        Err(_) => Err(MseError::Timeout),
    }
}

async fn initiate_inner<S>(
    mut stream: S,
    skey: [u8; 20],
    provide: u32,
    ia: &[u8],
    allow_plaintext: bool,
) -> Result<HandshakeOutcome<S>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if ia.len() > MAX_IA {
        return Err(MseError::InitialPayloadTooLarge(ia.len() as u16));
    }
    if provide & (CRYPTO_PLAINTEXT | CRYPTO_RC4) == 0 {
        return Err(MseError::CryptoNegotiation);
    }

    let keys = DhKeys::generate();
    let pad_a = random_pad();
    stream.write_all(&keys.public).await?;
    stream.write_all(&pad_a).await?;
    stream.flush().await?;

    let mut yb = [0u8; PUBLIC_KEY_LEN];
    stream
        .read_exact(&mut yb[..BT_HANDSHAKE_HEAD.len()])
        .await?;
    if yb.starts_with(BT_HANDSHAKE_HEAD) {
        return Err(MseError::PlaintextPeer);
    }
    stream
        .read_exact(&mut yb[BT_HANDSHAKE_HEAD.len()..])
        .await?;

    let s = keys.shared_secret(&yb);
    let mut enc = Rc4::for_mse(&key_a(&s, &skey));
    let mut dec = Rc4::for_mse(&key_b(&s, &skey));

    let pad_c = random_pad();
    let mut encrypted = Vec::with_capacity(VC_LEN + 4 + 2 + pad_c.len() + 2 + ia.len());
    encrypted.extend_from_slice(&VC);
    encrypted.extend_from_slice(&provide.to_be_bytes());
    encrypted.extend_from_slice(&(pad_c.len() as u16).to_be_bytes());
    encrypted.extend_from_slice(&pad_c);
    encrypted.extend_from_slice(&(ia.len() as u16).to_be_bytes());
    encrypted.extend_from_slice(ia);
    enc.apply(&mut encrypted);

    let mut step3 = Vec::with_capacity(40 + encrypted.len());
    step3.extend_from_slice(&hash_req1(&s));
    step3.extend_from_slice(&xor20(&hash_req2(&skey), &hash_req3(&s)));
    step3.extend_from_slice(&encrypted);
    stream.write_all(&step3).await?;
    stream.flush().await?;

    let mut expected_vc = VC;
    let mut dec_probe = dec.clone();
    dec_probe.apply(&mut expected_vc);
    let leftover = read_until(&mut stream, &expected_vc, INITIATOR_SYNC_MAX).await?;
    dec.apply(&mut [0u8; VC_LEN]);

    let mut stream = PrefixedStream::new(leftover, stream);
    let crypto_select = read_u32_dec(&mut stream, &mut dec).await?;
    let pad_d_len = read_u16_dec(&mut stream, &mut dec).await?;
    if usize::from(pad_d_len) > MAX_PAD {
        return Err(MseError::PaddingTooLarge(pad_d_len));
    }
    skip_dec(&mut stream, &mut dec, usize::from(pad_d_len)).await?;

    let selected = method_from_select(crypto_select, provide, allow_plaintext)?;
    let (rest, inner) = stream.into_parts();
    Ok(HandshakeOutcome {
        stream: finish_stream(inner, selected, enc, dec, rest),
        selected,
        skey,
    })
}

async fn respond_inner<S, Lookup, Select>(
    mut stream: S,
    mut lookup: Lookup,
    select: Select,
) -> Result<HandshakeOutcome<S>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Lookup: FnMut([u8; 20]) -> Option<[u8; 20]>,
    Select: FnOnce(u32) -> Option<CryptoMethod>,
{
    let keys = DhKeys::generate();
    let mut ya = [0u8; PUBLIC_KEY_LEN];
    stream.read_exact(&mut ya).await?;
    let s = keys.shared_secret(&ya);

    let pad_b = random_pad();
    stream.write_all(&keys.public).await?;
    stream.write_all(&pad_b).await?;
    stream.flush().await?;

    let leftover = read_until(&mut stream, &hash_req1(&s), RESPONDER_SYNC_MAX).await?;
    let mut stream = PrefixedStream::new(leftover, stream);

    let mut req2_xor = [0u8; 20];
    stream.read_exact(&mut req2_xor).await?;
    let req2 = xor20(&req2_xor, &hash_req3(&s));
    let skey = lookup(req2).ok_or(MseError::UnknownTorrent)?;

    let mut dec = Rc4::for_mse(&key_a(&s, &skey));
    let mut enc = Rc4::for_mse(&key_b(&s, &skey));

    let mut vc = [0u8; VC_LEN];
    stream.read_exact(&mut vc).await?;
    dec.apply(&mut vc);
    if vc != VC {
        return Err(MseError::InvalidVc);
    }

    let provide = read_u32_dec(&mut stream, &mut dec).await?;
    let pad_c_len = read_u16_dec(&mut stream, &mut dec).await?;
    if usize::from(pad_c_len) > MAX_PAD {
        return Err(MseError::PaddingTooLarge(pad_c_len));
    }
    skip_dec(&mut stream, &mut dec, usize::from(pad_c_len)).await?;
    let ia_len = read_u16_dec(&mut stream, &mut dec).await?;
    if usize::from(ia_len) > MAX_IA {
        return Err(MseError::InitialPayloadTooLarge(ia_len));
    }
    let mut ia = vec![0u8; usize::from(ia_len)];
    if !ia.is_empty() {
        stream.read_exact(&mut ia).await?;
        dec.apply(&mut ia);
    }

    let selected = select(provide).ok_or(MseError::CryptoNegotiation)?;
    if selected == CryptoMethod::Plaintext && provide & CRYPTO_PLAINTEXT == 0 {
        return Err(MseError::CryptoNegotiation);
    }
    if selected == CryptoMethod::Rc4 && provide & CRYPTO_RC4 == 0 {
        return Err(MseError::CryptoNegotiation);
    }

    let pad_d = random_pad();
    let mut reply = Vec::with_capacity(VC_LEN + 4 + 2 + pad_d.len());
    reply.extend_from_slice(&VC);
    reply.extend_from_slice(&selected.as_u32().to_be_bytes());
    reply.extend_from_slice(&(pad_d.len() as u16).to_be_bytes());
    reply.extend_from_slice(&pad_d);
    enc.apply(&mut reply);
    stream.write_all(&reply).await?;
    stream.flush().await?;

    let (rest, inner) = stream.into_parts();
    let mut prefix = ia;
    prefix.extend_from_slice(&rest);
    Ok(HandshakeOutcome {
        stream: finish_stream(inner, selected, enc, dec, prefix),
        selected,
        skey,
    })
}

fn finish_stream<S>(
    stream: S,
    selected: CryptoMethod,
    enc: Rc4,
    dec: Rc4,
    prefix: Vec<u8>,
) -> EncryptedStream<S> {
    match selected {
        CryptoMethod::Rc4 => EncryptedStream::rc4(stream, enc, dec, prefix),
        CryptoMethod::Plaintext => EncryptedStream::plaintext(stream, prefix),
    }
}

fn method_from_select(
    crypto_select: u32,
    provide: u32,
    allow_plaintext: bool,
) -> Result<CryptoMethod, MseError> {
    let chosen = crypto_select & provide;
    match chosen {
        CRYPTO_RC4 => Ok(CryptoMethod::Rc4),
        CRYPTO_PLAINTEXT => {
            if allow_plaintext {
                Ok(CryptoMethod::Plaintext)
            } else {
                Err(MseError::PlaintextNotAllowed)
            }
        }
        _ => Err(MseError::CryptoNegotiation),
    }
}

pub fn lookup_skey<'a>(
    req2_hash: [u8; 20],
    keys: impl IntoIterator<Item = &'a [u8; 20]>,
) -> Option<[u8; 20]> {
    keys.into_iter()
        .copied()
        .find(|skey| hash_req2(skey) == req2_hash)
}

fn random_pad() -> Vec<u8> {
    let mut len_bytes = [0u8; 2];
    OsRng.fill_bytes(&mut len_bytes);
    let len = (u16::from_be_bytes(len_bytes) as usize) % (MAX_PAD + 1);
    let mut pad = vec![0u8; len];
    if !pad.is_empty() {
        OsRng.fill_bytes(&mut pad);
    }
    pad
}

async fn read_until<S: AsyncRead + Unpin>(
    stream: &mut S,
    pattern: &[u8],
    max: usize,
) -> Result<Vec<u8>, MseError> {
    let mut buf = Vec::with_capacity(pattern.len() + 64);
    let mut tmp = [0u8; 64];
    let mut read_total = 0usize;
    while read_total < max {
        let want = (max - read_total).min(tmp.len());
        let n = stream.read(&mut tmp[..want]).await?;
        if n == 0 {
            return Err(MseError::Io(io::Error::new(
                ErrorKind::UnexpectedEof,
                "mse eof during sync",
            )));
        }
        read_total += n;
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_slice(&buf, pattern) {
            return Ok(buf[pos + pattern.len()..].to_vec());
        }
        if buf.len() > pattern.len() {
            let keep = pattern.len() - 1;
            buf.drain(..buf.len() - keep);
        }
    }
    Err(MseError::SyncNotFound)
}

async fn read_u32_dec<S: AsyncRead + Unpin>(
    stream: &mut S,
    dec: &mut Rc4,
) -> Result<u32, MseError> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;
    dec.apply(&mut buf);
    Ok(u32::from_be_bytes(buf))
}

async fn read_u16_dec<S: AsyncRead + Unpin>(
    stream: &mut S,
    dec: &mut Rc4,
) -> Result<u16, MseError> {
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    dec.apply(&mut buf);
    Ok(u16::from_be_bytes(buf))
}

async fn skip_dec<S: AsyncRead + Unpin>(
    stream: &mut S,
    dec: &mut Rc4,
    len: usize,
) -> Result<(), MseError> {
    let mut remaining = len;
    let mut tmp = [0u8; 64];
    while remaining > 0 {
        let n = remaining.min(tmp.len());
        stream.read_exact(&mut tmp[..n]).await?;
        dec.apply(&mut tmp[..n]);
        remaining -= n;
    }
    Ok(())
}
