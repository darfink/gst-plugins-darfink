//! RTMP server session.

use std::future::Future;
use std::io;
use std::time::Duration;

use bytes::BytesMut;
use scuffle_bytes_util::{BytesCursorExt, StringCow};
use scuffle_context::ContextFutExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::chunk::CHUNK_SIZE;
use crate::chunk::reader::ChunkReader;
use crate::chunk::writer::ChunkWriter;
use crate::command_messages::netconnection::{
    CapsExMask, NetConnectionCommand, NetConnectionCommandConnect, NetConnectionCommandConnectResult,
};
use crate::command_messages::netstream::{NetStreamCommand, NetStreamCommandPublishPublishingType};
use crate::command_messages::on_status::{OnStatus, OnStatusCode};
use crate::command_messages::{Command, CommandResultLevel, CommandType};
use crate::handshake;
use crate::handshake::HandshakeServer;
use crate::messages::MessageData;
use crate::protocol_control_messages::{
    ProtocolControlMessageAcknowledgement, ProtocolControlMessageSetChunkSize, ProtocolControlMessageSetPeerBandwidth,
    ProtocolControlMessageSetPeerBandwidthLimitType, ProtocolControlMessageWindowAcknowledgementSize,
};
use crate::user_control_messages::EventMessageStreamBegin;

mod error;
mod handler;

pub use error::ServerSessionError;
pub use handler::{SessionData, SessionHandler};

/// How a server session should react when its owner shuts down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSessionShutdown {
    /// Drop the session immediately without sending an RTMP shutdown message.
    Abrupt,
    /// Send the server-side connection-closed status and wait for the client
    /// to close, up to the supplied timeout.
    Graceful(Duration),
}

// The default acknowledgement window size that is used until the client sends a
// new acknowledgement window size.
// This is a common value used by other media servers as well.
// - https://github.com/FFmpeg/FFmpeg/blob/154c00514d889d27ae84a1001e00f9032fdc1c54/libavformat/rtmpproto.c#L2850
const DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE: u32 = 2_500_000; // 2.5 MB

/// Network operation timeouts for an RTMP server session.
///
/// A value of `None` disables that timeout. The defaults preserve the
/// timeouts used by scuffle-rtmp 0.2.3 before they were configurable.
#[derive(Clone, Copy, Debug)]
pub struct ServerSessionTimeouts {
    /// Maximum time allowed for each handshake read.
    pub handshake_read: Option<Duration>,
    /// Maximum time allowed for each established-session read.
    pub session_read: Option<Duration>,
    /// Maximum time allowed for each socket write.
    pub write: Option<Duration>,
}

impl Default for ServerSessionTimeouts {
    fn default() -> Self {
        Self {
            handshake_read: Some(Duration::from_secs(2)),
            session_read: Some(Duration::from_millis(2500)),
            write: Some(Duration::from_secs(2)),
        }
    }
}

/// A RTMP server session that is used to communicate with a client.
///
/// This provides a high-level API to drive a RTMP session.
pub struct ServerSession<S, H> {
    /// The context of the session
    /// A reconnect request will be sent if this context gets cancelled.
    ctx: Option<scuffle_context::Context>,
    /// Keep track of whether a reconnect request has already been sent.
    reconnect_request_sent: bool,
    /// When you connect via rtmp, you specify the app name in the url
    /// For example: rtmp://localhost:1935/live/xyz
    /// The app name is "live"
    /// The next part of the url is the stream name (or the stream key) "xyz"
    /// However the stream key is not required to be the same for each stream
    /// you publish / play Traditionally we only publish a single stream per
    /// RTMP connection, However we can publish multiple streams per RTMP
    /// connection (using different stream keys) and or play multiple streams
    /// per RTMP connection (using different stream keys) as per the RTMP spec.
    app_name: Option<StringCow<'static>>,
    caps_ex: Option<CapsExMask>,
    /// Used to read and write data
    io: S,
    handler: H,
    /// The size of the acknowledgement window
    acknowledgement_window_size: u32,
    /// The number of bytes read from the stream. Value wraps when reaching u32::MAX.
    /// This is used to know when to send acknoledgements.
    sequence_number: u32,
    /// Bytes received since the last acknowledgement.
    bytes_since_acknowledgement: u32,
    /// Buffer to read data into
    read_buf: BytesMut,
    /// Buffer to write data to
    write_buf: Vec<u8>,
    /// Sometimes when doing the handshake we read too much data,
    /// this flag is used to indicate that we have data ready to parse and we
    /// should not read more data from the stream
    skip_read: bool,
    /// This is used to read the data from the stream and convert it into rtmp
    /// messages
    chunk_reader: ChunkReader,
    /// This is used to convert rtmp messages into chunks
    chunk_writer: ChunkWriter,
    /// Is Publishing
    publishing_stream_ids: Vec<u32>,
    /// Timeouts applied to network reads and writes.
    timeouts: ServerSessionTimeouts,
}

