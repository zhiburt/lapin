use crate::{
    buffer::Buffer,
    channels::Channels,
    connection_status::ConnectionState,
    executor::Executor,
    frames::Frames,
    heartbeat::Heartbeat,
    internal_rpc::InternalRPCHandle,
    protocol::{self, AMQPError, AMQPHardError},
    reactor::{ReactorBuilder, ReactorHandle, Slot},
    socket_state::SocketState,
    tcp::HandshakeResult,
    thread::ThreadHandle,
    Configuration, ConnectionStatus, Error, PromiseResolver, Result, TcpStream,
};
use amq_protocol::frame::{gen_frame, parse_frame, AMQPFrame, GenError};
use std::{
    collections::VecDeque,
    convert::TryFrom,
    io::{self, Write},
    sync::Arc,
    thread::Builder as ThreadBuilder,
    time::Duration,
};
use tracing::{debug, error, trace};

const FRAMES_STORAGE: usize = 32;

#[derive(Debug, PartialEq)]
enum Status {
    Initial,
    Connected,
    Stop,
}

pub struct IoLoop {
    connection_status: ConnectionStatus,
    configuration: Configuration,
    channels: Channels,
    internal_rpc: InternalRPCHandle,
    frames: Frames,
    heartbeat: Heartbeat,
    socket_state: SocketState,
    reactor: Box<dyn ReactorHandle + Send>,
    connection_io_loop_handle: ThreadHandle,
    stream: TcpStream,
    slot: Slot,
    status: Status,
    frame_size: usize,
    receive_buffer: Buffer,
    send_buffer: Buffer,
    serialized_frames: VecDeque<(u64, Option<PromiseResolver<()>>)>,
}

