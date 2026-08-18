use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;
use scuffle_rtmp::ServerSession;
use scuffle_rtmp::session::server::{
  ServerSessionError, ServerSessionTimeouts, SessionData, SessionHandler,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_ADDRESS: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 1935;
const DEFAULT_TCP_NODELAY: bool = true;
const DEFAULT_ACCEPT_TIMEOUT: u64 = 0;
const DEFAULT_HANDSHAKE_TIMEOUT: u64 = 10_000_000_000;
const DEFAULT_READ_TIMEOUT: u64 = 0;
const DEFAULT_WRITE_TIMEOUT: u64 = 10_000_000_000;
const OUTPUT_QUEUE_CAPACITY: usize = 1;
const CREATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WORKER_START_TIMEOUT: Duration = Duration::from_secs(2);

const FLV_TAG_AUDIO: u8 = 8;
const FLV_TAG_VIDEO: u8 = 9;
const FLV_TAG_SCRIPT_DATA: u8 = 18;
const FLV_HEADER: &[u8] = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
  gst::DebugCategory::new(
    "scufflertmplistensrc",
    gst::DebugColorFlags::empty(),
    Some("Scuffle RTMP listener source"),
  )
});

#[derive(Clone)]
struct Settings {
  address: String,
  port: u16,
  tcp_nodelay: bool,
  accept_timeout: u64,
  handshake_timeout: u64,
  read_timeout: u64,
  write_timeout: u64,
  application: Option<String>,
  stream_key: Option<String>,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      address: DEFAULT_ADDRESS.into(),
      port: DEFAULT_PORT,
      tcp_nodelay: DEFAULT_TCP_NODELAY,
      accept_timeout: DEFAULT_ACCEPT_TIMEOUT,
      handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
      read_timeout: DEFAULT_READ_TIMEOUT,
      write_timeout: DEFAULT_WRITE_TIMEOUT,
      application: None,
      stream_key: None,
    }
  }
}

enum WorkerOutput {
  Data(Vec<u8>),
  Warning(String),
  Error(String),
  Eos,
  Wake,
}

#[derive(Default)]
struct State {
  receiver: Option<flume::Receiver<WorkerOutput>>,
  sender: Option<flume::Sender<WorkerOutput>>,
  cancellation: Option<CancellationToken>,
  worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct ScuffleRtmpListenSrc {
  settings: Mutex<Settings>,
  state: Mutex<State>,
  flushing: AtomicBool,
}

#[glib::object_subclass]
impl ObjectSubclass for ScuffleRtmpListenSrc {
  const NAME: &'static str = "GstScuffleRtmpListenSrc";
  type Type = super::ScuffleRtmpListenSrc;
  type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for ScuffleRtmpListenSrc {
  fn properties() -> &'static [glib::ParamSpec] {
    static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
      vec![
        glib::ParamSpecString::builder("address")
          .nick("Address")
          .blurb("IP address on which to accept one RTMP publisher")
          .default_value(Some(DEFAULT_ADDRESS))
          .mutable_ready()
          .build(),
        glib::ParamSpecUInt::builder("port")
          .nick("Port")
          .blurb("TCP port on which to accept one RTMP publisher (0 allocates a free port)")
          .maximum(u16::MAX as u32)
          .default_value(DEFAULT_PORT as u32)
          .mutable_ready()
          .build(),
        glib::ParamSpecBoolean::builder("tcp-nodelay")
          .nick("TCP no-delay")
          .blurb("Disable Nagle's algorithm on the accepted RTMP connection")
          .default_value(DEFAULT_TCP_NODELAY)
          .mutable_ready()
          .build(),
        glib::ParamSpecUInt64::builder("accept-timeout")
          .nick("Publisher accept timeout")
          .blurb("Nanoseconds to wait for a publisher connection (0 waits indefinitely)")
          .default_value(DEFAULT_ACCEPT_TIMEOUT)
          .mutable_ready()
          .build(),
        glib::ParamSpecUInt64::builder("handshake-timeout")
          .nick("Handshake read timeout")
          .blurb("Nanoseconds allowed for each RTMP handshake read (0 disables the timeout)")
          .default_value(DEFAULT_HANDSHAKE_TIMEOUT)
          .mutable_ready()
          .build(),
        glib::ParamSpecUInt64::builder("read-timeout")
          .nick("Session read timeout")
          .blurb("Nanoseconds allowed without RTMP session input (0 disables the timeout)")
          .default_value(DEFAULT_READ_TIMEOUT)
          .mutable_ready()
          .build(),
        glib::ParamSpecUInt64::builder("write-timeout")
          .nick("Session write timeout")
          .blurb("Nanoseconds allowed for each RTMP socket write (0 disables the timeout)")
          .default_value(DEFAULT_WRITE_TIMEOUT)
          .mutable_ready()
          .build(),
        glib::ParamSpecString::builder("application")
          .nick("RTMP application")
          .blurb("RTMP application path component to accept (unset accepts any)")
          .mutable_ready()
          .build(),
        glib::ParamSpecString::builder("stream-key")
          .nick("RTMP stream key")
          .blurb("RTMP publishing name/stream key to accept (unset accepts any)")
          .mutable_ready()
          .build(),
      ]
    });

