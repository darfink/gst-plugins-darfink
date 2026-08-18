// SPDX-License-Identifier: MPL-2.0

//! FLV tag framing and just enough header parsing to splice into a tag stream.
//!
//! This module deliberately does not parse tag *bodies*. The element needs to
//! answer exactly one question about the muxed stream flowing through it —
//! "which timestamp has the muxer reached?" — and that is in the 11-byte tag
//! header. Everything else is forwarded byte-for-byte.

/// FLV tag header length: type, size, timestamp, extended timestamp, stream id.
pub const TAG_HEADER_LEN: usize = 11;

/// Size of the `PreviousTagSize` field that follows every tag.
pub const PREVIOUS_TAG_SIZE_LEN: usize = 4;

/// `TagType` for script data (AMF0 metadata), from the FLV specification.
pub const TAG_TYPE_SCRIPT_DATA: u8 = 18;

/// The largest timestamp an FLV tag can express.
///
/// 24 bits of timestamp plus an 8-bit extension, treated as a signed 32-bit
/// value by every reader in practice.
pub const MAXIMUM_TIMESTAMP_MS: u64 = i32::MAX as u64;

/// A parsed FLV tag header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TagHeader {
  pub tag_type: u8,
  pub body_size: usize,
  pub timestamp_ms: u32,
}

impl TagHeader {
  /// Total bytes this tag occupies, including header and trailing size field.
  pub fn total_len(self) -> usize {
    TAG_HEADER_LEN + self.body_size + PREVIOUS_TAG_SIZE_LEN
  }
}

/// Parse a tag header from the front of `data`, if enough bytes are present.
pub fn parse_tag_header(data: &[u8]) -> Option<TagHeader> {
  if data.len() < TAG_HEADER_LEN {
    return None;
  }

  // The high two bits of the type byte are the filter and reserved flags.
  let tag_type = data[0] & 0x1F;
  let body_size = u32::from_be_bytes([0, data[1], data[2], data[3]]) as usize;
  let lower = u32::from_be_bytes([0, data[4], data[5], data[6]]);
  let timestamp_ms = (u32::from(data[7]) << 24) | lower;

  Some(TagHeader {
    tag_type,
    body_size,
    timestamp_ms,
  })
}

/// Frame an already-serialized script-data body as a complete FLV tag.
///
/// The returned bytes are self-contained and position-independent: an FLV body
/// is a flat sequence of such tags, which is precisely what makes splicing one
/// in downstream of a muxer sound.
pub fn script_data_tag(timestamp_ms: u32, body: &[u8]) -> Vec<u8> {
  let mut tag = Vec::with_capacity(TAG_HEADER_LEN + body.len() + PREVIOUS_TAG_SIZE_LEN);

  tag.push(TAG_TYPE_SCRIPT_DATA);

  let body_size = u32::try_from(body.len()).unwrap_or(u32::MAX);
  tag.extend_from_slice(&body_size.to_be_bytes()[1..]);

  // Timestamps split as UI24 lower bits followed by a separate high byte.
  tag.extend_from_slice(&timestamp_ms.to_be_bytes()[1..]);
  tag.push((timestamp_ms >> 24) as u8);

  // StreamID is always zero.
  tag.extend_from_slice(&[0, 0, 0]);

  tag.extend_from_slice(body);

  let tag_len = u32::try_from(TAG_HEADER_LEN + body.len()).unwrap_or(u32::MAX);
  tag.extend_from_slice(&tag_len.to_be_bytes());
  tag
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_framed_tag_parses_back_to_its_inputs() {
    let body = b"body bytes".as_slice();
    let tag = script_data_tag(1_234, body);

    let header = parse_tag_header(&tag).expect("header");
    assert_eq!(header.tag_type, TAG_TYPE_SCRIPT_DATA);
    assert_eq!(header.body_size, body.len());
    assert_eq!(header.timestamp_ms, 1_234);
    assert_eq!(header.total_len(), tag.len());
  }

  #[test]
  fn the_trailing_size_field_excludes_itself() {
    let body = b"x".as_slice();
    let tag = script_data_tag(0, body);
    let trailer = u32::from_be_bytes(tag[tag.len() - 4..].try_into().unwrap()) as usize;
    assert_eq!(trailer, TAG_HEADER_LEN + body.len());
  }

  #[test]
  fn timestamps_above_the_ui24_range_use_the_extension_byte() {
    let timestamp = 0x0100_0001;
    let tag = script_data_tag(timestamp, b"x");
    assert_eq!(tag[7], 0x01, "extension byte carries the high bits");
    assert_eq!(parse_tag_header(&tag).unwrap().timestamp_ms, timestamp);
  }

  #[test]
  fn a_short_buffer_yields_no_header() {
    assert!(parse_tag_header(&[0u8; TAG_HEADER_LEN - 1]).is_none());
  }
}
