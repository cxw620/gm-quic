use std::task::{ready, Context, Poll};

use qbase::{
    error::{Error as QuicError, ErrorKind},
    frame::{
        BeFrame, FrameType, MaxStreamDataFrame, ReceiveFrame, ResetStreamFrame, SendFrame,
        StopSendingFrame, StreamCtlFrame, StreamFrame, STREAM_FRAME_MAX_ENCODING_SIZE,
    },
    param::Parameters,
    sid::{
        handy::ConsistentConcurrency,
        remote_sid::{AcceptSid, ExceedLimitError},
        Dir, Role, StreamId, StreamIds,
    },
    varint::VarInt,
};

use super::{
    io::{ArcInput, ArcOutput, IOState},
    listener::{AcceptBiStream, AcceptUniStream, ArcListener},
    Ext,
};
use crate::{
    recv::{self, ArcRecver, Incoming, Reader},
    send::{ArcSender, Outgoing, Writer},
};

/// Manage all streams in the connection, send and receive frames, handle frame loss, and acknowledge.
///
/// The struct dont truly send and receive frames, this struct provides interfaces to generate frames
/// will be sent to the peer, receive frames, handle frame loss, and acknowledge.
///
/// [`Outgoing`], [`Incoming`] , [`Writer`] and [`Reader`] dont truly send and receive frames, too.
///
/// # Send frames
///
/// ## Stream frame
///
/// When the application wants to send data to the peer, it will call [`write`] method on [`Writer`]
/// to write data to the [`SendBuf`].
///
/// Protocol layer will call [`try_read_data`] to read data from the streams into stream frames and
/// write the frame into the quic packet.
///
/// ## Stream control frame
///
/// Be different from the stream frame, the stream control frame is much samller in size.
///
/// The struct has a generic type `T`, which must implement the [`SendFrame`] trait. The trait has
/// a method [`send_frame`], which will be called to send the stream control frame to the peer, see
/// [`SendFrame`] for more details.
///
/// # Receive frames, handle frame loss and acknowledge
///
/// Frames received, frames lost or acknowledgmented will be delivered to the corresponding method.
/// | method on [`DataStreams`]                             | corresponding method               |
/// | -------------------------------------------------------- | ---------------------------------- |
/// | [`recv_data`]                                            | [`Incoming::recv_data`]            |
/// | [`recv_stream_control`] ([`RESET_STREAM frame`])         | [`Incoming::recv_reset`]           |
/// | [`recv_stream_control`] ([`STOP_SENDING frame`])         | [`Outgoing::stop`]                 |
/// | [`recv_stream_control`] ([`MAX_STREAM_DATA frame`])      | [`Outgoing::update_window`]        |
/// | [`recv_stream_control`] ([`MAX_STREAMS frame`])          | [`ArcLocalStreamIds::recv_max_streams_frame`] |
/// | [`recv_stream_control`] ([`STREAM_DATA_BLOCKED frame`])  | none(the frame will be ignored)    |
/// | [`recv_stream_control`] ([`STREAMS_BLOCKED frame`])      | none(the frame will be ignored)    |
/// | [`on_data_acked`]                                        | [`Outgoing::on_data_acked`]        |
/// | [`may_loss_data`]                                        | [`Outgoing::may_loss_data`]        |
/// | [`on_reset_acked`]                                       | [`Outgoing::on_reset_acked`]       |
///
/// # Create and accept streams
///
/// Stream frames and stream control frames have the function of creating flows. If a steam frame is
/// received but the corresponding stream has not been created, a stream will be created passively.
///
/// [`AcceptBiStream`] and [`AcceptUniStream`] are provided to the application layer to `accept` a
/// stream (obtain a passively created stream). These future will be resolved when a stream is created
/// by peer.
///
/// Alternatively, sending a stream frame or a stream control frame will create a stream actively.
/// [`OpenBiStream`] and [`OpenUniStream`] are provided to the application layer to `open` a stream.
/// These future will be resolved when the connection established.
///
/// [`write`]: tokio::io::AsyncWriteExt::write
/// [`SendBuf`]: crate::send::SendBuf
/// [`send_frame`]: SendFrame::send_frame
/// [`try_read_data`]: DataStreams::try_read_data
/// [`recv_data`]: DataStreams::recv_data
/// [`recv_stream_control`]: DataStreams::recv_stream_control
/// [`on_data_acked`]: DataStreams::on_data_acked
/// [`may_loss_data`]: DataStreams::may_loss_data
/// [`on_reset_acked`]: DataStreams::on_reset_acked
/// [`RESET_STREAM frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-reset_stream-frame
/// [`STOP_SENDING frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-stop_sending-frames
/// [`MAX_STREAM_DATA frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-max_stream_data-frame
/// [`MAX_STREAMS frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-max_streams-frame
/// [`STREAM_DATA_BLOCKED frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-stream_data_blocked-frame
/// [`STREAMS_BLOCKED frame`]: https://www.rfc-editor.org/rfc/rfc9000.html#name-streams_blocked-frame
/// [`OpenBiStream`]: crate::streams::OpenBiStream
/// [`OpenUniStream`]: crate::streams::OpenUniStream
/// [`ArcLocalStreamIds::recv_max_streams_frame`]: qbase::sid::ArcLocalStreamIds::recv_max_streams_frame
///
#[derive(Debug)]
pub struct DataStreams<TX>
where
    TX: SendFrame<StreamCtlFrame> + Clone + Send + 'static,
{
    // 该queue与space中的transmitter中的frame_queue共享，为了方便向transmitter中写入帧
    ctrl_frames: TX,

    role: Role,
    stream_ids: StreamIds<Ext<TX>, Ext<TX>>,
    // the receive buffer size for the accpeted unidirectional stream created by peer
    uni_stream_rcvbuf_size: u64,
    // the receive buffer size of the bidirectional stream actively created by local
    local_bi_stream_rcvbuf_size: u64,
    // the receive buffer size for the accpeted bidirectional stream created by peer
    remote_bi_stream_rcvbuf_size: u64,
    // 所有流的待写端，要发送数据，就得向这些流索取
    output: ArcOutput<Ext<TX>>,
    // 所有流的待读端，收到了数据，交付给这些流
    input: ArcInput,
    // 对方主动创建的流
    listener: ArcListener<Ext<TX>>,
}