    PROPERTIES.as_ref()
  }

  fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
    let mut settings = self.settings.lock().expect("settings mutex poisoned");

    match pspec.name() {
      "address" => {
        settings.address = value
          .get::<Option<String>>()
          .expect("address type checked upstream")
          .unwrap_or_else(|| DEFAULT_ADDRESS.into());
      }
      "port" => {
        settings.port = value.get::<u32>().expect("port type checked upstream") as u16;
      }
      "tcp-nodelay" => {
        settings.tcp_nodelay = value.get().expect("tcp-nodelay type checked upstream");
      }
      "accept-timeout" => {
        settings.accept_timeout = value.get().expect("accept-timeout type checked upstream");
      }
      "handshake-timeout" => {
        settings.handshake_timeout = value
          .get()
          .expect("handshake-timeout type checked upstream");
      }
      "read-timeout" => {
        settings.read_timeout = value.get().expect("read-timeout type checked upstream");
      }
      "write-timeout" => {
        settings.write_timeout = value.get().expect("write-timeout type checked upstream");
      }
      "application" => {
        settings.application = value.get().expect("application type checked upstream");
      }
      "stream-key" => {
        settings.stream_key = value.get().expect("stream-key type checked upstream");
      }
      _ => unimplemented!(),
    }
  }

  fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
    let settings = self.settings.lock().expect("settings mutex poisoned");

    match pspec.name() {
      "address" => settings.address.to_value(),
      "port" => (settings.port as u32).to_value(),
      "tcp-nodelay" => settings.tcp_nodelay.to_value(),
      "accept-timeout" => settings.accept_timeout.to_value(),
      "handshake-timeout" => settings.handshake_timeout.to_value(),
      "read-timeout" => settings.read_timeout.to_value(),
      "write-timeout" => settings.write_timeout.to_value(),
      "application" => settings.application.to_value(),
      "stream-key" => settings.stream_key.to_value(),
      _ => unimplemented!(),
    }
  }

  fn constructed(&self) {
    self.parent_constructed();

    let source = self.obj();
    source.set_live(true);
    source.set_format(gst::Format::Bytes);
    source.set_do_timestamp(false);
  }
}

impl GstObjectImpl for ScuffleRtmpListenSrc {}

impl ElementImpl for ScuffleRtmpListenSrc {
  fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
    static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
      gst::subclass::ElementMetadata::new(
        "Scuffle RTMP listener source",
        "Source/Network",
        "Accepts one RTMP publisher and outputs an FLV byte stream",
        "Crowdcast",
      )
    });

    Some(&*METADATA)
  }

  fn pad_templates() -> &'static [gst::PadTemplate] {
    static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
      let caps = gst::Caps::builder("video/x-flv").build();
      let source_template = gst::PadTemplate::new(
        "src",
        gst::PadDirection::Src,
        gst::PadPresence::Always,
        &caps,
      )
      .expect("valid source pad template");

      vec![source_template]
    });

    PAD_TEMPLATES.as_ref()
  }
}

