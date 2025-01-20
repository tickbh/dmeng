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

use brotli::{CompressorWriter, Decompressor};
use flate2::{
    write::{DeflateEncoder, GzEncoder},
    Compression, read::{GzDecoder, DeflateDecoder},
};
use tokio_util::sync::PollSemaphore;

use std::{fmt::Debug, io::{self, Error}, sync::Arc};
use std::{
    fmt::Display,
    io::{Read, Write},
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, ReadBuf, AsyncSeekExt},
    sync::{mpsc::Receiver, OwnedSemaphorePermit, Semaphore},
};
use algorithm::buf::{Binary, BinaryMut, Bt, BtMut};
use webparse::{Helper, Serialize, WebResult};

use crate::{Consts, ProtResult};

use super::layer::RateLimitLayer;


fn read_all_data<R: Read>(read_buf: &mut BinaryMut, read: &mut Box<R>) -> io::Result<usize> {
    let mut cache_buf = vec![0u8; 4096];
    let mut size = 0;
    loop {
        let s = read.read(&mut cache_buf)?;
        size += s;
        read_buf.put_slice(&cache_buf[..s]);
        if s < cache_buf.len() {
            return Ok(size)
        }
    }
}

#[derive(Debug)]
struct InnerReceiver {
    receiver: Option<Receiver<(bool, Binary)>>,
    file: Option<Box<File>>,
    cache_buf: Vec<u8>,
    /// 数据包大小
    data_size: u64,
    /// 文件专用, 起始点
    start_pos: Option<u64>,
    /// 文件专用, 结束点
    end_pos: Option<u64>,
}

impl Drop for InnerReceiver {
    fn drop(&mut self) {
        if self.receiver.is_some() {
            // println!("drop one receiver = {:?}", self.receiver);
        }
    }
}

impl InnerReceiver {
    pub fn new() -> Self {
        Self {
            receiver: None,
            file: None,
            cache_buf: vec![],
            data_size: u64::MAX,
            start_pos: None,
            end_pos: None
        }
    }

    pub fn new_receiver(receiver: Receiver<(bool, Binary)>) -> Self {
        let vec = vec![0u8; 4096];
        Self {
            receiver: Some(receiver),
            file: None,
            cache_buf: vec,
            data_size: u64::MAX,
            start_pos: None,
            end_pos: None
        }
    }
    
    pub fn new_file(file: File, data_size: u64) -> Self {
        let vec = vec![0u8; 4096];
        Self {
            receiver: None,
            file: Some(Box::new(file)),
            cache_buf: vec,
            data_size,
            start_pos: None,
            end_pos: None
        }
    }

    pub async fn set_start_end(&mut self, start_pos: u64, end_pos: u64) -> ProtResult<()> {
        assert!(end_pos >= start_pos, "结束位置必须大于起始位置");
        self.start_pos = Some(start_pos);
        self.end_pos = Some(end_pos);
        self.data_size = end_pos - start_pos;
        if let Some(f) = &mut self.file {
            f.as_mut().seek(std::io::SeekFrom::Start(start_pos)).await?;
        }
        Ok(())
    }

    pub fn is_none(&self) -> bool {
        self.receiver.is_none() && self.file.is_none()
    }

    pub async fn recv(&mut self) -> Option<(bool, Binary)> {
        if let Some(receiver) = &mut self.receiver {
            return receiver.recv().await;
        }

        if let Some(file) = &mut self.file {
            match file.read(&mut self.cache_buf).await {
                Ok(size) => {
                    let is_end = size < self.cache_buf.len() || self.data_size <= size as u64;
                    let read = std::cmp::min(self.data_size as usize, size);
                    self.data_size -= read as u64;
                    if is_end {
                        return Some((true, Binary::from(self.cache_buf[..read].to_vec())));
                    } else {
                        return Some((false, Binary::from(self.cache_buf[..read].to_vec())));
                    }
                }
                Err(_) => return None,
            };
        }
        None
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<(bool, Binary)>> {
        if let Some(receiver) = &mut self.receiver {
            return receiver.poll_recv(cx);
        }

        if let Some(file) = &mut self.file {
            let size = {
                let mut buf = ReadBuf::new(&mut self.cache_buf);
                match Pin::new(file).poll_read(cx, &mut buf) {
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(_)) => buf.filled().len(),
                    Poll::Ready(Err(e)) => { 
                        log::trace!("读取文件时出错:{:?}", e);
                        return Poll::Ready(None);
                    }
                    
                }
            };
            
            let is_end = size < self.cache_buf.len() || self.data_size <= size as u64;
            let read = std::cmp::min(self.data_size as usize, size);
            self.data_size -= read as u64;

            return Poll::Ready(Some((
                is_end,
                Binary::from(self.cache_buf[..read].to_vec()),
            )));
        }

        return Poll::Ready(None);
    }
}