fn wrapper_error(fty: FrameType) -> impl FnOnce(ExceedLimitError) -> QuicError {
    move |e| QuicError::new(ErrorKind::StreamLimit, fty, e.to_string())
}

impl<TX> DataStreams<TX>
where
    TX: SendFrame<StreamCtlFrame> + Clone + Send + 'static,
{
    /// Try to read data from streams into stream frames and write the stream frame into the `buf`.
    ///
    /// # Fairness
    ///
    /// It's fair between streams. We have implemented a token bucket algorithm, and the [`try_read_data`]
    /// method will read the data of each stream sequentially. Starting from the first stream, when
    /// a stream exhausts its tokens (default is 4096, depending on the priority of the stream), or
    /// there is no data to send, the method will move to the next stream, and so on.
    ///
    /// # Flow control
    ///
    /// QUIC employs a limit-based flow control scheme where a receiver advertises the limit of total
    /// bytes it is prepared to receive on a given stream or for the entire connection. This leads to
    /// two levels of data flow control in QUIC, stream level and connection level.
    ///
    /// Stream-level flow control had limited by the [`write`] calls on [`Writer`], if the application
    /// wants to write more data than the stream's flow control limit , the [`write`] call will be
    /// blocked until the sending window is updated.
    ///
    /// For connection-level flow control, it's limited by the parameter `flow_limit` of this method.
    /// The amount of new data(never sent) will be read from the stream is less or equal to `flow_limit`.
    ///
    /// # Returns
    ///
    /// If no data written to the buffer, the method will return [`None`], or a tuple will be
    /// returned:
    ///
    /// * [`StreamFrame`]: The stream frame to be sent.
    /// * [`usize`]: The number of bytes written to the buffer.
    /// * [`usize`]: The number of new data writen to the buffer.
    ///
    /// [`try_read_data`]: DataStreams::try_read_data
    /// [`write`]: tokio::io::AsyncWriteExt::write
    pub fn try_read_data(
        &self,
        buf: &mut [u8],
        flow_limit: usize,
    ) -> Option<(StreamFrame, usize, usize)> {
        // todo: use core::range instead in rust 2024
        use core::ops::Bound::*;

        if buf.len() < STREAM_FRAME_MAX_ENCODING_SIZE + 1 {
            return None;
        }
        let mut guard = self.output.0.lock().unwrap();
        let output = guard.as_mut().ok()?;

        // 该tokens是令牌桶算法的token，为了多条Stream的公平性，给每个流定期地发放tokens，不累积
        // 各流轮流按令牌桶算法发放的tokens来整理数据去发送
        const DEFAULT_TOKENS: usize = 4096;
        let streams: &mut dyn Iterator<Item = _> = match &output.last_sent_stream {
            // [sid+1..] + [..=sid]
            Some((sid, tokens)) if *tokens == 0 => &mut output
                .outgoings
                .range((Excluded(sid), Unbounded))
                .chain(output.outgoings.range(..=sid))
                .map(|(sid, outgoing)| (*sid, outgoing, DEFAULT_TOKENS)),
            // [sid] + [sid+1..] + [..sid]
            Some((sid, tokens)) => &mut Option::into_iter(
                output
                    .outgoings
                    .get(sid)
                    .map(|outgoing| (*sid, outgoing, *tokens)),
            )
            .chain(
                output
                    .outgoings
                    .range((Excluded(sid), Unbounded))
                    .chain(output.outgoings.range(..sid))
                    .map(|(sid, outgoing)| (*sid, outgoing, DEFAULT_TOKENS)),
            ),
            // [..]
            None => &mut output
                .outgoings
                .range(..)
                .map(|(sid, outgoing)| (*sid, outgoing, DEFAULT_TOKENS)),
        };
        for (sid, (outgoing, _s), tokens) in streams.into_iter() {
            if let Some((frame, data_len, is_fresh, written)) =
                outgoing.try_read(sid, buf, tokens, flow_limit)
            {
                output.last_sent_stream = Some((sid, tokens - data_len));
                return Some((frame, written, if is_fresh { data_len } else { 0 }));
            }
        }
        None
    }

    /// Called when the stream frame acked.
    ///
    /// Actually calls the [`Outgoing::on_data_acked`] method of the corresponding stream.
    pub fn on_data_acked(&self, frame: StreamFrame) {
        if let Ok(set) = self.output.0.lock().unwrap().as_mut() {
            let mut is_all_rcvd = false;
            if let Some((o, s)) = set.get(&frame.id) {
                is_all_rcvd = o.on_data_acked(&frame.range(), frame.is_fin());
                if is_all_rcvd {
                    s.shutdown_send();
                    if s.is_terminated() {
                        self.stream_ids.remote.on_end_of_stream(frame.id);
                    }
                }
            }

            if is_all_rcvd {
                set.remove(&frame.id);
            }
        }
    }

    /// Called when the stream frame may lost.
    ///
    /// Actually calls the [`Outgoing::may_loss_data`] method of the corresponding stream.
    pub fn may_loss_data(&self, stream_frame: &StreamFrame) {
        if let Some((o, _s)) = self
            .output
            .0
            .lock()
            .unwrap()
            .as_mut()
            .ok()
            .and_then(|set| set.get(&stream_frame.id))
        {
            o.may_loss_data(&stream_frame.range());
        }
    }

    /// Called when the stream reset frame acked.
    ///
    /// Actually calls the [`Outgoing::on_reset_acked`] method of the corresponding stream.
    pub fn on_reset_acked(&self, reset_frame: ResetStreamFrame) {
        if let Ok(set) = self.output.0.lock().unwrap().as_mut() {
            if let Some((o, s)) = set.remove(&reset_frame.stream_id) {
                o.on_reset_acked();
                s.shutdown_send();
                if s.is_terminated() {
                    self.stream_ids
                        .remote
                        .on_end_of_stream(reset_frame.stream_id);
                }
            }
            // 如果流是双向的，接收部分的流独立地管理结束。其实是上层应用决定接收的部分是否同时结束
        }
    }

    /// Called when a stream frame which from peer is received by local.
    ///
    /// If the correspoding stream is not exist, `accept` the stream.
    ///
    /// Actually calls the [`Incoming::recv_data`] method of the corresponding stream.
    pub fn recv_data(
        &self,
        (stream_frame, body): &(StreamFrame, bytes::Bytes),
    ) -> Result<usize, QuicError> {
        let sid = stream_frame.id;
        // 对方必须是发送端，才能发送此帧
        if sid.role() != self.role {
            // 对方的sid，看是否跳跃，把跳跃的流给创建好
            self.try_accept_sid(sid)
                .map_err(wrapper_error(stream_frame.frame_type()))?;
        } else {
            // 我方的sid，那必须是双向流才能收到对方的数据，否则就是错误
            if sid.dir() == Dir::Uni {
                return Err(QuicError::new(
                    ErrorKind::StreamState,
                    stream_frame.frame_type(),
                    format!("local {sid} cannot receive STREAM_FRAME"),
                ));
            }
        }
        let ret = self
            .input
            .0
            .lock()
            .unwrap()
            .as_mut()
            .ok()
            .and_then(|set| set.get(&sid))
            .map(|(incoming, _s)| incoming.recv_data(stream_frame, body.clone()));
        // TODO: 此处应该返回是否接收完，代表着接收结束，可以将该流的接收状态标识为关闭

        match ret {
            Some(recv_ret) => recv_ret,
            // 该流已结束，收到的数据将被忽略
            None => Ok(0),
        }
    }

    /// Called when a stream control frame which from peer is received by local.
    ///
    /// If the correspoding stream is not exist, `accept` the stream first.
    ///
    /// Actually calls the corresponding method of the corresponding stream for the corresponding frame type.
    pub fn recv_stream_control(&self, stream_ctl_frame: &StreamCtlFrame) -> Result<(), QuicError> {
        match stream_ctl_frame {
            StreamCtlFrame::ResetStream(reset) => {
                let sid = reset.stream_id;
                // 对方必须是发送端，才能发送此帧
                if sid.role() != self.role {
                    self.try_accept_sid(sid)
                        .map_err(wrapper_error(reset.frame_type()))?;
                } else {
                    // 我方创建的流必须是双向流，对方才能发送ResetStream,否则就是错误
                    if sid.dir() == Dir::Uni {
                        return Err(QuicError::new(
                            ErrorKind::StreamState,
                            reset.frame_type(),
                            format!("local {sid} cannot receive RESET_STREAM frame"),
                        ));
                    }
                }
                if let Ok(set) = self.input.0.lock().unwrap().as_mut() {
                    if let Some((incoming, s)) = set.remove(&sid) {
                        incoming.recv_reset(reset)?;
                        s.shutdown_receive();
                        if s.is_terminated() {
                            self.stream_ids.remote.on_end_of_stream(reset.stream_id);
                        }
                    }
                }
            }
            StreamCtlFrame::StopSending(stop_sending) => {
                let sid = stop_sending.stream_id;
                // 对方必须是接收端，才能发送此帧
                if sid.role() != self.role {
                    // 对方创建的单向流，接收端是我方，不可能收到对方的StopSendingFrame
                    if sid.dir() == Dir::Uni {
                        return Err(QuicError::new(
                            ErrorKind::StreamState,
                            stop_sending.frame_type(),
                            format!("remote {sid} must not send STOP_SENDING_FRAME"),
                        ));
                    }
                    self.try_accept_sid(sid)
                        .map_err(wrapper_error(stop_sending.frame_type()))?;
                }
                if self
                    .output
                    .0
                    .lock()
                    .unwrap()
                    .as_mut()
                    .ok()
                    .and_then(|set| set.get(&sid))
                    .map(|(outgoing, _s)| outgoing.stop(stop_sending.app_err_code.into()))
                    .unwrap_or(false)
                {
                    self.ctrl_frames
                        .send_frame([StreamCtlFrame::ResetStream(ResetStreamFrame {
                            stream_id: sid,
                            app_error_code: VarInt::from_u32(0),
                            final_size: VarInt::from_u32(0),
                        })]);
                }
            }
            StreamCtlFrame::MaxStreamData(max_stream_data) => {
                let sid = max_stream_data.stream_id;
                // 对方必须是接收端，才能发送此帧
                if sid.role() != self.role {
                    // 对方创建的单向流，接收端是我方，不可能收到对方的MaxStreamData
                    if sid.dir() == Dir::Uni {
                        return Err(QuicError::new(
                            ErrorKind::StreamState,
                            max_stream_data.frame_type(),
                            format!("remote {sid} must not send MAX_STREAM_DATA_FRAME"),
                        ));
                    }
                    self.try_accept_sid(sid)
                        .map_err(wrapper_error(max_stream_data.frame_type()))?;
                }
                if let Some((outgoing, _s)) = self
                    .output
                    .0
                    .lock()
                    .unwrap()
                    .as_ref()
                    .ok()
                    .and_then(|set| set.get(&sid))
                {
                    outgoing.update_window(max_stream_data.max_stream_data.into_inner());
                }
            }
            StreamCtlFrame::StreamDataBlocked(stream_data_blocked) => {
                let sid = stream_data_blocked.stream_id;
                // 对方必须是发送端，才能发送此帧
                if sid.role() != self.role {
                    self.try_accept_sid(sid)
                        .map_err(wrapper_error(stream_data_blocked.frame_type()))?;
                } else {
                    // 我方创建的，必须是双向流，对方才是发送端，才能发出StreamDataBlocked；否则就是错误
                    if sid.dir() == Dir::Uni {
                        return Err(QuicError::new(
                            ErrorKind::StreamState,
                            stream_data_blocked.frame_type(),
                            format!("local {sid} cannot receive STREAM_DATA_BLOCKED_FRAME"),
                        ));
                    }
                }
                // 仅仅起到通知作用?主动更新窗口的，此帧没多大用，或许要进一步放大缓冲区大小；被动更新窗口的，此帧有用
            }
            StreamCtlFrame::MaxStreams(max_streams) => {
                // 主要更新我方能创建的单双向流
                _ = self.stream_ids.local.recv_frame(max_streams);
            }
            StreamCtlFrame::StreamsBlocked(streams_blocked) => {
                // 在某些流并发策略中，收到此帧，可能会更新MaxStreams
                _ = self.stream_ids.remote.recv_frame(streams_blocked);
            }
        }
        Ok(())
    }

    /// Called when a connection error occured.
    ///
    /// After the method called, read on [`Reader`] or write on [`Writer`] will return an error,
    /// the resouces will be released.
    pub fn on_conn_error(&self, err: &QuicError) {
        let mut output = match self.output.guard() {
            Ok(out) => out,
            Err(_) => return,
        };
        let mut input = match self.input.guard() {
            Ok(input) => input,
            Err(_) => return,
        };
        let mut listener = match self.listener.guard() {
            Ok(listener) => listener,
            Err(_) => return,
        };

        output.on_conn_error(err);
        input.on_conn_error(err);
        listener.on_conn_error(err);
    }
}