impl BaseSrcImpl for ScuffleRtmpListenSrc {
  fn start(&self) -> Result<(), gst::ErrorMessage> {
    let settings = self
      .settings
      .lock()
      .expect("settings mutex poisoned")
      .clone();
    let ip_address = settings.address.parse::<IpAddr>().map_err(|error| {
      gst::error_msg!(
        gst::ResourceError::Settings,
        ["Invalid listener address '{}': {error}", settings.address]
      )
    })?;
    let socket_address = SocketAddr::new(ip_address, settings.port);
    let listener = TcpListener::bind(socket_address).map_err(|error| {
      gst::error_msg!(
        gst::ResourceError::OpenRead,
        ["Failed to bind RTMP listener on {socket_address}: {error}"]
      )
    })?;
    listener.set_nonblocking(true).map_err(|error| {
      gst::error_msg!(
        gst::ResourceError::Settings,
        ["Failed to configure RTMP listener as nonblocking: {error}"]
      )
    })?;

    let local_port = listener
      .local_addr()
      .map_err(|error| {
        gst::error_msg!(
          gst::ResourceError::OpenRead,
          ["Failed to query RTMP listener address: {error}"]
        )
      })?
      .port();

    let (sender, receiver) = flume::bounded(OUTPUT_QUEUE_CAPACITY);
    let cancellation = CancellationToken::new();
    let (startup_sender, startup_receiver) = std::sync::mpsc::sync_channel(1);
    let worker_sender = sender.clone();
    let worker_cancellation = cancellation.clone();
    let worker_settings = settings.clone();
    let worker = std::thread::Builder::new()
      .name("scuffle-rtmp-listener".into())
      .spawn(move || {
        run_worker(
          listener,
          worker_sender,
          worker_cancellation,
          worker_settings,
          startup_sender,
        );
      })
      .map_err(|error| {
        gst::error_msg!(
          gst::ResourceError::OpenRead,
          ["Failed to spawn RTMP listener worker: {error}"]
        )
      })?;

    match startup_receiver.recv_timeout(WORKER_START_TIMEOUT) {
      Ok(Ok(())) => {}
      Ok(Err(error)) => {
        cancellation.cancel();
        let _ = worker.join();
        return Err(gst::error_msg!(gst::ResourceError::OpenRead, ["{error}"]));
      }
      Err(error) => {
        cancellation.cancel();
        let _ = worker.join();
        return Err(gst::error_msg!(
          gst::ResourceError::OpenRead,
          ["RTMP listener worker failed to start: {error}"]
        ));
      }
    }

    {
      let mut state = self.state.lock().expect("state mutex poisoned");
      if state.worker.is_some() {
        cancellation.cancel();
        drop(state);
        let _ = worker.join();
        return Err(gst::error_msg!(
          gst::CoreError::StateChange,
          ["RTMP listener is already running"]
        ));
      }

      state.receiver = Some(receiver);
      state.sender = Some(sender);
      state.cancellation = Some(cancellation);
      state.worker = Some(worker);
    }

    self.flushing.store(false, Ordering::Release);

    if settings.port == 0 {
      self.settings.lock().expect("settings mutex poisoned").port = local_port;
      self.obj().notify("port");
    }

    gst::info!(
      CAT,
      imp = self,
      "Listening for one RTMP publisher on {}:{}",
      settings.address,
      local_port
    );

    Ok(())
  }

  fn stop(&self) -> Result<(), gst::ErrorMessage> {
    gst::info!(CAT, imp = self, "Stopping RTMP listener");
    self.flushing.store(true, Ordering::Release);

    let (sender, cancellation, worker) = {
      let mut state = self.state.lock().expect("state mutex poisoned");
      let sender = state.sender.take();
      let cancellation = state.cancellation.take();
      let worker = state.worker.take();
      state.receiver.take();
      (sender, cancellation, worker)
    };

    if let Some(sender) = sender {
      let _ = sender.try_send(WorkerOutput::Wake);
    }
    if let Some(cancellation) = cancellation {
      cancellation.cancel();
    }
    if let Some(worker) = worker {
      worker.join().map_err(|_| {
        gst::error_msg!(
          gst::ResourceError::Close,
          ["RTMP listener worker panicked during shutdown"]
        )
      })?;
    }

    Ok(())
  }

  fn is_seekable(&self) -> bool {
    false
  }

  fn unlock(&self) -> Result<(), gst::ErrorMessage> {
    self.flushing.store(true, Ordering::Release);
    if let Some(sender) = self
      .state
      .lock()
      .expect("state mutex poisoned")
      .sender
      .as_ref()
    {
      let _ = sender.try_send(WorkerOutput::Wake);
    }
    Ok(())
  }

  fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
    self.flushing.store(false, Ordering::Release);
    Ok(())
  }
}