struct InnerCompress {
    write_gz: Option<Box<GzEncoder<BinaryMut>>>,
    write_br: Option<Box<CompressorWriter<BinaryMut>>>,
    write_de: Option<Box<DeflateEncoder<BinaryMut>>>,
}

impl Debug for InnerCompress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InnerCompress")
            .field("write_gz", &self.write_gz)
            .field("write_de", &self.write_de)
            .finish()
    }
}

impl InnerCompress {
    pub fn new() -> Self {
        Self {
            write_gz: None,
            write_br: None,
            write_de: None,
        }
    }

    pub fn open_write_gz(&mut self) {
        if self.write_gz.is_none() {
            self.write_gz = Some(Box::new(GzEncoder::new(BinaryMut::new(), Compression::default())) );
        }
    }

    pub fn open_write_de(&mut self) {
        if self.write_de.is_none() {
            self.write_de = Some(Box::new(DeflateEncoder::new(
                BinaryMut::new(),
                Compression::default(),
            )));
        }
    }

    pub fn open_write_br(&mut self) {
        if self.write_br.is_none() {
            self.write_br = Some(Box::new(CompressorWriter::new(BinaryMut::new(), 4096, 11, 22)));
        }
    }
}


struct InnerDecompress {
    reader_gz: Option<Box<GzDecoder<BinaryMut>>>,
    reader_br: Option<Box<Decompressor<BinaryMut>>>,
    reader_de: Option<Box<DeflateDecoder<BinaryMut>>>,
}

impl Debug for InnerDecompress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InnerDecompress")
            .field("reader_gz", &self.reader_gz)
            .field("reader_de", &self.reader_de)
            .finish()
    }
}


impl InnerDecompress {
    pub fn new() -> Self {
        Self {
            reader_gz: None,
            reader_br: None,
            reader_de: None,
        }
    }

    pub fn open_reader_gz(&mut self) {
        if self.reader_gz.is_none() {
            self.reader_gz = Some(Box::new(GzDecoder::new(BinaryMut::new())));
        }
    }

    pub fn open_reader_de(&mut self) {
        if self.reader_de.is_none() {
            self.reader_de = Some(Box::new(DeflateDecoder::new(
                BinaryMut::new(),
            )));
        }
    }

    pub fn open_reader_br(&mut self) {
        if self.reader_br.is_none() {
            self.reader_br = Some(Box::new(Decompressor::new(BinaryMut::new(), 4096)));
        }
    }
}

pub struct Body {
    receiver: InnerReceiver,
    sem: PollSemaphore,
    permit: Option<OwnedSemaphorePermit>,
    origin_buf: Option<BinaryMut>,
    read_buf: Option<BinaryMut>,
    cache_body_data: BinaryMut,
    origin_compress_method: i8,
    now_compress_method: i8,
    compress: InnerCompress,
    decompress: InnerDecompress,
    is_chunked: bool,
    is_end: bool,
    is_process_end: bool,
    max_read_buf: usize,
    rate_limit: Option<RateLimitLayer>,
}

impl Default for Body {
    fn default() -> Self {
        Self {
            receiver: InnerReceiver::new(),
            sem: PollSemaphore::new(Arc::new(Semaphore::new(10))),
            permit: None,
            origin_buf: None,
            read_buf: Default::default(),
            cache_body_data: BinaryMut::new(),
            
            origin_compress_method: Consts::COMPRESS_METHOD_NONE,
            now_compress_method: Consts::COMPRESS_METHOD_NONE,
            compress: InnerCompress::new(),
            decompress: InnerDecompress::new(),
            is_chunked: false,
            is_end: true,
            is_process_end: false,

            // 为了数据安全, 防止一次性全部读到内存, 限定默认大小为10M
            max_read_buf: 10_485_760,
            rate_limit: None,
        }
    }
}

impl Body {
    pub fn empty() -> Body {
        Default::default()
    }

