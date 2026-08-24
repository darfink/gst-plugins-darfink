# Local changes

This is `scuffle-rtmp` 0.2.3 from crates.io, licensed under MIT or
Apache-2.0. It is kept locally so the GStreamer listener can configure network
timeouts that are hardcoded upstream.

The local patch adds `ServerSessionTimeouts` and
`ServerSession::with_timeouts`. The defaults on that type retain upstream's
two-second handshake-read timeout, 2.5-second session-read timeout, and
two-second write timeout. Supplying `None` disables an individual timeout.

It also adds `ServerSession::run_with_shutdown`, which can send
`NetConnection.Connect.Closed` and wait for a publisher to close before the
server drops the connection.

The chunk reader also retains the preceding timestamp delta per chunk stream.
When a type-3 chunk starts a new message it advances the timestamp by that
delta, while continuation chunks retain the current message timestamp. RTMP
timestamp arithmetic wraps at 32 bits.

The server advertises FFmpeg-compatible 2,500,000-byte acknowledgement and
peer-bandwidth windows. Acknowledgements report the sequence number after the
boundary-crossing read, and a zero window from a peer is rejected.

Command replies carry an explicit RTMP message stream id so NetStream
`onStatus` events land on the publishing stream. Clients such as GStreamer's
`rtmp2sink` wait for `NetStream.Publish.Start` on that stream and ignore the
same status when it arrives on NetConnection stream 0.

`deleteStream` also tolerates a non-numeric stream argument. Upstream expects
the Adobe stream id, but `rtmp2sink` sends the stream name; the reader treats
that as unrecognized and the session resolves it against the streams this
connection publishes instead of dropping a healthy publisher.
