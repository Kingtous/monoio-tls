use std::{
    future::Future,
    io::{self, Read, Write},
    ops::{Deref, DerefMut},
};

use monoio::{
    io::{AsyncReadRent, AsyncWriteRent},
    BufResult,
};
use rustls::{ConnectionCommon, SideData};

use crate::unsafe_io::{UnsafeRead, UnsafeWrite};

#[derive(Debug)]
pub(crate) struct Stream<IO, C> {
    pub(crate) io: IO,
    pub(crate) session: C,
}

impl<IO, C> Stream<IO, C> {
    pub fn new(io: IO, session: C) -> Self {
        Self { io, session }
    }
}

impl<IO: AsyncReadRent + AsyncWriteRent, C, SD: SideData> Stream<IO, C>
where
    C: DerefMut + Deref<Target = ConnectionCommon<SD>>,
{
    pub(crate) async fn read_io(&mut self) -> io::Result<usize> {
        let mut unsafe_read = UnsafeRead::default();

        let n = loop {
            match self.session.read_tls(&mut unsafe_read) {
                Ok(n) => {
                    break n;
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    unsafe { unsafe_read.do_io(&mut self.io).await? };
                    continue;
                }
                Err(err) => return Err(err),
            }
        };

        let state = match self.session.process_new_packets() {
            Ok(state) => state,
            Err(err) => {
                // TODO(ihciah): when to write_io? If we do this in read call, the UnsafeWrite may crash
                // when we impl split in an UnsafeCell way.
                let _ = self.write_io().await;
                return Err(io::Error::new(io::ErrorKind::InvalidData, err));
            }
        };

        if state.peer_has_closed() && self.session.is_handshaking() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tls handshake alert",
            ));
        }

        Ok(n)
    }

    pub(crate) async fn write_io(&mut self) -> io::Result<usize> {
        let mut unsafe_write = UnsafeWrite::default();

        let n = loop {
            match self.session.write_tls(&mut unsafe_write) {
                Ok(n) => {
                    break n;
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    unsafe { unsafe_write.do_io(&mut self.io).await? };
                    continue;
                }
                Err(err) => return Err(err),
            }
        };

        Ok(n)
    }

    pub(crate) async fn handshake(&mut self) -> io::Result<(usize, usize)> {
        let mut wrlen = 0;
        let mut rdlen = 0;
        let mut eof = false;

        loop {
            while self.session.wants_write() && self.session.is_handshaking() {
                wrlen += self.write_io().await?;
            }
            while !eof && self.session.wants_read() && self.session.is_handshaking() {
                let n = self.read_io().await?;
                rdlen += n;
                if n == 0 {
                    eof = true;
                }
            }

            match (eof, self.session.is_handshaking()) {
                (true, true) => {
                    let err = io::Error::new(io::ErrorKind::UnexpectedEof, "tls handshake eof");
                    return Err(err);
                }
                (false, true) => (),
                (_, false) => {
                    break;
                }
            };
        }

        // flush buffer
        while self.session.wants_write() {
            wrlen += self.write_io().await?;
        }

        Ok((rdlen, wrlen))
    }
}

impl<IO: AsyncReadRent + AsyncWriteRent, C, SD: SideData> AsyncReadRent for Stream<IO, C>
where
    C: DerefMut + Deref<Target = ConnectionCommon<SD>>,
{
    type ReadFuture<'a, T> = impl Future<Output = BufResult<usize, T>>
    where
        T: 'a, Self: 'a;

    type ReadvFuture<'a, T> = impl Future<Output = BufResult<usize, T>>
    where
        T: 'a, Self: 'a;

    fn read<T: monoio::buf::IoBufMut>(&mut self, mut buf: T) -> Self::ReadFuture<'_, T> {
        let slice = unsafe { std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total()) };
        async move {
            loop {
                // read from rustls to buffer
                match self.session.reader().read(slice) {
                    Ok(n) => {
                        unsafe { buf.set_init(n) };
                        return (Ok(n), buf);
                    }
                    // we need more data, read something.
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => {
                        return (Err(e), buf);
                    }
                }

                // now we need data, read something into rustls
                match self.read_io().await {
                    Ok(0) => {
                        return (
                            Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "tls raw stream eof",
                            )),
                            buf,
                        );
                    }
                    Ok(_) => (),
                    Err(e) => {
                        return (Err(e), buf);
                    }
                };
            }
        }
    }

    fn readv<T: monoio::buf::IoVecBufMut>(&mut self, buf: T) -> Self::ReadvFuture<'_, T> {
        // TODO
        async move {
            let _ = buf;
            todo!()
        }
    }
}

impl<IO: AsyncReadRent + AsyncWriteRent, C, SD: SideData> AsyncWriteRent for Stream<IO, C>
where
    C: DerefMut + Deref<Target = ConnectionCommon<SD>>,
{
    type WriteFuture<'a, T> = impl Future<Output = BufResult<usize, T>>
    where
        T: 'a, Self: 'a;

    type WritevFuture<'a, T> = impl Future<Output = BufResult<usize, T>>
    where
        T: 'a, Self: 'a;

    type ShutdownFuture<'a> = impl Future<Output = Result<(), std::io::Error>>
    where
        Self: 'a;

    fn write<T: monoio::buf::IoBuf>(&mut self, buf: T) -> Self::WriteFuture<'_, T> {
        async move {
            // construct slice
            let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };

            // write slice to rustls
            let n = match self.session.writer().write(slice) {
                Ok(n) => n,
                Err(e) => return (Err(e), buf),
            };

            // write from rustls to connection
            while self.session.wants_write() {
                match self.write_io().await {
                    Ok(0) => {
                        break;
                    }
                    Ok(_) => (),
                    Err(e) => return (Err(e), buf),
                }
            }
            (Ok(n), buf)
        }
    }

    fn writev<T: monoio::buf::IoVecBuf>(&mut self, buf_vec: T) -> Self::WritevFuture<'_, T> {
        async move {
            let _ = buf_vec;
            todo!()
        }
    }

    fn shutdown(&mut self) -> Self::ShutdownFuture<'_> {
        self.session.send_close_notify();
        async move {
            while self.session.wants_write() {
                self.write_io().await?;
            }
            self.io.shutdown().await
        }
    }
}