    pub fn print_debug(&self) {
        println!("receiver = {:?}", std::mem::size_of_val(&self.receiver));

        println!("file = {:?}", std::mem::size_of_val(&self.receiver.file));

        println!("sem = {:?}", std::mem::size_of_val(&self.sem));
        println!("permit = {:?}", std::mem::size_of_val(&self.permit));
        println!("origin_buf = {:?}", std::mem::size_of_val(&self.origin_buf));
        println!("read_buf = {:?}", std::mem::size_of_val(&self.read_buf));
        println!("cache_body_data = {:?}", std::mem::size_of_val(&self.cache_body_data));
        println!("origin_compress_method = {:?}", std::mem::size_of_val(&self.origin_compress_method));
        println!("compress = {:?}", std::mem::size_of_val(&self.compress));
        println!("decompress = {:?}", std::mem::size_of_val(&self.decompress));
        println!("is_chunked = {:?}", std::mem::size_of_val(&self.is_chunked));
        println!("rate_limit = {:?}", std::mem::size_of_val(&self.rate_limit));
    }

    pub fn only(binary: Binary) -> Body {
        Body {
            origin_buf: Some(BinaryMut::from(binary)),
            ..Default::default()
        }
    }
    
    pub fn new_binary(binary: BinaryMut) -> Body {
        Body {
            origin_buf: Some(binary),
            ..Default::default()
        }
    }

    pub fn new(receiver: Receiver<(bool, Binary)>, binary: BinaryMut, is_end: bool) -> Body {
        Body {
            receiver: InnerReceiver::new_receiver(receiver),
            origin_buf: Some(binary),
            is_end,
            ..Default::default()
        }
    }

    pub fn new_file(file: File, data_size: u64) -> Body {
        Body {
            receiver: InnerReceiver::new_file(file, data_size),
            is_end: false,
            ..Default::default()
        }
    }

    pub fn new_text(text: String) -> Self {
        Body {
            origin_buf: Some(BinaryMut::from(text)),
            ..Default::default()
        }
    }

    pub fn set_rate_limit(&mut self, rate: RateLimitLayer) {
        self.rate_limit = Some(rate);
    }

    pub fn set_max_read_buf(&mut self, max_read_buf: usize) {
        self.max_read_buf = max_read_buf;
    }
    
    pub async fn set_start_end(&mut self, start_pos: u64, end_pos: u64) -> ProtResult<()> {
        self.receiver.set_start_end(start_pos, end_pos).await
    }

    pub fn binary(&mut self) -> Binary {
        let mut buffer = BinaryMut::new();
        if let Some(bin) = self.read_buf.take() {
            buffer.put_slice(bin.chunk());
            
            self.notify_some_read();
        }
        buffer.freeze()
    }


    pub fn get_origin_compress(&self) -> i8 {
        self.origin_compress_method
    }

    pub fn get_now_compress(&self) -> i8 {
        // 输入输出同一种编码, 不做任何处理
        if self.origin_compress_method == self.now_compress_method {
            return 0;
        }
        self.now_compress_method
    }

    pub fn check_over_limit(&mut self) {
        if self.read_buf.is_some() && self.read_buf.as_ref().unwrap().remaining() >= self.max_read_buf {
            self.permit.take();
        }
    }

    pub fn notify_some_read(&mut self) {
        if self.permit.is_some() {
            return;
        }
        if self.sem.available_permits() == 0 {
            self.sem.add_permits(1);
        }
    }

