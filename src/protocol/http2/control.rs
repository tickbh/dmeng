use std::{
    collections::{HashMap, LinkedList},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use futures_core::{ready, Stream};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc::Sender,
};
use webparse::{
    http::{
        http2::frame::{Frame, GoAway, Reason, Settings, StreamIdentifier},
    },
    Binary, Request, Response, http2::frame::WindowUpdate,
};

use crate::{ProtResult, RecvStream, ProtError};

use super::{
    codec::Codec, inner_stream::InnerStream, send_response::SendControl, state::StateHandshake,
    PriorityQueue, SendResponse, StateGoAway, StatePingPong, StateSettings, SendRequest, 
};

use webparse::http2::DEFAULT_INITIAL_WINDOW_SIZE;
use webparse::http2::WindowSize;

#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub next_stream_id: StreamIdentifier,
    pub initial_max_send_streams: usize,
    pub max_send_buffer_size: usize,
    pub reset_stream_duration: Duration,
    pub reset_stream_max: usize,
    pub remote_reset_stream_max: usize,
    pub settings: Settings,
}

impl ControlConfig {
    pub fn apply_remote_settings(&mut self, settings: &Settings) {
        self.settings = settings.clone();
    }

    pub fn get_initial_window_size(&self) -> WindowSize {
        self.settings.initial_window_size().unwrap_or(DEFAULT_INITIAL_WINDOW_SIZE)
    }
}

pub struct Control {
    /// 所有收到的帧, 如果收到Header结束就开始返回request, 后续收到Data再继续返回直至结束,
    /// id为0的帧为控制帧, 需要立即做处理
    recv_frames: HashMap<StreamIdentifier, InnerStream>,

    ready_queue: LinkedList<StreamIdentifier>,
    last_stream_id: StreamIdentifier,
    send_frames: PriorityQueue,
    response_queue: Arc<Mutex<Vec<SendResponse>>>,
    request_queue: Vec<SendRequest>,

    handshake: StateHandshake,
    setting: StateSettings,
    goaway: StateGoAway,
    ping_pong: StatePingPong,

    pub error: Option<GoAway>,

    config: ControlConfig,
    
    sender_push: Sender<(StreamIdentifier, Response<RecvStream>)>,

}

impl Control {
    pub fn new(config: ControlConfig, sender_push: Sender<(StreamIdentifier, Response<RecvStream>)>) -> Self {
        Control {
            recv_frames: HashMap::new(),
            send_frames: PriorityQueue::new(config.get_initial_window_size()),
            ready_queue: LinkedList::new(),
            response_queue: Arc::new(Mutex::new(Vec::new())),
            request_queue: Vec::new(),
            setting: StateSettings::new(config.settings.clone()),
            handshake: StateHandshake::new_server(),
            goaway: StateGoAway::new(),
            ping_pong: StatePingPong::new(),
            last_stream_id: StreamIdentifier::zero(),
            error: None,
            config,
            sender_push,
        }
    }

    pub fn encode_response(&mut self, cx: &mut Context) -> ProtResult<()>
    {
        let mut list = self.response_queue.lock().unwrap();
        let vals = (*list).drain(..).collect::<Vec<SendResponse>>();
        for mut l in vals {
            let (isend, vec) = l.encode_frames(cx);
            self.send_frames.send_frames(l.stream_id, vec)?;
            if !isend {
                (*list).push(l);
            }
        }
        Ok(())
    }


    pub fn encode_request(&mut self, cx: &mut Context) -> ProtResult<()>
    {
        if self.request_queue.is_empty() {
            return Ok(());
        }
        let vals = self.request_queue.drain(..).collect::<Vec<SendRequest>>();
        for mut l in vals {
            let (isend, vec) = l.encode_frames(cx);

            for v in &vec {
                println!("vec = {:?}", v);
            }
            self.send_frames.send_frames(l.stream_id, vec)?;
            if !isend {
                self.request_queue.push(l);
            }
        }
        Ok(())
    }

    pub fn next_stream_id(&mut self) -> StreamIdentifier {
        self.config.next_stream_id.next_id()
    }