impl PushSrcImpl for ScuffleRtmpListenSrc {
  fn create(&self, _buffer: Option<&mut gst::BufferRef>) -> Result<CreateSuccess, gst::FlowError> {
    let receiver = self
      .state
      .lock()
      .expect("state mutex poisoned")
      .receiver
      .clone()
      .ok_or(gst::FlowError::Flushing)?;

    loop {
      if self.flushing.load(Ordering::Acquire) {
        return Err(gst::FlowError::Flushing);
      }

      match receiver.recv_timeout(CREATE_POLL_INTERVAL) {
        Ok(WorkerOutput::Data(data)) => {
          return Ok(CreateSuccess::NewBuffer(gst::Buffer::from_mut_slice(data)));
        }
        Ok(WorkerOutput::Warning(warning)) => {
          gst::element_imp_warning!(self, gst::ResourceError::Read, ["{warning}"]);
        }
        Ok(WorkerOutput::Eos) => return Err(gst::FlowError::Eos),
        Ok(WorkerOutput::Error(error)) => {
          gst::element_imp_error!(self, gst::ResourceError::Read, ["{error}"]);
          return Err(gst::FlowError::Error);
        }
        Ok(WorkerOutput::Wake) | Err(flume::RecvTimeoutError::Timeout) => {}
        Err(flume::RecvTimeoutError::Disconnected) => {
          return if self.flushing.load(Ordering::Acquire) {
            Err(gst::FlowError::Flushing)
          } else {
            Err(gst::FlowError::Eos)
          };
        }
      }
    }
  }
}

fn run_worker(
  listener: TcpListener,
  output: flume::Sender<WorkerOutput>,
  cancellation: CancellationToken,
  settings: Settings,
  startup: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
  let runtime = match tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
  {
    Ok(runtime) => runtime,
    Err(error) => {
      let _ = startup.send(Err(format!("Failed to create Tokio runtime: {error}")));
      return;
    }
  };

  runtime.block_on(async move {
    let listener = match tokio::net::TcpListener::from_std(listener) {
      Ok(listener) => listener,
      Err(error) => {
        let _ = startup.send(Err(format!(
          "Failed to initialize Tokio RTMP listener: {error}"
        )));
        return;
      }
    };

    if startup.send(Ok(())).is_err() {
      return;
    }

    let accept = async {
      if settings.accept_timeout == 0 {
        listener.accept().await.map_err(|error| error.to_string())
      } else {
        let timeout = Duration::from_nanos(settings.accept_timeout);
        tokio::time::timeout(timeout, listener.accept())
          .await
          .map_err(|_| format!("Timed out waiting {timeout:?} for an RTMP publisher"))?
          .map_err(|error| error.to_string())
      }
    };
    let accepted = tokio::select! {
      _ = cancellation.cancelled() => return,
      accepted = accept => accepted,
    };
    let (stream, peer_address) = match accepted {
      Ok(accepted) => accepted,
      Err(error) => {
        send_output(
          &output,
          &cancellation,
          WorkerOutput::Error(format!("Failed to accept RTMP publisher: {error}")),
        )
        .await;
        return;
      }
    };
    drop(listener);

    if let Err(error) = stream.set_nodelay(settings.tcp_nodelay) {
      send_output(
        &output,
        &cancellation,
        WorkerOutput::Error(format!("Failed to configure TCP_NODELAY: {error}")),
      )
      .await;
      return;
    }

    gst::info!(
      CAT,
      "Accepted RTMP publisher connection from {peer_address}"
    );

    let handler = RtmpHandler::new(
      output.clone(),
      cancellation.clone(),
      settings.application,
      settings.stream_key,
    );
    let session = ServerSession::new(stream, handler).with_timeouts(ServerSessionTimeouts {
      handshake_read: nanoseconds_timeout(settings.handshake_timeout),
      session_read: nanoseconds_timeout(settings.read_timeout),
      write: nanoseconds_timeout(settings.write_timeout),
    });
    let result = tokio::select! {
      _ = cancellation.cancelled() => return,
      result = session.run() => result,
    };

    match result {
      Ok(true) => {
        gst::info!(CAT, "RTMP publisher completed cleanly");
        send_output(&output, &cancellation, WorkerOutput::Eos).await;
      }
      Ok(false) => {
        send_output(
          &output,
          &cancellation,
          WorkerOutput::Warning("RTMP publisher disconnected without unpublishing".into()),
        )
        .await;
        send_output(&output, &cancellation, WorkerOutput::Eos).await;
      }
      Err(error) => {
        gst::warning!(CAT, "RTMP session failed: {error}");
        send_output(
          &output,
          &cancellation,
          WorkerOutput::Error(format!("RTMP session failed: {error}")),
        )
        .await;
      }
    }
  });
}