    pub fn set_compress_gzip(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_GZIP;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_compress_deflate(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_DEFLATE;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_compress_brotli(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_BROTLI;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_compress_origin_gzip(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_GZIP;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_compress_origin_deflate(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_DEFLATE;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_compress_origin_brotli(&mut self) {
        self.origin_compress_method = Consts::COMPRESS_METHOD_BROTLI;
        self.now_compress_method = Consts::COMPRESS_METHOD_NONE;
    }

    pub fn set_origin_compress_method(&mut self, method: i8) -> i8 {
        self.origin_compress_method = method;
        self.origin_compress_method
    }

    pub fn add_compress_method(&mut self, method: i8) -> i8 {
        self.now_compress_method = method;
        self.get_now_compress()
    }

    pub fn is_chunked(&mut self) -> bool {
        self.is_chunked
    }

    pub fn set_chunked(&mut self, chunked: bool) {
        self.is_chunked = chunked;
    }

    pub fn cache_buffer(&mut self, buf: &[u8]) -> usize {
        if self.read_buf.is_none() {
            self.read_buf = Some(BinaryMut::new());
        }
        self.decode_read_data(buf).ok().unwrap_or(0)
    }

    pub fn is_end(&self) -> bool {
        self.is_end
    }

    pub fn set_end(&mut self, end: bool) {
        self.is_end = end
    }

    pub fn read_now(&mut self) -> Binary {
        let mut buffer = BinaryMut::new();
        let _ = self.process_data(None);
        if self.cache_body_data.remaining() > 0 {
            buffer.put_slice(&self.cache_body_data.chunk());
            self.cache_body_data.advance_all();
        }
        return buffer.freeze();
    }

    pub fn origin_len(&self) -> usize {
        let mut size = 0;
        if let Some(bin) = &self.read_buf {
            size += bin.remaining();
        }
        return size;
    }

    pub fn copy_now(&self) -> Binary {
        let mut buffer = BinaryMut::new();
        if let Some(bin) = &self.read_buf {
            buffer.put_slice(bin.chunk());
        }
        return buffer.freeze();
    }

    pub fn body_len(&mut self) -> usize {
        return self.cache_body_data.remaining();
    }

    pub async fn wait_all(&mut self) -> Option<usize> {
        let _ = self.process_data(None);
        let mut size = 0;
        if !self.is_end && !self.receiver.is_none() {
            while let Some(v) = self.receiver.recv().await {
                self.is_end = v.0;
                size += self.cache_buffer(v.1.chunk());
                if self.is_end == true {
                    break;
                }
            }
        }
        Some(size)
    }

    pub async fn read_all(&mut self, buffer: &mut BinaryMut) -> Option<usize> {
        let _ = self.process_data(None);

        if !self.is_end && !self.receiver.is_none() {
            while let Some(v) = self.receiver.recv().await {
                self.cache_buffer(v.1.chunk());
                self.is_end = v.0;
                if self.is_end == true {
                    break;
                }
            }
        }
        let _ = self.process_data(None);
        match self.read_data(buffer) {
            Ok(s) => Some(s),
            _ => None,
        }
    }

    fn inner_encode_write_data<B: Bt + BtMut>(
        buffer: &mut B,
        data: &[u8],
        is_chunked: bool,
    ) -> std::io::Result<usize> {
        if is_chunked {
            Helper::encode_chunk_data(buffer, data)
        } else {
            Ok(buffer.put_slice(data))
        }
    }

    fn encode_write_data(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self.get_now_compress() {
            Consts::COMPRESS_METHOD_GZIP => {
                // 数据结束，需要主动调用结束以导出全部结果
                if data.len() == 0 {
                    self.compress.open_write_gz();
                    let gz = self.compress.write_gz.take().unwrap();
                    let value = gz.finish().unwrap();
                    if value.remaining() > 0 {
                        Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &value,
                            self.is_chunked,
                        )?;
                    }
                    if self.is_chunked {
                        Helper::encode_chunk_data(&mut self.cache_body_data, data)
                    } else {
                        Ok(0)
                    }
                } else {
                    self.compress.open_write_gz();
                    let gz = self.compress.write_gz.as_mut().unwrap();
                    gz.write_all(data).unwrap();
                    // 每次写入，在尝试读取出数据
                    if gz.get_mut().remaining() > 0 {
                        let s = Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &gz.get_mut().chunk(),
                            self.is_chunked,
                        );
                        gz.get_mut().clear();
                        s
                    } else {
                        Ok(0)
                    }
                }
            }
            Consts::COMPRESS_METHOD_DEFLATE => {
                // 数据结束，需要主动调用结束以导出全部结果
                if data.len() == 0 {
                    self.compress.open_write_de();
                    let de = self.compress.write_de.take().unwrap();
                    let value = de.finish().unwrap();
                    if value.remaining() > 0 {
                        Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &value,
                            self.is_chunked,
                        )?;
                    }
                    if self.is_chunked {
                        Helper::encode_chunk_data(&mut self.cache_body_data, data)
                    } else {
                        Ok(0)
                    }
                } else {
                    self.compress.open_write_de();
                    let de = self.compress.write_de.as_mut().unwrap();
                    de.write_all(data).unwrap();
                    // 每次写入，在尝试读取出数据
                    if de.get_mut().remaining() > 0 {
                        let s = Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &de.get_mut().chunk(),
                            self.is_chunked,
                        );
                        de.get_mut().clear();
                        s
                    } else {
                        Ok(0)
                    }
                }
            }
            Consts::COMPRESS_METHOD_BROTLI => {
                // 数据结束，需要主动调用结束以导出全部结果
                if data.len() == 0 {
                    self.compress.open_write_br();
                    let mut de = self.compress.write_br.take().unwrap();
                    de.flush()?;
                    let value = de.into_inner();
                    if value.remaining() > 0 {
                        Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &value,
                            self.is_chunked,
                        )?;
                    }
                    if self.is_chunked {
                        Helper::encode_chunk_data(&mut self.cache_body_data, data)
                    } else {
                        Ok(0)
                    }
                } else {
                    self.compress.open_write_br();
                    let de = self.compress.write_br.as_mut().unwrap();
                    de.write_all(data).unwrap();
                    // 每次写入，在尝试读取出数据
                    if de.get_mut().remaining() > 0 {
                        let s = Self::inner_encode_write_data(
                            &mut self.cache_body_data,
                            &de.get_mut().chunk(),
                            self.is_chunked,
                        );
                        de.get_mut().clear();
                        s
                    } else {
                        Ok(0)
                    }
                }
            }
            _ => Self::inner_encode_write_data(&mut self.cache_body_data, data, self.is_chunked),
        }
    }