    pub fn poll_write<T>(&mut self, cx: &mut Context, codec: &mut Codec<T>, is_wait: bool) -> Poll<ProtResult<()>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        // 等待接收中，不能写入新消息
        self.encode_response(cx)?;
        self.encode_request(cx)?;
        if let Some(reason) = ready!(self.goaway.poll_handle(cx, codec)?) {
            return Poll::Ready(Err(ProtError::library_go_away(reason)));
        };
        ready!(self.ping_pong.poll_handle(cx, codec))?;
        match ready!(self.send_frames.poll_handle(cx, codec)) {
            Some(Err(e)) => return Poll::Ready(Err(e)),
            _ => (),
        }
        ready!(codec.poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }

    pub fn poll_request<T>(
        &mut self,
        cx: &mut Context<'_>,
        codec: &mut Codec<T>,
    ) -> Poll<Option<ProtResult<Request<RecvStream>>>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        ready!(self.handshake.poll_handle(cx, codec))?;
        loop {
            let is_wait = ready!(self.setting.poll_handle(cx, codec, &mut self.config))?;
            // 写入如果pending不直接pending, 等尝试读pending则返回
            match self.poll_write(cx, codec, is_wait) {
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                _ => (),
            }

            match Pin::new(&mut *codec).poll_next(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    match &frame {
                        Frame::Settings(settings) => {
                            self.setting
                                .recv_setting(codec, settings.clone(), &mut self.config)?;
                        }
                        Frame::Data(_) => {
                            let _ = self.recv_frame(frame)?;
                        }
                        Frame::Headers(_) => {
                            let _ = self.recv_frame(frame)?;
                        }
                        Frame::Priority(v) => {
                            self.send_frames.priority_recv(v.clone());
                        }
                        Frame::PushPromise(_) => {}
                        Frame::Ping(p) => {
                            self.ping_pong.receive(p.clone());
                        }
                        Frame::GoAway(e) => {
                            self.error = Some(e.clone());
                        },
                        Frame::WindowUpdate(_v) => {
                            // self.config.settings.set_initial_window_size(Some(v.size_increment()))
                        }
                        Frame::Reset(_v) => {}
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => match ready!(self.build_request_frame()?) {
                    Some(r) => {
                        return Poll::Ready(Some(Ok(r)));
                    }
                    None => {
                        if let Some(e) = &self.error {
                            return Poll::Ready(Some(Err(ProtError::library_go_away(e.reason()))));
                        } else {
                            return Poll::Pending
                        }
                    },
                },
            }
        }
    }


    pub fn poll_response<T>(
        &mut self,
        cx: &mut Context<'_>,
        codec: &mut Codec<T>,
    ) -> Poll<Option<ProtResult<Response<RecvStream>>>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        ready!(self.handshake.poll_handle(cx, codec))?;
        loop {
            let is_wait = ready!(self.setting.poll_handle(cx, codec, &mut self.config))?;
            println!("is wait = {}", is_wait);
            // 写入如果pending不直接pending, 等尝试读pending则返回
            match self.poll_write(cx, codec, is_wait) {
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                _ => (),
            }
            match Pin::new(&mut *codec).poll_next(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    match &frame {
                        Frame::Settings(settings) => {
                            let finish = self.setting
                                .recv_setting(codec, settings.clone(), &mut self.config)?;
                            // if finish {
                            //     codec.send_frame(Frame::WindowUpdate(WindowUpdate::new(0.into(), 33333)))?;
                            // }
                        }
                        Frame::Data(_) => {
                            let _ = self.recv_frame(frame)?;
                        }
                        Frame::Headers(_) => {
                            let _ = self.recv_frame(frame)?;
                        }
                        Frame::Priority(v) => {
                            self.send_frames.priority_recv(v.clone());
                        }
                        Frame::PushPromise(_) => {}
                        Frame::Ping(p) => {
                            self.ping_pong.receive(p.clone());
                        }
                        Frame::GoAway(e) => {
                            self.error = Some(e.clone());
                        },
                        Frame::WindowUpdate(_v) => {
                            // self.config.settings.set_initial_window_size(Some(v.size_increment()))
                        }
                        Frame::Reset(_v) => {}
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => match ready!(self.build_response_frame()?) {
                    Some(r) => {
                        return Poll::Ready(Some(Ok(r)));
                    }
                    None => {
                        if let Some(e) = &self.error {
                            return Poll::Ready(Some(Err(ProtError::library_go_away(e.reason()))));
                        } else {
                            return Poll::Pending
                        }
                    },
                },
            }
        }
    }