fn nanoseconds_timeout(nanoseconds: u64) -> Option<Duration> {
  (nanoseconds != 0).then(|| Duration::from_nanos(nanoseconds))
}

struct RtmpHandler {
  output: flume::Sender<WorkerOutput>,
  cancellation: CancellationToken,
  application: Option<String>,
  stream_key: Option<String>,
  header_sent: bool,
}

impl RtmpHandler {
  fn new(
    output: flume::Sender<WorkerOutput>,
    cancellation: CancellationToken,
    application: Option<String>,
    stream_key: Option<String>,
  ) -> Self {
    Self {
      output,
      cancellation,
      application,
      stream_key,
      header_sent: false,
    }
  }

  async fn ensure_header(&mut self) -> bool {
    if self.header_sent {
      return true;
    }

    self.header_sent = true;
    send_output(
      &self.output,
      &self.cancellation,
      WorkerOutput::Data(FLV_HEADER.to_vec()),
    )
    .await
  }
}

impl SessionHandler for RtmpHandler {
  async fn on_publish(
    &mut self,
    stream_id: u32,
    app_name: &str,
    stream_name: &str,
  ) -> Result<(), ServerSessionError> {
    let app_matches = self
      .application
      .as_deref()
      .is_none_or(|expected| expected == app_name);
    let key_matches = self
      .stream_key
      .as_deref()
      .is_none_or(|expected| expected == stream_name);
    if !app_matches || !key_matches {
      gst::warning!(CAT, "Rejected RTMP publish for stream id {stream_id}");
      send_output(
        &self.output,
        &self.cancellation,
        WorkerOutput::Error(
          "RTMP publisher did not match the configured application and stream key".into(),
        ),
      )
      .await;
      self.cancellation.cancel();
      return Ok(());
    }

    gst::info!(
      CAT,
      "Accepted RTMP publish for app '{app_name}', stream id {stream_id}"
    );
    self.ensure_header().await;
    Ok(())
  }

  async fn on_unpublish(&mut self, stream_id: u32) -> Result<(), ServerSessionError> {
    gst::info!(CAT, "RTMP stream id {stream_id} unpublished");
    Ok(())
  }

  async fn on_data(
    &mut self,
    _stream_id: u32,
    data: SessionData,
  ) -> Result<(), ServerSessionError> {
    if !self.ensure_header().await {
      return Ok(());
    }

    let tag = match data {
      SessionData::Audio { timestamp, data } => frame_flv_tag(FLV_TAG_AUDIO, timestamp, &data),
      SessionData::Video { timestamp, data } => frame_flv_tag(FLV_TAG_VIDEO, timestamp, &data),
      SessionData::Amf0 { timestamp, data } => frame_flv_tag(FLV_TAG_SCRIPT_DATA, timestamp, &data),
    };

    match tag {
      Ok(tag) => {
        send_output(&self.output, &self.cancellation, WorkerOutput::Data(tag)).await;
      }
      Err(error) => {
        send_output(
          &self.output,
          &self.cancellation,
          WorkerOutput::Error(error.to_string()),
        )
        .await;
        self.cancellation.cancel();
      }
    }

    Ok(())
  }
}

async fn send_output(
  sender: &flume::Sender<WorkerOutput>,
  cancellation: &CancellationToken,
  output: WorkerOutput,
) -> bool {
  tokio::select! {
    _ = cancellation.cancelled() => false,
    result = sender.send_async(output) => result.is_ok(),
  }
}

fn frame_flv_tag(tag_type: u8, timestamp: u32, payload: &[u8]) -> io::Result<Vec<u8>> {
  if payload.len() > 0x00ff_ffff {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "RTMP media message exceeds the FLV 24-bit tag-size limit",
    ));
  }
  let payload_len = payload.len() as u32;

  let mut tag = Vec::with_capacity(11 + payload.len() + 4);
  tag.push(tag_type);
  push_u24_be(&mut tag, payload_len);
  push_u24_be(&mut tag, timestamp & 0x00ff_ffff);
  tag.push((timestamp >> 24) as u8);
  push_u24_be(&mut tag, 0);
  tag.extend_from_slice(payload);
  tag.extend_from_slice(&(11 + payload_len).to_be_bytes());
  Ok(tag)
}

fn push_u24_be(output: &mut Vec<u8>, value: u32) {
  output.extend_from_slice(&[
    ((value >> 16) & 0xff) as u8,
    ((value >> 8) & 0xff) as u8,
    (value & 0xff) as u8,
  ]);
}