    pub fn poll_encode_write<B: Bt + BtMut>(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut B,
    ) -> Poll<webparse::WebResult<usize>> {
        ready!(self.process_data(Some(cx)))?;
        let s = self.read_data(buffer)?;
        Poll::Ready(Ok(s))
    }

    fn inner_poll_sem_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.permit.is_some() {
            return Poll::Ready(Ok(()))
        }
        match self.sem.poll_acquire(cx) {
            Poll::Pending => {
                log::trace!("数据超过了限制的大小,等待缓冲区的读取才能继续!");
                Poll::Pending
            },
            Poll::Ready(None) => unreachable!("who closed it?"),
            Poll::Ready(Some(x)) => {
                self.permit.replace(x);
                Poll::Ready(Ok(()))
            }
        }
    }

    fn inner_poll_read(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        if self.is_end {
            return Poll::Ready(Ok(false));
        }
        ready!(self.inner_poll_sem_ready(cx))?;
        let mut has_change = false;
        loop {
            if let Some(rate) = &mut self.rate_limit {
                match rate.poll_ready(cx) {
                    Poll::Pending => {
                        break;
                    }
                    Poll::Ready(_) => {}
                }
            }
            match self.receiver.poll_recv(cx) {
                Poll::Ready(Some((is_end, bin))) => {
                    self.is_end = is_end;
                    self.cache_buffer(&bin.chunk());
                    if let Some(rate) = &mut self.rate_limit {
                        rate.poll_call(bin.remaining() as u64)?;
                    }
                    has_change = true;
                    if self.is_end {
                        break;
                    }
                }
                Poll::Ready(None) => {
                    self.is_end = true;
                    has_change = true;
                    break;
                }
                Poll::Pending => break,
            }
        }
        if has_change {
            self.check_over_limit();
        }
        return Poll::Ready(Ok(has_change));
    }

    /// 返回true表示需要等待, 否则继续执行
    fn decode_read_data(&mut self, data: &[u8])  -> std::io::Result<usize> {
        if self.read_buf.is_none() {
            self.read_buf = Some(BinaryMut::new());
        }
        // 原始的压缩方式不为空, 表示数据可能需要处理
        if self.origin_compress_method != Consts::COMPRESS_METHOD_NONE {
            // 数据方式与原有的一模一样, 不做处理
            if self.origin_compress_method == self.now_compress_method {
                self.read_buf.as_mut().unwrap().put_slice(data);
                return Ok(0)
            }
            // 数据结束前不做解压缩操作, 后续也不可读
            let size = match self.origin_compress_method {
                Consts::COMPRESS_METHOD_GZIP => {
                    self.decompress.open_reader_gz();
                    let gz = self.decompress.reader_gz.as_mut().unwrap();
                    gz.write_all(data)?;
                    let s = read_all_data(self.read_buf.as_mut().unwrap(), gz)?;
                    s
                },
                Consts::COMPRESS_METHOD_DEFLATE => {
                    self.decompress.open_reader_de();
                    let de = self.decompress.reader_de.as_mut().unwrap();
                    let s = read_all_data(self.read_buf.as_mut().unwrap(), de)?;
                    s
                },
                Consts::COMPRESS_METHOD_BROTLI => {
                    self.decompress.open_reader_br();
                    let br = self.decompress.reader_br.as_mut().unwrap();
                    let s = read_all_data(self.read_buf.as_mut().unwrap(), br)?;
                    s
                },
                _ => {
                    return Err(Error::new(io::ErrorKind::Interrupted, "未知的压缩格式"));
                }
            };
            if self.is_end {
                self.origin_compress_method = Consts::COMPRESS_METHOD_NONE;
            }
            self.notify_some_read();
            return Ok(size)
        }
        self.read_buf.as_mut().unwrap().put_slice(data);
        Ok(data.len())
    }

    pub fn process_data(&mut self, cx: Option<&mut Context<'_>>) -> Poll<webparse::WebResult<usize> > {
        if self.is_process_end {
            return Poll::Ready(Ok(0));
        }

        if let Some(origin) = self.origin_buf.take() {
            let _ = self.decode_read_data(origin.chunk())?;
        }

        if let Some(cx) = cx {
            ready!(self.inner_poll_read(cx)?);
        }
        
        if let Some(mut bin) = self.read_buf.take() {
            if bin.chunk().len() > 0 {
                self.encode_write_data(bin.chunk())?;
            }
            bin.advance_all();
            self.read_buf = Some(bin);
            self.notify_some_read();
        }
        if self.is_end {
            self.encode_write_data(&[])?;
        }
        self.is_process_end = self.is_end;
        Poll::Ready(Ok(0))
    }

    pub fn read_data<B: Bt + BtMut>(
        &mut self,
        read_data: &mut B,
    ) -> WebResult<usize> {
        let _ = self.process_data(None)?;
        let mut size = 0;
        if self.cache_body_data.remaining() > 0 {
            size += read_data.put_slice(&self.cache_body_data.chunk());
            self.cache_body_data.advance_all();
        }
        Ok(size)
    }
}