    pub fn build_request(
        &mut self,
        _frames: &Vec<Frame<Binary>>,
    ) -> Option<ProtResult<Request<Binary>>> {
        None
    }

    pub fn build_request_frame(&mut self) -> Poll<Option<ProtResult<Request<RecvStream>>>> {
        if self.ready_queue.is_empty() {
            return Poll::Ready(None);
        }
        let stream_id = self.ready_queue.pop_front().unwrap();
        match self
            .recv_frames
            .get_mut(&stream_id)
            .unwrap()
            .build_request()
        {
            Err(e) => return Poll::Ready(Some(Err(e))),
            Ok(mut r) => {
                let method = r.method().clone();
                r.extensions_mut().insert(stream_id);
                r.extensions_mut().insert(SendControl::new(
                    stream_id,
                    self.sender_push.clone(),
                    method,
                ));
                Poll::Ready(Some(Ok(r)))
            }
        }
    }

    pub fn build_response_frame(&mut self) -> Poll<Option<ProtResult<Response<RecvStream>>>> {
        if self.ready_queue.is_empty() {
            return Poll::Ready(None);
        }
        let stream_id = self.ready_queue.pop_front().unwrap();
        match self
            .recv_frames
            .get_mut(&stream_id)
            .unwrap()
            .build_response()
        {
            Err(e) => return Poll::Ready(Some(Err(e))),
            Ok(mut r) => {
                // let method = r.method().clone();
                r.extensions_mut().insert(stream_id);
                r.extensions_mut().insert(SendControl::new(
                    stream_id,
                    self.sender_push.clone(),
                    webparse::Method::Get,
                ));
                Poll::Ready(Some(Ok(r)))
            }
        }
    }

    pub fn recv_frame(&mut self, frame: Frame<Binary>) -> Poll<Option<ProtResult<bool>>> {
        let stream_id = frame.stream_id();
        if stream_id.is_zero() {
            return Poll::Ready(None);
        }

        let is_end_headers = frame.is_end_headers();
        let _is_end_stream = frame.is_end_stream();

        if !self.recv_frames.contains_key(&stream_id) {
            self.recv_frames.insert(stream_id, InnerStream::new(frame));
        } else {
            self.recv_frames.get_mut(&stream_id).unwrap().push(frame)?;
        }

        self.last_stream_id = self.last_stream_id.max(stream_id);

        if is_end_headers {
            self.ready_queue.push_back(stream_id);
            Poll::Ready(Some(Ok(true)))
        } else {
            Poll::Ready(None)
        }
    }

    pub fn go_away_now(&mut self, e: Reason) {
        let frame = GoAway::new(self.last_stream_id, e);
        self.goaway.go_away_now(frame);
    }

    pub fn go_away_now_data(&mut self, e: Reason, data: Binary) {
        let frame = GoAway::with_debug_data(self.last_stream_id, e, data);
        self.goaway.go_away_now(frame);
    }

    pub fn last_goaway_reason(&mut self) -> &Reason {
        self.goaway.reason()
    }

    pub fn set_handshake_status(&mut self, binary: Binary, is_client: bool) {
        self.handshake.set_handshake_status(binary, is_client)
    }

    pub fn set_setting_status(&mut self, setting: Settings, is_done: bool) {
        self.setting.set_settings(setting, is_done);
    }

    pub fn set_setting_done(&mut self) {
        self.setting.set_settings_done();
    }

    pub async fn send_response(&mut self, res: Response<RecvStream>, stream_id: StreamIdentifier) -> ProtResult<()>
    {
        self.send_response_may_push(res, stream_id, None).await
    }

    pub async fn send_response_may_push(&mut self, res: Response<RecvStream>, stream_id: StreamIdentifier, push: Option<StreamIdentifier>) -> ProtResult<()>
    {
        let mut data = self.response_queue.lock().unwrap();
        let is_end = res.body().is_end();
        let response = SendResponse::new(
            stream_id,
            push,
            res,
            webparse::Method::Get,
            is_end,
        );
        data.push(response);
        Ok(())
    }


    pub async fn send_request(&mut self, req: Request<RecvStream>) -> ProtResult<()>
    {
        let is_end = req.body().is_end();
        let next_id = self.next_stream_id();
        self.request_queue.push(SendRequest::new(next_id, req, is_end));
        Ok(())
    }
    
}
