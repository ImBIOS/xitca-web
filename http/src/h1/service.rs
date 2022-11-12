use std::{future::Future, net::SocketAddr};

use futures_core::Stream;
use xitca_io::io::AsyncIo;
use xitca_service::Service;
use xitca_unsafe_collection::pin;

use crate::{
    bytes::Bytes,
    error::{HttpServiceError, TimeoutError},
    http::Response,
    request::Request,
    service::HttpService,
    util::futures::Timeout,
};

use super::{body::RequestBody, proto};

pub type H1Service<St, S, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> =
    HttpService<St, S, RequestBody, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>;

impl<St, S, B, BE, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    Service<(St, SocketAddr)> for H1Service<St, S, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Service<Request<RequestBody>, Response = Response<B>>,
    A: Service<St>,
    St: AsyncIo,
    A::Response: AsyncIo,
    B: Stream<Item = Result<Bytes, BE>>,
    HttpServiceError<S::Error, BE>: From<A::Error>,
{
    type Response = ();
    type Error = HttpServiceError<S::Error, BE>;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> + 'f where Self: 'f;

    fn call<'s>(&'s self, (io, _): (St, SocketAddr)) -> Self::Future<'s>
    where
        St: 's,
    {
        async {
            // tls accept timer.
            let timer = self.keep_alive();
            pin!(timer);

            let mut io = self
                .tls_acceptor
                .call(io)
                .timeout(timer.as_mut())
                .await
                .map_err(|_| HttpServiceError::Timeout(TimeoutError::TlsAccept))??;

            // update timer to first request timeout.
            self.update_first_request_deadline(timer.as_mut());

            proto::run(&mut io, timer.as_mut(), self.config, &self.service, self.date.get())
                .await
                .map_err(Into::into)
        }
    }
}
