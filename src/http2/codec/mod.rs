// Copyright 2022 - 2023 Wenmeng See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// Author: tickbh
// -----
// Created Date: 2023/09/14 09:42:25

mod error;
mod framed_read;
mod framed_write;

use std::io;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use algorithm::buf::BinaryMut;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_stream::Stream;
use tokio_util::codec::length_delimited;
use webparse::http::http2::encoder::Encoder;
use webparse::http::http2::frame::Frame;
use webparse::http::http2::{
    HeaderIndex, DEFAULT_MAX_FRAME_SIZE, DEFAULT_SETTINGS_HEADER_TABLE_SIZE,
};

use crate::ProtResult;

pub use self::framed_read::FramedRead;
pub use self::framed_write::FramedWrite;

#[derive(Debug)]
pub struct Codec<T> {
    inner: FramedRead<FramedWrite<T>>,
    header_index: Arc<RwLock<HeaderIndex>>,
    header_table_size: usize,
    max_send_frame_size: usize,
}

impl<T> Codec<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Returns a new `Codec` with the default max frame size
    #[inline]
    pub fn new(io: T) -> Self {
        Self::with_max_recv_frame_size(io, DEFAULT_MAX_FRAME_SIZE as usize)
    }

    pub fn into_io_with_cache(self) -> (T, BinaryMut, BinaryMut) {
        use bytes::Buf;
        let bytes = self.inner.get_read_buffer();
        let read = BinaryMut::from(bytes.chunk().to_vec());
        let write = self.inner.get_ref().get_bytes().clone();
        (self.inner.into_io().into_io(), read, write)
    }

    pub fn into_io(self) -> T {
        // self.inner.get_mut().get_bytes()
        self.inner.into_io().into_io()
    }

    /// Returns a new `Codec` with the given maximum frame size
    pub fn with_max_recv_frame_size(io: T, _max_frame_size: usize) -> Self {
        // Wrap with writer
        let framed_write = FramedWrite::new(io);

        // Delimit the frames
        let delimited = length_delimited::Builder::new()
            .big_endian()
            .length_field_length(3)
            .length_adjustment(9)
            .num_skip(0) // Don't skip the header
            .new_read(framed_write);
        let header_index = Arc::new(RwLock::new(HeaderIndex::new()));
        let inner = FramedRead::new(delimited);

        // Use FramedRead's method since it checks the value is within range.
        // inner.set_max_frame_size(max_frame_size);

        Codec {
            inner,
            header_index,
            header_table_size: DEFAULT_SETTINGS_HEADER_TABLE_SIZE,
            max_send_frame_size: DEFAULT_MAX_FRAME_SIZE as usize,
        }
    }

    pub fn is_write_end(&self) -> bool {
        self.inner.get_ref().is_write_end()
    }

    pub fn get_reader(&mut self) -> &mut FramedRead<FramedWrite<T>> {
        &mut self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().get_mut()
    }

    // pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
    //     // self.get_mut().read_exact(buf)
    // }

    /// Returns `Ready` when the codec can buffer a frame
    pub fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        self.framed_write().poll_ready(cx)
    }

    /// Returns `Ready` when the codec can buffer a frame
    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        self.framed_write().flush(cx)
    }

    fn framed_write(&mut self) -> &mut FramedWrite<T> {
        self.inner.get_mut()
    }

    pub fn send_frame(&mut self, frame: Frame) -> ProtResult<usize> {
        log::trace!("HTTP2:发送帧数据: {:?}", frame);
        let mut encoder = Encoder::new_index(self.header_index.clone(), self.max_send_frame_size);
        let usize = frame.encode(self.framed_write().get_mut_bytes(), &mut encoder)?;
        Ok(usize)
    }

    pub fn set_send_header_table_size(&mut self, size: usize) {
        self.header_table_size = size;
        if let Ok(mut header) = self.header_index.write() {
            header.set_max_table_size(size);
        }
    }

    pub fn set_max_send_frame_size(&mut self, size: usize) {
        self.max_send_frame_size = size;
    }

    pub fn shutdown(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        self.framed_write().shutdown(cx)
    }

    pub fn set_cache_buf(&mut self, read_buf: BinaryMut, write_buf: BinaryMut) {
        self.inner.set_cache_buf(read_buf);
        self.framed_write().set_cache_buf(write_buf);
    }
}

impl<T> Stream for Codec<T>
where
    T: AsyncRead + Unpin,
{
    type Item = ProtResult<Frame>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}