impl<TX> DataStreams<TX>
where
    TX: SendFrame<StreamCtlFrame> + Clone + Send + 'static,
{
    pub(super) fn new(role: Role, local_params: &Parameters, ctrl_frames: TX) -> Self {
        let max_bi_streams = local_params.initial_max_streams_bidi().into();
        let max_uni_streams = local_params.initial_max_streams_uni().into();
        Self {
            role,
            stream_ids: StreamIds::new(
                role,
                max_bi_streams,
                max_uni_streams,
                Ext(ctrl_frames.clone()),
                Box::new(ConsistentConcurrency::new(max_bi_streams, max_uni_streams)),
            ),
            uni_stream_rcvbuf_size: local_params.initial_max_stream_data_uni().into(),
            local_bi_stream_rcvbuf_size: local_params.initial_max_stream_data_bidi_local().into(),
            remote_bi_stream_rcvbuf_size: local_params.initial_max_stream_data_bidi_remote().into(),
            output: ArcOutput::new(),
            input: ArcInput::default(),
            listener: ArcListener::new(),
            ctrl_frames,
        }
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn poll_open_bi_stream(
        &self,
        cx: &mut Context<'_>,
        snd_wnd_size: u64,
    ) -> Poll<Result<Option<(StreamId, (Reader, Writer<Ext<TX>>))>, QuicError>> {
        let mut output = match self.output.guard() {
            Ok(out) => out,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let mut input = match self.input.guard() {
            Ok(input) => input,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if let Some(sid) = ready!(self.stream_ids.local.poll_alloc_sid(cx, Dir::Bi)) {
            let arc_sender = self.create_sender(sid, snd_wnd_size);
            let arc_recver = self.create_recver(sid, self.local_bi_stream_rcvbuf_size);
            let io_state = IOState::bidirection();
            output.insert(sid, Outgoing(arc_sender.clone()), io_state.clone());
            input.insert(sid, Incoming(arc_recver.clone()), io_state);
            Poll::Ready(Ok(Some((sid, (Reader(arc_recver), Writer(arc_sender))))))
        } else {
            Poll::Ready(Ok(None))
        }
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn poll_open_uni_stream(
        &self,
        cx: &mut Context<'_>,
        snd_wnd_size: u64,
    ) -> Poll<Result<Option<(StreamId, Writer<Ext<TX>>)>, QuicError>> {
        let mut output = match self.output.guard() {
            Ok(out) => out,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if let Some(sid) = ready!(self.stream_ids.local.poll_alloc_sid(cx, Dir::Uni)) {
            let arc_sender = self.create_sender(sid, snd_wnd_size);
            let io_state = IOState::send_only();
            output.insert(sid, Outgoing(arc_sender.clone()), io_state);
            Poll::Ready(Ok(Some((sid, Writer(arc_sender)))))
        } else {
            Poll::Ready(Ok(None))
        }
    }

    pub(super) fn accept_bi(&self, snd_wnd_size: u64) -> AcceptBiStream<Ext<TX>> {
        self.listener.accept_bi_stream(snd_wnd_size)
    }

    pub(super) fn accept_uni(&self) -> AcceptUniStream<Ext<TX>> {
        self.listener.accept_uni_stream()
    }

    fn try_accept_sid(&self, sid: StreamId) -> Result<(), ExceedLimitError> {
        match sid.dir() {
            Dir::Bi => self.try_accept_bi_sid(sid),
            Dir::Uni => self.try_accept_uni_sid(sid),
        }
    }

    fn try_accept_bi_sid(&self, sid: StreamId) -> Result<(), ExceedLimitError> {
        let mut output = match self.output.guard() {
            Ok(out) => out,
            Err(_) => return Ok(()),
        };
        let mut input = match self.input.guard() {
            Ok(input) => input,
            Err(_) => return Ok(()),
        };
        let mut listener = match self.listener.guard() {
            Ok(listener) => listener,
            Err(_) => return Ok(()),
        };
        let result = self.stream_ids.remote.try_accept_sid(sid)?;

        match result {
            AcceptSid::Old => Ok(()),
            AcceptSid::New(need_create) => {
                let rcv_buf_size = self.remote_bi_stream_rcvbuf_size;
                for sid in need_create {
                    let arc_recver = self.create_recver(sid, rcv_buf_size);
                    let arc_sender = self.create_sender(sid, 0);
                    let io_state = IOState::bidirection();
                    input.insert(sid, Incoming(arc_recver.clone()), io_state.clone());
                    output.insert(sid, Outgoing(arc_sender.clone()), io_state);
                    listener.push_bi_stream(sid, (arc_recver, arc_sender));
                }
                Ok(())
            }
        }
    }

    fn try_accept_uni_sid(&self, sid: StreamId) -> Result<(), ExceedLimitError> {
        let mut input = match self.input.guard() {
            Ok(input) => input,
            Err(_) => return Ok(()),
        };
        let mut listener = match self.listener.guard() {
            Ok(listener) => listener,
            Err(_) => return Ok(()),
        };
        let result = self.stream_ids.remote.try_accept_sid(sid)?;
        match result {
            AcceptSid::Old => Ok(()),
            AcceptSid::New(need_create) => {
                let rcv_buf_size = self.uni_stream_rcvbuf_size;

                for sid in need_create {
                    let arc_receiver = self.create_recver(sid, rcv_buf_size);
                    let io_state = IOState::receive_only();
                    input.insert(sid, Incoming(arc_receiver.clone()), io_state);
                    listener.push_uni_stream(sid, arc_receiver);
                }
                Ok(())
            }
        }
    }

    fn create_sender(&self, sid: StreamId, wnd_size: u64) -> ArcSender<Ext<TX>> {
        ArcSender::new(sid, wnd_size, Ext(self.ctrl_frames.clone()))
    }

    fn create_recver(&self, sid: StreamId, buf_size: u64) -> ArcRecver {
        let arc_recver = recv::new(buf_size);
        // Continuously check whether the MaxStreamData window needs to be updated.
        tokio::spawn({
            let incoming = Incoming(arc_recver.clone());
            let ctrl_frames = self.ctrl_frames.clone();
            async move {
                while let Some(max_data) = incoming.need_update_window().await {
                    ctrl_frames.send_frame([StreamCtlFrame::MaxStreamData(MaxStreamDataFrame {
                        stream_id: sid,
                        max_stream_data: unsafe { VarInt::from_u64_unchecked(max_data) },
                    })]);
                }
            }
        });
        // 监听是否被应用stop了。如果是，则要发送一个StopSendingFrame
        tokio::spawn({
            let incoming = Incoming(arc_recver.clone());
            let ctrl_frames = self.ctrl_frames.clone();
            async move {
                if let Some(err_code) = incoming.is_stopped_by_app().await {
                    ctrl_frames.send_frame([StreamCtlFrame::StopSending(StopSendingFrame {
                        stream_id: sid,
                        app_err_code: VarInt::from_u64(err_code)
                            .expect("app error code must not exceed VARINT_MAX"),
                    })]);
                }
            }
        });
        arc_recver
    }
}