impl<S, H> ServerSession<S, H> {
    /// Create a new session.
    pub fn new(io: S, handler: H) -> Self {
        Self {
            ctx: None,
            reconnect_request_sent: false,
            app_name: None,
            caps_ex: None,
            io,
            handler,
            acknowledgement_window_size: DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE,
            sequence_number: 0,
            bytes_since_acknowledgement: 0,
            skip_read: false,
            chunk_reader: ChunkReader::default(),
            chunk_writer: ChunkWriter::default(),
            read_buf: BytesMut::new(),
            write_buf: Vec::new(),
            publishing_stream_ids: Vec::new(),
            timeouts: ServerSessionTimeouts::default(),
        }
    }

    /// Set the context of the session.
    pub fn with_context(mut self, ctx: scuffle_context::Context) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Configure network operation timeouts for this session.
    pub fn with_timeouts(mut self, timeouts: ServerSessionTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin, H: SessionHandler> ServerSession<S, H> {
    /// Run the session to completion.
    ///
    /// The result of the return value will be true if all publishers have
    /// disconnected. If any publishers are still connected, the result will
    /// be false. This can be used to detect non-graceful disconnects (ie. the
    /// client crashed).
    pub async fn run(self) -> Result<bool, crate::error::RtmpError> {
        self.run_with_shutdown(std::future::pending()).await
    }

    /// Run the session while listening for an owner shutdown request.
    ///
    /// An abrupt request returns immediately and drops the connection. A
    /// graceful request sends `NetConnection.Connect.Closed`, flushes it, and
    /// continues driving the session until the client closes or the timeout
    /// expires.
    pub async fn run_with_shutdown<F>(mut self, shutdown: F) -> Result<bool, crate::error::RtmpError>
    where
        F: Future<Output = ServerSessionShutdown>,
    {
        let ctx = self.ctx.clone().unwrap_or_else(scuffle_context::Context::global);
        tokio::pin!(shutdown);

        let mut handshaker = HandshakeServer::default();
        // Run the handshake to completion
        loop {
            let handshake_result = {
                let handshake = self.drive_handshake(&mut handshaker).with_context(&ctx);
                tokio::pin!(handshake);
                tokio::select! {
                    shutdown = &mut shutdown => return Ok(matches!(shutdown, ServerSessionShutdown::Abrupt)),
                    result = &mut handshake => result,
                }
            };
            match handshake_result {
                Some(Ok(false)) => self.flush().await?, // Continue driving
                Some(Ok(true)) => break,                // Handshake is complete
                Some(Err(e)) => return Err(e),
                None => return Ok(false), // Context was cancelled
            }
        }

        // Drop the handshaker, we don't need it anymore
        // We can get rid of the memory that was allocated for it
        drop(handshaker);

        tracing::debug!("handshake complete");

        // Drive the session to completion
        loop {
            match tokio::select! {
                shutdown = &mut shutdown => return self.handle_shutdown(shutdown).await,
                result = self.drive() => result,
            } {
                Ok(true) => self.flush().await?, // Continue driving
                Ok(false) => break,              // Client has closed the connection
                Err(err) if err.is_client_closed() => {
                    // The client closed the connection
                    // We are done with the session
                    tracing::debug!("client closed the connection");
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        // We should technically check the stream_map here
        // However most clients just disconnect without cleanly stopping the subscrition
        // streams (play streams) So we just check that all publishers have disconnected
        // cleanly
        Ok(self.publishing_stream_ids.is_empty())
    }

    async fn handle_shutdown(&mut self, shutdown: ServerSessionShutdown) -> Result<bool, crate::error::RtmpError> {
        match shutdown {
            ServerSessionShutdown::Abrupt => Ok(false),
            ServerSessionShutdown::Graceful(timeout) => {
                self.send_connection_closed().await?;

                let deadline = tokio::time::sleep(timeout);
                tokio::pin!(deadline);
                loop {
                    match tokio::select! {
                        _ = &mut deadline => break,
                        result = self.drive() => result,
                    } {
                        Ok(true) => self.flush().await?,
                        Ok(false) => break,
                        Err(error) if error.is_client_closed() => break,
                        Err(error) => return Err(error),
                    }
                }

                Ok(false)
            }
        }
    }

    async fn send_connection_closed(&mut self) -> Result<(), crate::error::RtmpError> {
        Command {
            command_type: CommandType::OnStatus(OnStatus {
                level: CommandResultLevel::Status,
                code: OnStatusCode::NET_CONNECTION_CONNECT_CLOSED,
                description: Some("The server is shutting down.".into()),
                others: None,
            }),
            transaction_id: 0.0,
        }
        .write(&mut self.write_buf, &self.chunk_writer, 0)?;

        self.flush().await
    }

    /// This drives the first stage of the session.
    /// It is used to do the handshake with the client.
    /// The handshake is the first thing that happens when a client connects to a
    /// RTMP server.
    ///
    /// Returns true if the handshake is complete, false if the handshake is not complete yet.
    /// If the handshake is not complete yet, this function should be called again.
    async fn drive_handshake(&mut self, handshaker: &mut HandshakeServer) -> Result<bool, crate::error::RtmpError> {
        // Read the handshake data + 1 byte for the version
        const READ_SIZE: usize = handshake::RTMP_HANDSHAKE_SIZE + 1;
        self.read_buf.reserve(READ_SIZE);

        let mut bytes_read = 0;
        while bytes_read < READ_SIZE {
            let n = io_with_timeout(
                self.timeouts.handshake_read,
                self.io.read_buf(&mut self.read_buf),
            )
            .await?;
            bytes_read += n;

            self.sequence_number = self.sequence_number.wrapping_add(n.try_into().unwrap_or(u32::MAX));
        }

        let mut cursor = std::io::Cursor::new(self.read_buf.split().freeze());

        handshaker.handshake(&mut cursor, &mut self.write_buf)?;

        if handshaker.is_finished() {
            let over_read = cursor.extract_remaining();

            if !over_read.is_empty() {
                self.skip_read = true;
                self.read_buf.extend_from_slice(&over_read);
            }

            self.send_set_chunk_size().await?;

            // We are done with the handshake
            // This causes the loop to exit
            // And move onto the next stage of the session
            Ok(true)
        } else {
            // We are not done with the handshake yet
            // We need to read more data from the stream
            // This causes the loop to continue
            Ok(false)
        }
    }

    /// This drives the second and main stage of the session.
    /// It is used to read data from the stream and parse it into RTMP messages.
    /// We also send data to the client if they are playing a stream.
    ///
    /// Finish the handshake first by repeatedly calling [`drive_handshake`][ServerSession::drive_handshake]
    /// until it returns true before calling this function.
    ///
    /// Returns true if the session is still active, false if the client has closed the connection.
    async fn drive(&mut self) -> Result<bool, crate::error::RtmpError> {
        // Send a reconnect request if we haven't yet, the client supports it and the context is cancelled
        if !self.reconnect_request_sent
            && self.caps_ex.is_some_and(|c| c.intersects(CapsExMask::Reconnect))
            && self.ctx.as_ref().is_some_and(|ctx| ctx.is_done())
        {
            tracing::debug!("sending reconnect request");

            OnStatus {
                code: OnStatusCode::NET_CONNECTION_CONNECT_RECONNECT_REQUEST,
                level: CommandResultLevel::Status,
                description: None,
                others: None,
            }
            .write(&mut self.write_buf, 0.0)?;

            self.reconnect_request_sent = true;
        }

        // If we have data ready to parse, parse it
        if self.skip_read {
            self.skip_read = false;
        } else {
            self.read_buf.reserve(CHUNK_SIZE);

            let n = io_with_timeout(
                self.timeouts.session_read,
                self.io.read_buf(&mut self.read_buf),
            )
            .await? as u32;

            if n == 0 {
                return Ok(false);
            }

            self.record_bytes_read(n)?;
        }

        self.process_chunks().await?;

        Ok(true)
    }

    /// Parse data from the client into RTMP messages and process them.
    async fn process_chunks(&mut self) -> Result<(), crate::error::RtmpError> {
        while let Some(chunk) = self.chunk_reader.read_chunk(&mut self.read_buf)? {
            let timestamp = chunk.message_header.timestamp;
            let msg_stream_id = chunk.message_header.msg_stream_id;

            let msg = MessageData::read(&chunk)?;
            self.process_message(msg, msg_stream_id, timestamp).await?;
        }

        Ok(())
    }

    /// Process one RTMP message
    async fn process_message(
        &mut self,
        msg: MessageData<'_>,
        stream_id: u32,
        timestamp: u32,
    ) -> Result<(), crate::error::RtmpError> {
        match msg {
            MessageData::Amf0Command(command) => self.on_command_message(stream_id, command).await?,
            MessageData::SetChunkSize(ProtocolControlMessageSetChunkSize { chunk_size }) => {
                self.on_set_chunk_size(chunk_size as usize)?;
            }
            MessageData::SetAcknowledgementWindowSize(ProtocolControlMessageWindowAcknowledgementSize {
                acknowledgement_window_size,
            }) => {
                self.on_acknowledgement_window_size(acknowledgement_window_size)?;
            }
            MessageData::AudioData { data } => {
                self.handler
                    .on_data(stream_id, SessionData::Audio { timestamp, data })
                    .await?;
            }
            MessageData::VideoData { data } => {
                self.handler
                    .on_data(stream_id, SessionData::Video { timestamp, data })
                    .await?;
            }
            MessageData::DataAmf0 { data } => {
                self.handler.on_data(stream_id, SessionData::Amf0 { timestamp, data }).await?;
            }
            MessageData::Unknown(unknown_message) => {
                self.handler.on_unknown_message(stream_id, unknown_message).await?;
            }
            // ignore everything else
            _ => {}
        }

        Ok(())
    }

    /// Set the server chunk size to the client
    async fn send_set_chunk_size(&mut self) -> Result<(), crate::error::RtmpError> {
        ProtocolControlMessageSetChunkSize {
            chunk_size: CHUNK_SIZE as u32,
        }
        .write(&mut self.write_buf, &self.chunk_writer)?;
        self.chunk_writer.set_chunk_size(CHUNK_SIZE);

        Ok(())
    }

    /// on_amf0_command_message is called when we receive an AMF0 command
    /// message from the client We then handle the command message
    async fn on_command_message(&mut self, stream_id: u32, command: Command<'_>) -> Result<(), crate::error::RtmpError> {
        match command.command_type {
            CommandType::NetConnection(NetConnectionCommand::Connect(connect)) => {
                self.on_command_connect(stream_id, command.transaction_id, connect).await?;
            }
            CommandType::NetConnection(NetConnectionCommand::CreateStream) => {
                self.on_command_create_stream(stream_id, command.transaction_id).await?;
            }
            CommandType::NetStream(NetStreamCommand::Play { .. })
            | CommandType::NetStream(NetStreamCommand::Play2 { .. }) => {
                return Err(crate::error::RtmpError::Session(ServerSessionError::PlayNotSupported));
            }
            CommandType::NetStream(NetStreamCommand::DeleteStream {
                stream_id: delete_stream_id,
            }) => {
                self.on_command_delete_stream(stream_id, command.transaction_id, delete_stream_id)
                    .await?;
            }
            CommandType::NetStream(NetStreamCommand::CloseStream) => {
                // Not sure what this does, might be important
            }
            CommandType::NetStream(NetStreamCommand::Publish {
                publishing_name,
                publishing_type,
            }) => {
                self.on_command_publish(stream_id, command.transaction_id, publishing_name.as_str(), publishing_type)
                    .await?;
            }
            CommandType::Unknown(unknown_command) => {
                // A `deleteStream` naming its stream rather than numbering it
                // arrives here (see the reader). It is still an unpublish, so
                // resolve it against what this connection publishes.
                if unknown_command.command_name.as_str() == "deleteStream" {
                    if let Some(&publishing) = self.publishing_stream_ids.first() {
                        self.on_command_delete_stream(stream_id, command.transaction_id, publishing as f64)
                            .await?;
                    }
                } else {
                    self.handler.on_unknown_command(stream_id, unknown_command).await?;
                }
            }
            // ignore everything else
            _ => {}
        }

        Ok(())
    }

    /// on_set_chunk_size is called when we receive a set chunk size message
    /// from the client We then update the chunk size of the unpacketizer
    fn on_set_chunk_size(&mut self, chunk_size: usize) -> Result<(), crate::error::RtmpError> {
        if self.chunk_reader.update_max_chunk_size(chunk_size) {
            Ok(())
        } else {
            Err(crate::error::RtmpError::Session(ServerSessionError::InvalidChunkSize(
                chunk_size,
            )))
        }
    }

    /// on_acknowledgement_window_size is called when we receive a new acknowledgement window size
    /// from the client.
    fn on_acknowledgement_window_size(&mut self, acknowledgement_window_size: u32) -> Result<(), crate::error::RtmpError> {
        tracing::debug!(acknowledgement_window_size = %acknowledgement_window_size, "received new acknowledgement window size");
        if acknowledgement_window_size == 0 {
            return Err(ServerSessionError::InvalidAcknowledgementWindowSize(
                acknowledgement_window_size,
            )
            .into());
        }
        self.acknowledgement_window_size = acknowledgement_window_size;
        Ok(())
    }

    /// Record newly received bytes and emit an acknowledgement after crossing
    /// the peer's configured window. The acknowledgement sequence includes the
    /// bytes from the read that crossed the boundary.
    fn record_bytes_read(&mut self, bytes_read: u32) -> Result<(), crate::error::RtmpError> {
        self.sequence_number = self.sequence_number.wrapping_add(bytes_read);
        let unacknowledged = u64::from(self.bytes_since_acknowledgement) + u64::from(bytes_read);
        let window = u64::from(self.acknowledgement_window_size);

        if unacknowledged >= window {
            tracing::debug!(sequence_number = %self.sequence_number, "sending acknowledgement");
            ProtocolControlMessageAcknowledgement {
                sequence_number: self.sequence_number,
            }
            .write(&mut self.write_buf, &self.chunk_writer)?;
            self.bytes_since_acknowledgement = (unacknowledged % window) as u32;
        } else {
            self.bytes_since_acknowledgement = unacknowledged as u32;
        }

        Ok(())
    }

    /// on_command_connect is called when we receive a amf0 command message with
    /// the name "connect" We then handle the connect message
    /// This is called when the client first connects to the server
    async fn on_command_connect(
        &mut self,
        _stream_id: u32,
        transaction_id: f64,
        connect: NetConnectionCommandConnect<'_>,
    ) -> Result<(), crate::error::RtmpError> {
        ProtocolControlMessageWindowAcknowledgementSize {
            acknowledgement_window_size: DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE,
        }
        .write(&mut self.write_buf, &self.chunk_writer)?;

        ProtocolControlMessageSetPeerBandwidth {
            acknowledgement_window_size: DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE,
            limit_type: ProtocolControlMessageSetPeerBandwidthLimitType::Dynamic,
        }
        .write(&mut self.write_buf, &self.chunk_writer)?;

        self.app_name = Some(connect.app.into_owned());
        self.caps_ex = connect.caps_ex;

        let result = NetConnectionCommand::ConnectResult(NetConnectionCommandConnectResult::default());

        Command {
            command_type: CommandType::NetConnection(result),
            transaction_id,
        }
        .write(&mut self.write_buf, &self.chunk_writer, 0)?;

        Ok(())
    }

    /// on_command_create_stream is called when we receive a amf0 command
    /// message with the name "createStream" We then handle the createStream
    /// message This is called when the client wants to create a stream
    /// A NetStream is used to start publishing or playing a stream
    async fn on_command_create_stream(
        &mut self,
        _stream_id: u32,
        transaction_id: f64,
    ) -> Result<(), crate::error::RtmpError> {
        // 1.0 is the Stream ID of the stream we are creating
        Command {
            command_type: CommandType::NetConnection(NetConnectionCommand::CreateStreamResult { stream_id: 1.0 }),
            transaction_id,
        }
        .write(&mut self.write_buf, &self.chunk_writer, 0)?;

        Ok(())
    }

    /// A delete stream message is unrelated to the NetConnection close method.
    /// Delete stream is basically a way to tell the server that you are done
    /// publishing or playing a stream. The server will then remove the stream
    /// from its list of streams.
    async fn on_command_delete_stream(
        &mut self,
        _stream_id: u32,
        transaction_id: f64,
        delete_stream_id: f64,
    ) -> Result<(), crate::error::RtmpError> {
        let stream_id = delete_stream_id as u32;

        self.handler.on_unpublish(stream_id).await?;

        // Remove the stream id from the list of publishing stream ids
        self.publishing_stream_ids.retain(|id| *id != stream_id);

        Command {
            command_type: CommandType::OnStatus(OnStatus {
                level: CommandResultLevel::Status,
                code: OnStatusCode::NET_STREAM_DELETE_STREAM_SUCCESS,
                description: None,
                others: None,
            }),
            transaction_id,
        }
        // A NetStream status belongs on the stream it describes, not on 0.
        .write(&mut self.write_buf, &self.chunk_writer, stream_id)?;

        Ok(())
    }

    /// on_command_publish is called when we receive a amf0 command message with
    /// the name "publish" publish commands are used to publish a stream to the
    /// server ie. the user wants to start streaming to the server
    async fn on_command_publish(
        &mut self,
        stream_id: u32,
        transaction_id: f64,
        publishing_name: &str,
        _publishing_type: NetStreamCommandPublishPublishingType<'_>,
    ) -> Result<(), crate::error::RtmpError> {
        let Some(app_name) = &self.app_name else {
            // The app name is not set yet
            return Err(crate::error::RtmpError::Session(ServerSessionError::PublishBeforeConnect));
        };

        self.handler.on_publish(stream_id, app_name.as_ref(), publishing_name).await?;

        self.publishing_stream_ids.push(stream_id);

        EventMessageStreamBegin { stream_id }.write(&self.chunk_writer, &mut self.write_buf)?;

        Command {
            command_type: CommandType::OnStatus(OnStatus {
                level: CommandResultLevel::Status,
                code: OnStatusCode::NET_STREAM_PUBLISH_START,
                description: None,
                others: None,
            }),
            transaction_id,
        }
        // A NetStream status belongs on the stream it describes, not on 0.
        .write(&mut self.write_buf, &self.chunk_writer, stream_id)?;

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), crate::error::RtmpError> {
        if !self.write_buf.is_empty() {
            io_with_timeout(
                self.timeouts.write,
                self.io.write_all(self.write_buf.as_ref()),
            )
            .await?;
            self.write_buf.clear();
        }

        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(all(test, coverage_nightly), coverage(off))]
mod tests {
    use std::collections::HashMap;

    use bytes::BytesMut;

    use super::*;
    use crate::chunk::reader::ChunkReader;
    use crate::messages::MessageType;

    struct Handler;

    impl SessionHandler for Handler {
        async fn on_publish(&mut self, _stream_id: u32, _app_name: &str, _stream_name: &str) -> Result<(), ServerSessionError> {
            Ok(())
        }

        async fn on_unpublish(&mut self, _stream_id: u32) -> Result<(), ServerSessionError> {
            Ok(())
        }

        async fn on_data(&mut self, _stream_id: u32, _data: SessionData) -> Result<(), ServerSessionError> {
            Ok(())
        }
    }

    fn session() -> ServerSession<tokio::io::DuplexStream, Handler> {
        let (server, _client) = tokio::io::duplex(64);
        ServerSession::new(server, Handler)
    }

    #[tokio::test]
    async fn graceful_shutdown_sends_connection_closed_status() {
        let (server, mut client) = tokio::io::duplex(4096);
        let mut session = ServerSession::new(server, Handler);
        session.publishing_stream_ids.push(1);

        session
            .send_connection_closed()
            .await
            .expect("connection-closed status");

        let mut wire = vec![0; 4096];
        let length = tokio::io::AsyncReadExt::read(&mut client, &mut wire)
            .await
            .expect("read connection-closed status");
        assert!(wire[..length]
            .windows(b"NetConnection.Connect.Closed".len())
            .any(|window| window == b"NetConnection.Connect.Closed"));
    }

    #[tokio::test]
    async fn connect_advertises_ffmpeg_acknowledgement_windows() {
        let mut session = session();
        session
            .on_command_connect(0, 1.0, NetConnectionCommandConnect { app: "live".into(), caps_ex: None, others: HashMap::new() })
            .await
            .expect("connect response");

        let mut wire = BytesMut::from(session.write_buf.as_slice());
        let mut reader = ChunkReader::default();
        let window = reader.read_chunk(&mut wire).expect("read window chunk").expect("complete window chunk");
        assert_eq!(window.message_header.msg_type_id, MessageType::WindowAcknowledgementSize);
        assert_eq!(window.payload.as_ref(), DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE.to_be_bytes());

        let bandwidth = reader.read_chunk(&mut wire).expect("read bandwidth chunk").expect("complete bandwidth chunk");
        assert_eq!(bandwidth.message_header.msg_type_id, MessageType::SetPeerBandwidth);
        assert_eq!(bandwidth.payload.as_ref(), [DEFAULT_ACKNOWLEDGEMENT_WINDOW_SIZE.to_be_bytes().as_slice(), &[2]].concat());
    }

    #[test]
    fn acknowledgement_includes_boundary_crossing_read() {
        let mut session = session();
        session.acknowledgement_window_size = 10;
        session.sequence_number = 7;
        session.bytes_since_acknowledgement = 7;

        session.record_bytes_read(5).expect("record bytes");

        assert_eq!(session.sequence_number, 12);
        assert_eq!(session.bytes_since_acknowledgement, 2);
        let mut wire = BytesMut::from(session.write_buf.as_slice());
        let acknowledgement =
            ChunkReader::default().read_chunk(&mut wire).expect("read acknowledgement").expect("complete acknowledgement");
        assert_eq!(acknowledgement.message_header.msg_type_id, MessageType::Acknowledgement);
        assert_eq!(acknowledgement.payload.as_ref(), 12_u32.to_be_bytes());
    }

    #[test]
    fn zero_acknowledgement_window_is_rejected() {
        let mut session = session();
        let error = session.on_acknowledgement_window_size(0).expect_err("zero window must fail");
        assert!(matches!(error, crate::error::RtmpError::Session(ServerSessionError::InvalidAcknowledgementWindowSize(0))));
    }
}

async fn io_with_timeout<T>(
    timeout: Option<Duration>,
    future: impl Future<Output = io::Result<T>>,
) -> Result<T, crate::error::RtmpError> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(ServerSessionError::Timeout)?,
        None => future.await,
    }
    .map_err(Into::into)
}
