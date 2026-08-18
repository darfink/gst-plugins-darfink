// SPDX-License-Identifier: MPL-2.0

//! AMF0 serialization for FLV script-data subtitle messages.
//!
//! The shape here is not a design choice. It is dictated by FFmpeg's
//! `flv_data_packet()` in `libavformat/flvdec.c`, which is the reader every
//! consumer of this stream ultimately goes through:
//!
//! * the message name must be `onTextData`, `onCaption`, or `onCaptionInfo`;
//! * the payload must be an ECMA mixed array, an object, or a strict array;
//! * within a mixed array or object, the cue text lives in a property literally
//!   named `text`, holding an AMF0 string.
//!
//! Two details of that reader constrain what may be written here:
//!
//! * property names are read into a `char buf[20]`, so a name of 20 bytes or
//!   more desynchronizes the parse. Nothing this module writes comes close, but
//!   the limit is why arbitrary caller-supplied properties are not offered.
//! * FFmpeg stops at the *first* `text` property it finds, so exactly one is
//!   written.

/// AMF0 type markers, from the AMF0 specification.
mod marker {
  pub const NUMBER: u8 = 0x00;
  pub const STRING: u8 = 0x02;
  pub const MIXED_ARRAY: u8 = 0x08;
  pub const OBJECT_END: u8 = 0x09;
}

/// The longest UTF-8 string an AMF0 short string can carry.
///
/// Longer text would need the AMF0 *long* string type (0x0C), which FFmpeg's
/// `flv_data_packet` does not accept for the `text` property: it reads a 16-bit
/// length unconditionally. Callers truncate rather than emit an unreadable tag.
pub const MAXIMUM_TEXT_BYTES: usize = u16::MAX as usize;

/// The script-data message name carrying a cue.
///
/// Both variants reach the identical branch of FFmpeg's demuxer and produce an
/// `AV_CODEC_ID_TEXT` subtitle stream. They differ only in log noise, which is
/// why the default is not the more familiar name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MessageName {
  /// Reaches `flv_data_packet` without comment.
  #[default]
  OnCaption,
  /// Identical handling, but FFmpeg calls `avpriv_request_sample()` first and
  /// logs "OnTextData packet is not implemented" once per cue before decoding
  /// it correctly. Offered for consumers that match on the name.
  OnTextData,
}

impl MessageName {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::OnCaption => "onCaption",
      Self::OnTextData => "onTextData",
    }
  }
}

fn put_string_body(out: &mut Vec<u8>, value: &str) {
  let bytes = value.as_bytes();
  let length = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
  out.extend_from_slice(&length.to_be_bytes());
  out.extend_from_slice(&bytes[..length as usize]);
}

fn put_string(out: &mut Vec<u8>, value: &str) {
  out.push(marker::STRING);
  put_string_body(out, value);
}

fn put_property_name(out: &mut Vec<u8>, name: &str) {
  put_string_body(out, name);
}

/// Serialize the AMF0 body of a script-data subtitle message.
///
/// The result is the tag *body* only: the FLV tag header framing it belongs to
/// the caller, which is the only party that knows the stream's timestamps.
///
/// `text` is truncated to [`MAXIMUM_TEXT_BYTES`] on a UTF-8 character boundary.
/// An empty `text` is meaningful and is written as such: it is how this
/// transport expresses "clear the display", mirroring the window erase that
/// CEA-708 performs with `clear_current_window()`.
pub fn script_data_body(name: MessageName, text: &str, duration_ms: Option<u32>) -> Vec<u8> {
  let text = truncate_on_character_boundary(text, MAXIMUM_TEXT_BYTES);

  let mut out = Vec::with_capacity(64 + text.len());
  put_string(&mut out, name.as_str());

  // A mixed array rather than an object: this is what Wowza and the wider FLV
  // caption ecosystem emit, and FFmpeg accepts it on the same path.
  out.push(marker::MIXED_ARRAY);
  let property_count = 1 + u32::from(duration_ms.is_some());
  out.extend_from_slice(&property_count.to_be_bytes());

  // `text` is written first so a reader scanning for it stops immediately.
  put_property_name(&mut out, "text");
  put_string(&mut out, text);

  // An advisory end time for readers that want one. FFmpeg ignores it and
  // resolves a cue's end from its successor, which is the behaviour this
  // element is designed around; it is written only for consumers that can use
  // it, and never as a substitute for emitting a successor cue.
  if let Some(duration_ms) = duration_ms {
    put_property_name(&mut out, "duration");
    out.push(marker::NUMBER);
    out.extend_from_slice(&f64::from(duration_ms).to_be_bytes());
  }

  // Object end marker: an empty property name followed by the end type.
  out.extend_from_slice(&[0x00, 0x00, marker::OBJECT_END]);
  out
}

fn truncate_on_character_boundary(text: &str, limit: usize) -> &str {
  if text.len() <= limit {
    return text;
  }
  let mut end = limit;
  while end > 0 && !text.is_char_boundary(end) {
    end -= 1;
  }
  &text[..end]
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Walk a serialized body the way FFmpeg's reader does, returning the text.
  fn read_text_property(body: &[u8]) -> Option<String> {
    let mut cursor = 0usize;

    assert_eq!(body[cursor], marker::STRING);
    cursor += 1;
    let name_length = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2 + name_length;

    assert_eq!(body[cursor], marker::MIXED_ARRAY);
    cursor += 1 + 4;

    loop {
      let property_length = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
      cursor += 2;
      if property_length == 0 {
        return None;
      }
      let property = std::str::from_utf8(&body[cursor..cursor + property_length]).unwrap();
      cursor += property_length;
      let kind = body[cursor];
      cursor += 1;
      match kind {
        marker::STRING => {
          let length = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
          cursor += 2;
          let value = std::str::from_utf8(&body[cursor..cursor + length]).unwrap();
          cursor += length;
          if property == "text" {
            return Some(value.to_owned());
          }
        }
        marker::NUMBER => cursor += 8,
        other => panic!("unexpected AMF0 type {other:#x}"),
      }
    }
  }

  #[test]
  fn message_name_is_the_first_amf0_string() {
    let body = script_data_body(MessageName::OnCaption, "hello", None);
    assert_eq!(body[0], marker::STRING);
    assert_eq!(&body[3..3 + 9], b"onCaption");
  }

  #[test]
  fn text_property_round_trips() {
    let body = script_data_body(MessageName::OnCaption, "hello world", None);
    assert_eq!(read_text_property(&body).as_deref(), Some("hello world"));
  }

  #[test]
  fn an_empty_cue_is_written_rather_than_omitted() {
    // This is the clear-display signal; dropping it would leave the previous
    // caption on screen forever, since cues end only when replaced.
    let body = script_data_body(MessageName::OnCaption, "", None);
    assert_eq!(read_text_property(&body).as_deref(), Some(""));
  }

  #[test]
  fn text_precedes_duration_so_a_reader_stops_early() {
    let body = script_data_body(MessageName::OnCaption, "cue", Some(1_000));
    assert_eq!(read_text_property(&body).as_deref(), Some("cue"));
  }

  #[test]
  fn multibyte_text_truncates_on_a_character_boundary() {
    let text = "é".repeat(MAXIMUM_TEXT_BYTES);
    let body = script_data_body(MessageName::OnCaption, &text, None);
    let recovered = read_text_property(&body).expect("text property");
    assert!(recovered.len() <= MAXIMUM_TEXT_BYTES);
    assert!(recovered.chars().all(|character| character == 'é'));
  }
}