impl AsyncRead for Body {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        
        ready!(self.process_data(Some(cx)).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process data error")))?;
        let len = std::cmp::min(self.cache_body_data.remaining(), buf.remaining());
        buf.put_slice(&self.cache_body_data.chunk()[..len]);
        self.cache_body_data.advance(len);
        return Poll::Ready(Ok(()));
    }
}

impl Serialize for Body {
    fn serialize<B: Bt + BtMut>(
        &mut self,
        buffer: &mut B,
    ) -> webparse::WebResult<usize> {
        let mut size = 0;
        if let Some(bin) = self.read_buf.take() {
            size += buffer.put_slice(bin.chunk());
            self.notify_some_read();
        }
        Ok(size)
    }
}

unsafe impl Sync for Body {}

unsafe impl Send for Body {}

impl From<()> for Body {
    fn from(_: ()) -> Self {
        Body::empty()
    }
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        let bin = BinaryMut::from(value.as_bytes().to_vec());
        Body::new_binary(bin)
    }
}

impl From<Binary> for Body {
    fn from(value: Binary) -> Self {
        Body::only(value)
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        let bin = BinaryMut::from(value.into_bytes().to_vec());
        Body::new_binary(bin)
    }
}

impl From<Vec<u8>> for Body {
    fn from(value: Vec<u8>) -> Self {
        let bin = BinaryMut::from(value);
        Body::new_binary(bin)
    }
}

impl From<Body> for Vec<u8> {
    fn from(mut value: Body) -> Self {
        let bin = value.read_now();
        bin.into_slice_all()
    }
}

impl From<Body> for String {
    fn from(mut value: Body) -> Self {
        let bin = value.read_now();
        let v = bin.into_slice_all();
        String::from_utf8_lossy(&v).to_string()
    }
}

impl Display for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_end {
            let bin = self.copy_now();
            f.write_str(&String::from_utf8_lossy(bin.chunk()))
        } else {
            let mut f = f.debug_struct("RecvStream");
            f.field("状态", &self.is_end);
            if self.is_end {
                f.field("接收字节数", &self.cache_body_data.remaining());
            }
            f.finish()
        }
    }
}

impl Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self))
    }
}