impl IoLoop {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        connection_status: ConnectionStatus,
        configuration: Configuration,
        channels: Channels,
        internal_rpc: InternalRPCHandle,
        frames: Frames,
        socket_state: SocketState,
        connection_io_loop_handle: ThreadHandle,
        stream: HandshakeResult,
        reactor_builder: &dyn ReactorBuilder,
        executor: Arc<dyn Executor>,
    ) -> Result<Self> {
        let mut stream = TcpStream::try_from(stream)?;
        let heartbeat = Heartbeat::new(channels.clone());
        let mut reactor = reactor_builder.build(heartbeat.clone(), executor);
        let reactor_handle = reactor.handle();
        let frame_size = std::cmp::max(
            protocol::constants::FRAME_MIN_SIZE as usize,
            configuration.frame_max() as usize,
        );
        let slot = reactor.register(stream.inner_mut(), socket_state.handle())?;

        Ok(Self {
            connection_status,
            configuration,
            channels,
            internal_rpc,
            frames,
            heartbeat,
            socket_state,
            reactor: reactor_handle,
            connection_io_loop_handle,
            stream,
            slot,
            status: Status::Initial,
            frame_size,
            receive_buffer: Buffer::with_capacity(FRAMES_STORAGE * frame_size),
            send_buffer: Buffer::with_capacity(FRAMES_STORAGE * frame_size),
            serialized_frames: VecDeque::default(),
        })
    }

    fn finish_setup(&mut self) -> Result<bool> {
        if self.connection_status.connected() {
            let frame_max = self.configuration.frame_max() as usize;
            self.frame_size = std::cmp::max(self.frame_size, frame_max);
            self.receive_buffer.grow(FRAMES_STORAGE * self.frame_size);
            self.send_buffer.grow(FRAMES_STORAGE * self.frame_size);
            let heartbeat = self.configuration.heartbeat();
            if heartbeat != 0 {
                let heartbeat = Duration::from_millis(u64::from(heartbeat) * 500); // * 1000 (ms) / 2 (half the negotiated timeout)
                self.heartbeat.set_timeout(heartbeat);
                self.reactor.start_heartbeat();
            }
            let peer = self.stream.inner().peer_addr()?;
            debug!(%peer, "Connected");
            self.status = Status::Connected;
        }
        Ok(true)
    }

    fn ensure_setup(&mut self) -> Result<bool> {
        match self.status {
            Status::Initial => self.finish_setup(),
            Status::Connected => Ok(true),
            Status::Stop => Ok(false),
        }
    }

    fn has_data(&self) -> bool {
        self.frames.has_pending()
            || self.send_buffer.available_data() > 0
            || !self.serialized_frames.is_empty()
    }

    fn can_write(&mut self) -> bool {
        self.socket_state.writable() && self.has_data() && !self.connection_status.blocked()
    }

    fn can_read(&mut self) -> bool {
        self.socket_state.readable() && self.receive_buffer.available_space() > 0
    }

    fn can_parse(&self) -> bool {
        self.receive_buffer.available_data() > 0
    }

    fn should_continue(&self) -> bool {
        (self.status != Status::Connected
            || self.connection_status.connected()
            || self.connection_status.closing())
            && self.status != Status::Stop
            && !self.connection_status.errored()
    }

    pub fn start(mut self) -> Result<()> {
        let waker = self.socket_state.handle();
        let handle = self.connection_io_loop_handle.clone();
        handle.register(
            ThreadBuilder::new()
                .name("lapin-io-loop".to_owned())
                .spawn(move || {
                    while self.should_continue() {
                        if let Err(err) = self.run() {
                            self.critical_error(err)?;
                        }
                    }
                    self.internal_rpc.stop();
                    self.heartbeat.cancel();
                    Ok(())
                })?,
        );
        waker.wake();
        Ok(())
    }

    fn poll_socket_events(&mut self) {
        self.socket_state.poll_events();
    }

    fn check_connection_state(&mut self) {
        if self.connection_status.closed() {
            self.status = Status::Stop;
        }
    }

    fn run(&mut self) -> Result<()> {
        trace!("io_loop run");
        self.poll_socket_events();
        if !self.ensure_setup()? {
            return Ok(());
        }
        self.check_connection_state();
        trace!(
            can_read=%self.socket_state.readable(),
            can_write=%self.socket_state.writable(),
            has_data=%self.has_data(),
            "io_loop do_run",
        );
        if !self.can_read() && !self.can_write() && self.should_continue() {
            self.socket_state.wait();
        }
        self.poll_socket_events();
        if self.stream.is_handshaking() {
            self.stream.handshake()?;
            if self.stream.is_handshaking() {
                // We hit WOULDBLOCK while handshaking, wait for the next socket event
                return Ok(());
            }
        }
        self.write()?;
        self.check_connection_state();
        if self.should_continue() {
            self.read()?;
        }
        self.handle_frames()?;
        self.check_connection_state();
        trace!(
            can_read=%self.socket_state.readable(),
            can_write=%self.socket_state.writable(),
            has_data=%self.has_data(),
            status=?self.status,
            "io_loop do_run done",
        );
        Ok(())
    }

    fn critical_error(&mut self, error: Error) -> Result<()> {
        if let Some(resolver) = self.connection_status.connection_resolver() {
            resolver.swear(Err(error.clone()));
        }
        self.status = Status::Stop;
        self.channels.set_connection_error(error.clone());
        for (_, resolver) in std::mem::take(&mut self.serialized_frames) {
            if let Some(resolver) = resolver {
                resolver.swear(Err(error.clone()));
            }
        }
        Err(error)
    }

    fn handle_read_result(&mut self, result: Result<()>) -> Result<()> {
        if let Err(e) = self
            .socket_state
            .handle_read_result(result, &*self.reactor, self.slot)
        {
            error!(error=?e, "error reading");
            self.critical_error(e)?;
        }
        Ok(())
    }

    fn handle_write_result(&mut self, result: Result<()>) -> Result<()> {
        if let Err(e) = self
            .socket_state
            .handle_write_result(result, &*self.reactor, self.slot)
        {
            error!(error=?e, "error writing");
            self.critical_error(e)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.stream.flush()?;
        Ok(())
    }

    fn write(&mut self) -> Result<()> {
        if self.socket_state.writable() {
            let res = self.flush();
            self.handle_write_result(res)?;
        }
        while self.can_write() {
            let res = self.write_to_stream();
            self.handle_write_result(res)?;
        }
        Ok(())
    }

    fn read(&mut self) -> Result<()> {
        while self.can_read() {
            let res = self.read_from_stream();
            self.handle_read_result(res)?;
        }
        Ok(())
    }

    fn write_to_stream(&mut self) -> Result<()> {
        self.flush()?;
        self.serialize()?;

        let sz = self.send_buffer.write_to(&mut self.stream)?;

        if sz > 0 {
            self.heartbeat.update_last_write();

            trace!("wrote {} bytes", sz);
            self.send_buffer.consume(sz);

            let mut written = sz as u64;
            while written > 0 {
                if let Some((to_write, resolver)) = self.serialized_frames.pop_front() {
                    if written < to_write {
                        self.serialized_frames
                            .push_front((to_write - written, resolver));
                        trace!("{} to write to complete this frame", to_write - written);
                        written = 0;
                    } else {
                        if let Some(resolver) = resolver {
                            resolver.swear(Ok(()));
                        }
                        written -= to_write;
                    }
                } else {
                    error!(
                        "We've written {} but didn't expect to write anything",
                        written
                    );
                    break;
                }
            }

            if self.send_buffer.available_data() > 0 {
                // We didn't write all the data yet
                trace!("Still {} to write", self.send_buffer.available_data());
            }

            self.flush()?;
        } else {
            error!("Socket was writable but we wrote 0, marking as wouldblock");
            self.handle_write_result(Err(io::Error::from(io::ErrorKind::WouldBlock).into()))?;
        }
        Ok(())
    }

    fn read_from_stream(&mut self) -> Result<()> {
        match self.connection_status.state() {
            ConnectionState::Closed => Ok(()),
            ConnectionState::Error => Err(Error::InvalidConnectionState(ConnectionState::Error)),
            _ => {
                let sz = self.receive_buffer.read_from(&mut self.stream)?;

                if sz > 0 {
                    trace!("read {} bytes", sz);
                    self.receive_buffer.fill(sz);
                } else {
                    error!("Socket was readable but we read 0, marking as wouldblock");
                    self.handle_read_result(
                        Err(io::Error::from(io::ErrorKind::WouldBlock).into()),
                    )?;
                }
                Ok(())
            }
        }
    }

    fn serialize(&mut self) -> Result<()> {
        while let Some((next_msg, resolver)) = self.frames.pop(self.channels.flow()) {
            trace!(%next_msg, "will write to buffer");
            let checkpoint = self.send_buffer.checkpoint();
            let res = gen_frame(&next_msg)((&mut self.send_buffer).into());
            match res.map(|w| w.into_inner().1) {
                Ok(sz) => self.serialized_frames.push_back((sz, resolver)),
                Err(e) => {
                    self.send_buffer.rollback(checkpoint);
                    match e {
                        GenError::BufferTooSmall(_) => {
                            // Requeue msg
                            self.frames.retry((next_msg, resolver));
                            break;
                        }
                        e => {
                            error!(error=?e, "error generating frame");
                            self.critical_error(Error::SerialisationError(Arc::new(e)))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_frames(&mut self) -> Result<()> {
        while self.can_parse() {
            if let Some(frame) = self.parse()? {
                self.channels.handle_frame(frame)?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn parse(&mut self) -> Result<Option<AMQPFrame>> {
        match parse_frame(self.receive_buffer.parsing_context()) {
            Ok((i, f)) => {
                let consumed = self.receive_buffer.offset(i);
                let frame_max = self.configuration.frame_max() as usize;
                if frame_max > 0 && consumed > frame_max {
                    error!(bytes = consumed, "received large frame");
                    let error = AMQPError::new(
                        AMQPHardError::FRAMEERROR.into(),
                        format!("frame too large: {} bytes", consumed).into(),
                    );
                    self.internal_rpc.close_connection(
                        error.get_id(),
                        error.get_message().to_string(),
                        0,
                        0,
                    );
                    self.critical_error(Error::ProtocolError(error))?;
                }
                self.receive_buffer.consume(consumed);
                Ok(Some(f))
            }
            Err(e) => {
                if !e.is_incomplete() {
                    error!(error=?e, "parse error");
                    self.critical_error(Error::ParsingError(e))?;
                }
                Ok(None)
            }
        }
    }
}
