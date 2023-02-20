use core::{
    future::{pending, poll_fn, Future},
    pin::Pin,
};

use std::{io, sync::Arc};

use tokio::{
    sync::{mpsc::UnboundedReceiver, Notify},
    task::JoinHandle,
};
use xitca_io::{
    bytes::{Buf, BytesMut},
    io::{AsyncIo, Interest, Ready},
};
use xitca_unsafe_collection::{
    bytes::read_buf,
    futures::{Select as _, SelectOutput},
};

use crate::{
    error::{unexpected_eof_err, write_zero_err, Error},
    request::Request,
};

use super::context::Context;

pub struct BufferedIo<Io> {
    io: Io,
    write_buf: BytesMut,
    read_buf: BytesMut,
    rx: UnboundedReceiver<Request>,
    ctx: Context,
}

impl<Io> BufferedIo<Io>
where
    Io: AsyncIo,
{
    pub(crate) fn new(io: Io, rx: UnboundedReceiver<Request>) -> Self {
        Self {
            io,
            write_buf: BytesMut::new(),
            read_buf: BytesMut::new(),
            rx,
            ctx: Context::new(),
        }
    }

    fn handle_io(&mut self, ready: Ready) -> Result<(), Error> {
        if ready.is_readable() {
            loop {
                match read_buf(&mut self.io, &mut self.read_buf) {
                    Ok(0) => return Err(unexpected_eof_err()),
                    Ok(_) => continue,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            self.ctx.try_decode(&mut self.read_buf)?;
        }

        if ready.is_writable() {
            loop {
                match self.io.write(&self.write_buf) {
                    Ok(0) => return Err(write_zero_err()),
                    Ok(n) => {
                        self.write_buf.advance(n);
                        if self.write_buf.is_empty() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        Ok(())
    }

    pub async fn run(mut self) -> Result<(), Error> {
        self._run().await
    }

    pub(crate) fn spawn(mut self) -> Handle<Self>
    where
        Io: Send + 'static,
        for<'r> <Io as AsyncIo>::ReadyFuture<'r>: Send,
    {
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();
        let handle = tokio::task::spawn(async move {
            let _ = self._run().select(notify2.notified()).await;
            self
        });
        Handle { handle, notify }
    }

    async fn _run(&mut self) -> Result<(), Error> {
        loop {
            let want_write = !self.write_buf.is_empty();
            match try_rx(&mut self.rx, &mut self.ctx)
                .select(try_io(&mut self.io, want_write))
                .await
            {
                // batch message and keep polling.
                SelectOutput::A(Some(req)) => {
                    self.write_buf.extend_from_slice(req.msg.as_ref());
                    if let Some(tx) = req.tx {
                        self.ctx.push_concurrent_req(tx);
                    }
                }
                // client is gone.
                SelectOutput::A(None) => break,
                SelectOutput::B(ready) => {
                    let ready = ready?;
                    self.handle_io(ready)?;
                }
            }
        }

        self.shutdown().await
    }

    #[allow(clippy::manual_async_fn)]
    #[cold]
    #[inline(never)]
    fn shutdown(&mut self) -> impl Future<Output = Result<(), Error>> + '_ {
        async {
            loop {
                let want_write = !self.write_buf.is_empty();
                let want_read = !self.ctx.is_empty();
                let interest = match (want_read, want_write) {
                    (false, false) => break,
                    (true, true) => Interest::READABLE | Interest::WRITABLE,
                    (true, false) => Interest::READABLE,
                    (false, true) => Interest::WRITABLE,
                };
                let fut = self.io.ready(interest);
                let ready = fut.await?;
                self.handle_io(ready)?;
            }

            loop {
                match self.io.flush() {
                    Ok(_) => break,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
                let fut = self.io.ready(Interest::WRITABLE);
                fut.await?;
            }

            poll_fn(|cx| Pin::new(&mut self.io).poll_shutdown(cx))
                .await
                .map_err(Into::into)
        }
    }
}

pub(crate) struct Handle<Io> {
    handle: JoinHandle<Io>,
    notify: Arc<Notify>,
}

impl<Io> Handle<Io> {
    pub(crate) async fn into_inner(self) -> Io {
        self.notify.notify_waiters();
        self.handle.await.unwrap()
    }
}

async fn try_rx(rx: &mut UnboundedReceiver<Request>, ctx: &mut Context) -> Option<Request> {
    if ctx.throttled() {
        pending().await
    } else {
        rx.recv().await
    }
}

fn try_io<Io>(io: &mut Io, want_write: bool) -> Io::ReadyFuture<'_>
where
    Io: AsyncIo,
{
    let interest = if want_write {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };

    io.ready(interest)
}