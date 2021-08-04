use std::cmp;

use bytes::{BufMut, Bytes, BytesMut};
use http::{
    header::{CONNECTION, CONTENT_LENGTH, DATE, TRANSFER_ENCODING},
    response::Parts,
    StatusCode, Version,
};
use tracing::{debug, warn};

use crate::body::ResponseBodySize;
use crate::util::date::DATE_VALUE_LENGTH;

use super::buf::WriteBuf;
use super::codec::Kind;
use super::context::{ConnectionType, Context};
use super::error::{Parse, ProtoError};

impl<const MAX_HEADERS: usize> Context<'_, MAX_HEADERS> {
    pub(super) fn encode_continue<W, const WRITE_BUF_LIMIT: usize>(&mut self, buf: &mut W)
    where
        W: WriteBuf<WRITE_BUF_LIMIT>,
    {
        buf.write_static(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    pub(super) fn encode_head<W, const WRITE_BUF_LIMIT: usize>(
        &mut self,
        parts: Parts,
        size: ResponseBodySize,
        buf: &mut W,
    ) -> Result<TransferEncoding, ProtoError>
    where
        W: WriteBuf<WRITE_BUF_LIMIT>,
    {
        buf.write_head(|buf| self.encode_head_inner(parts, size, buf))
    }

    fn encode_head_inner(
        &mut self,
        mut parts: Parts,
        size: ResponseBodySize,
        buf: &mut BytesMut,
    ) -> Result<TransferEncoding, ProtoError> {
        let version = parts.version;
        let status = parts.status;

        // decide if content-length or transfer-encoding header would be skipped.
        let mut skip_len = match (status, version) {
            (StatusCode::SWITCHING_PROTOCOLS, _) => false,
            // Sending content-length or transfer-encoding header on 2xx response
            // to CONNECT is forbidden in RFC 7231.
            (s, _) if self.is_connect_method() && s.is_success() => true,
            (s, _) if s.is_informational() => {
                warn!(target: "h1_encode", "response with 1xx status code not supported");
                return Err(ProtoError::Parse(Parse::StatusCode));
            }
            _ => false,
        };

        // In some error cases, we don't know about the invalid message until already
        // pushing some bytes onto the `buf`. In those cases, we don't want to send
        // the half-pushed message, so rewind to before.
        // let orig_len = buf.len();

        // encode version, status code and reason
        encode_version_status_reason(buf, version, status);

        let mut skip_date = false;

        let mut encoding = TransferEncoding::eof();

        for (name, value) in parts.headers.drain() {
            let name = name.expect("Handling optional header name is not implemented");

            // TODO: more spec check needed. the current check barely does anything.
            match name {
                CONTENT_LENGTH => {
                    debug_assert!(!skip_len, "CONTENT_LENGTH header can not be set");
                    let value = value
                        .to_str()
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .ok_or(Parse::HeaderValue)?;
                    encoding = TransferEncoding::length(value);
                    skip_len = true;
                }
                TRANSFER_ENCODING => {
                    debug_assert!(!skip_len, "TRANSFER_ENCODING header can not be set");
                    encoding = TransferEncoding::chunked_from(self.ctype());
                    skip_len = true;
                }
                CONNECTION if self.is_force_close() => continue,
                CONNECTION => {
                    for val in value.to_str().map_err(|_| Parse::HeaderValue)?.split(',') {
                        let val = val.trim();

                        if val.eq_ignore_ascii_case("close") {
                            self.set_ctype(ConnectionType::Close);
                        } else if val.eq_ignore_ascii_case("keep-alive") {
                            self.set_ctype(ConnectionType::KeepAlive);
                        } else if val.eq_ignore_ascii_case("upgrade") {
                            self.set_ctype(ConnectionType::Upgrade);
                        }
                    }
                }
                DATE => skip_date = true,
                _ => {}
            }

            buf.put_slice(name.as_str().as_bytes());
            buf.put_slice(b": ");
            buf.put_slice(value.as_bytes());
            buf.put_slice(b"\r\n");
        }

        if self.is_force_close() {
            buf.put_slice(b"connection: close\r\n");
        }

        // encode transfer-encoding or content-length
        if !skip_len {
            match size {
                ResponseBodySize::None => {
                    encoding = TransferEncoding::eof();
                }
                ResponseBodySize::Stream => {
                    buf.put_slice(b"transfer-encoding: chunked\r\n");
                    encoding = TransferEncoding::chunked_from(self.ctype());
                }
                ResponseBodySize::Sized(size) => {
                    let mut buffer = itoa::Buffer::new();
                    buf.put_slice(b"content-length: ");
                    buf.put_slice(buffer.format(size).as_bytes());
                    buf.put_slice(b"\r\n");
                    encoding = TransferEncoding::length(size as u64);
                }
            }
        }

        // set date header if there is not any.
        if !skip_date {
            buf.reserve(DATE_VALUE_LENGTH + 8);
            buf.put_slice(b"date: ");
            buf.put_slice(self.date.borrow().date());
            buf.put_slice(b"\r\n\r\n");
        } else {
            buf.put_slice(b"\r\n");
        }

        // put header map back to cache.
        parts.headers.clear();
        self.header = Some(parts.headers);

        // put extension back to cache;
        parts.extensions.clear();
        self.extensions = parts.extensions;

        Ok(encoding)
    }
}

#[inline]
fn encode_version_status_reason<B: BufMut>(buf: &mut B, version: Version, status: StatusCode) {
    // encode version, status code and reason
    match (version, status) {
        // happy path shortcut.
        (Version::HTTP_11, StatusCode::OK) => {
            buf.put_slice(b"HTTP/1.1 200 OK\r\n");
            return;
        }
        (Version::HTTP_10, _) => {
            buf.put_slice(b"HTTP/1.0 ");
        }
        (Version::HTTP_11, _) => {
            buf.put_slice(b"HTTP/1.1 ");
        }
        _ => {
            debug!(target: "h1_encode", "response with unexpected response version");
            buf.put_slice(b"HTTP/1.1 ");
        }
    }

    buf.put_slice(status.as_str().as_bytes());
    buf.put_slice(b" ");
    // a reason MUST be written, as many parsers will expect it.
    buf.put_slice(status.canonical_reason().unwrap_or("<none>").as_bytes());
    buf.put_slice(b"\r\n");
}

/// Encoders to handle different Transfer-Encodings.
#[derive(Debug)]
pub(super) struct TransferEncoding {
    kind: Kind,
}

impl TransferEncoding {
    pub(super) const fn eof() -> Self {
        Self { kind: Kind::Eof }
    }

    pub(super) const fn chunked() -> Self {
        Self {
            kind: Kind::EncodeChunked,
        }
    }

    pub(super) const fn plain_chunked() -> Self {
        Self {
            kind: Kind::PlainChunked,
        }
    }

    pub(super) const fn length(len: u64) -> Self {
        Self {
            kind: Kind::Length(len),
        }
    }

    pub(super) fn chunked_from(ctype: ConnectionType) -> Self {
        if ctype == ConnectionType::Upgrade {
            Self::plain_chunked()
        } else {
            Self::chunked()
        }
    }

    /// Encode message. Return `EOF` state of encoder
    pub(super) fn encode<W, const WRITE_BUF_LIMIT: usize>(&mut self, mut bytes: Bytes, buf: &mut W)
    where
        W: WriteBuf<WRITE_BUF_LIMIT>,
    {
        // Skip encode empty bytes.
        // This is to avoid unnecessary extending on h1::proto::buf::ListWriteBuf when user
        // provided empty bytes by accident.
        if bytes.is_empty() {
            return;
        }

        match self.kind {
            Kind::Eof | Kind::PlainChunked => buf.write_buf(bytes),
            Kind::EncodeChunked => buf.write_chunk(bytes),
            Kind::Length(ref mut remaining) => {
                if *remaining > 0 {
                    let len = cmp::min(*remaining, bytes.len() as u64);
                    buf.write_buf(bytes.split_to(len as usize));
                    *remaining -= len as u64;
                }
            }
            _ => unreachable!(),
        }
    }

    /// Encode eof. Return `EOF` state of encoder
    pub(super) fn encode_eof<W, const WRITE_BUF_LIMIT: usize>(&mut self, buf: &mut W)
    where
        W: WriteBuf<WRITE_BUF_LIMIT>,
    {
        match self.kind {
            Kind::Eof | Kind::PlainChunked | Kind::Length(0) => {}
            Kind::EncodeChunked => buf.write_static(b"0\r\n\r\n"),
            Kind::Length(n) => unreachable!("UnexpectedEof for Length Body with {} remaining", n),
            _ => unreachable!(),
        }
    }
}