use crate::{binlog_error::BinlogError, ext::cursor_ext::CursorExt};
use byteorder::{LittleEndian, ReadBytesExt};
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::io::{Cursor, Read};

/// The 7-byte little-endian commit timestamp carries a marker in bit 55
/// (the high bit of the 7th, most-significant byte); when it is set, the
/// 7 bytes for `original_commit_timestamp` follow this field.
const COMMIT_TIMESTAMP_LENGTH: usize = 7;
const ORIGINAL_COMMIT_TIMESTAMP_FLAG: u8 = 0b1000_0000;
const ORIGINAL_SERVER_VERSION_FLAG: u32 = 0x8000_0000;
const UNDEFINED_SERVER_VERSION: u32 = 0x7fff_ffff;

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct GtidEvent {
    pub flags: u8,
    pub gtid: String,
    /// Logical-clock value used by parallel replication (group commit).
    /// Strictly increasing **within the current binlog file**; resets on
    /// every `Rotate_event`. Pair with the binlog filename, or with
    /// `immediate_commit_timestamp` for wall-clock ordering, if you need
    /// a globally-monotonic key. Only populated by MySQL >= 5.7.6.
    pub last_committed: Option<i64>,
    /// Logical-clock value used by parallel replication. Strictly
    /// increasing within the current binlog file; resets on every
    /// `Rotate_event`. Same caveat as `last_committed`.
    pub sequence_number: Option<i64>,
    /// Microseconds since the Unix epoch.
    pub immediate_commit_timestamp: Option<u64>,
    /// Microseconds since the Unix epoch. When the master is also the origin,
    /// MySQL writes only one timestamp on the wire and we mirror it here.
    pub original_commit_timestamp: Option<u64>,
    pub transaction_length: Option<u64>,
    pub immediate_server_version: Option<u32>,
    pub original_server_version: Option<u32>,
}

impl GtidEvent {
    pub fn parse(cursor: &mut Cursor<&Vec<u8>>) -> Result<Self, BinlogError> {
        // refer: https://dev.mysql.com/doc/refman/8.0/en/replication-gtids-concepts.html
        // refer: https://dev.mysql.com/doc/dev/mysql-server/latest/classbinary__log_1_1Gtid__event.html
        let flags = cursor.read_u8()?;
        let sid = Self::read_uuid(cursor)?;
        let gno = cursor.read_u64::<LittleEndian>()?;
        let mut event = GtidEvent {
            flags,
            gtid: format!("{}:{}", sid, gno),
            ..Default::default()
        };

        // Everything past `gno` was added incrementally to MySQL and is optional;
        // each block is gated on remaining bytes so old masters parse cleanly.
        if cursor.available() < 1 + 8 + 8 {
            return Ok(event);
        }
        let _logical_clock_typecode = cursor.read_u8()?;
        event.last_committed = Some(cursor.read_i64::<LittleEndian>()?);
        event.sequence_number = Some(cursor.read_i64::<LittleEndian>()?);

        if cursor.available() < COMMIT_TIMESTAMP_LENGTH {
            return Ok(event);
        }
        // The marker lives in bit 55 of the 56-bit little-endian value, i.e.
        // the high bit of the most-significant (last) byte.
        let mut buf = [0u8; COMMIT_TIMESTAMP_LENGTH];
        cursor.read_exact(&mut buf)?;
        let last = COMMIT_TIMESTAMP_LENGTH - 1;
        let has_original = buf[last] & ORIGINAL_COMMIT_TIMESTAMP_FLAG != 0;
        buf[last] &= !ORIGINAL_COMMIT_TIMESTAMP_FLAG;
        let ict = u56_le(&buf);
        event.immediate_commit_timestamp = Some(ict);
        event.original_commit_timestamp = Some(if has_original {
            cursor.read_exact(&mut buf)?;
            u56_le(&buf)
        } else {
            ict
        });

        if cursor.available() == 0 {
            return Ok(event);
        }
        event.transaction_length = Some(cursor.read_packed_number()? as u64);

        if cursor.available() < 4 {
            return Ok(event);
        }
        let raw_isv = cursor.read_u32::<LittleEndian>()?;
        let has_original_server_version = raw_isv & ORIGINAL_SERVER_VERSION_FLAG != 0;
        let isv = raw_isv & !ORIGINAL_SERVER_VERSION_FLAG;
        event.immediate_server_version = (isv != UNDEFINED_SERVER_VERSION).then_some(isv);
        event.original_server_version = if has_original_server_version {
            if cursor.available() < 4 {
                None
            } else {
                let osv = cursor.read_u32::<LittleEndian>()? & !ORIGINAL_SERVER_VERSION_FLAG;
                (osv != UNDEFINED_SERVER_VERSION).then_some(osv)
            }
        } else {
            event.immediate_server_version
        };

        Ok(event)
    }

    pub fn read_uuid(cursor: &mut Cursor<&Vec<u8>>) -> Result<String, BinlogError> {
        Ok(format!(
            "{}-{}-{}-{}-{}",
            Self::bytes_to_hex_string(cursor, 4)?,
            Self::bytes_to_hex_string(cursor, 2)?,
            Self::bytes_to_hex_string(cursor, 2)?,
            Self::bytes_to_hex_string(cursor, 2)?,
            Self::bytes_to_hex_string(cursor, 6)?,
        ))
    }

    fn bytes_to_hex_string(
        cursor: &mut Cursor<&Vec<u8>>,
        byte_count: u8,
    ) -> Result<String, BinlogError> {
        let mut res = String::new();
        for _ in 0..byte_count {
            write!(&mut res, "{:02x}", cursor.read_u8()?)?;
        }
        Ok(res)
    }
}

/// Decode a 56-bit little-endian unsigned integer (the wire width MySQL
/// uses for commit timestamps) from a 7-byte buffer. The standard
/// `u64::from_le_bytes` only accepts an 8-byte buffer, so we zero-extend
/// into a fixed-size array first.
fn u56_le(buf: &[u8; COMMIT_TIMESTAMP_LENGTH]) -> u64 {
    let mut wide = [0u8; 8];
    wide[..COMMIT_TIMESTAMP_LENGTH].copy_from_slice(buf);
    u64::from_le_bytes(wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal GTID event payload: flags + 16-byte UUID + 8-byte gno,
    /// followed by `extra`.
    fn payload(extra: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 16 + 8 + extra.len());
        buf.push(0);
        buf.extend_from_slice(&[0u8; 16]);
        buf.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // gno = 1
        buf.extend_from_slice(extra);
        buf
    }

    fn parse(extra: &[u8]) -> GtidEvent {
        let buf = payload(extra);
        let mut cursor = Cursor::new(&buf);
        GtidEvent::parse(&mut cursor).unwrap()
    }

    #[test]
    fn legacy_master_no_tail() {
        let ev = parse(&[]);
        assert_eq!(ev.gtid, "00000000-0000-0000-0000-000000000000:1");
        assert!(ev.last_committed.is_none());
        assert!(ev.immediate_commit_timestamp.is_none());
    }

    #[test]
    fn single_commit_timestamp_implies_original() {
        let mut tail = vec![0u8; 1 + 8 + 8]; // logical clock typecode + last_committed + sequence_number
        // 7 bytes little-endian = 0x123456 microseconds, high bit clear -> single timestamp
        tail.extend_from_slice(&[0x56, 0x34, 0x12, 0, 0, 0, 0]);
        let ev = parse(&tail);
        assert_eq!(ev.immediate_commit_timestamp, Some(0x123456));
        assert_eq!(ev.original_commit_timestamp, Some(0x123456));
    }

    #[test]
    fn dual_commit_timestamps_when_origin_differs() {
        let mut tail = vec![0u8; 1 + 8 + 8];
        // High bit on byte 6 (most significant of the 7-byte little-endian
        // value) marks "an `original_commit_timestamp` follows".
        tail.extend_from_slice(&[0x56, 0x34, 0x12, 0, 0, 0, 0x80]);
        tail.extend_from_slice(&[0x78, 0x56, 0, 0, 0, 0, 0]);
        let ev = parse(&tail);
        assert_eq!(ev.immediate_commit_timestamp, Some(0x123456));
        assert_eq!(ev.original_commit_timestamp, Some(0x5678));
    }

    #[test]
    fn full_tail_with_server_versions() {
        let mut tail = vec![0u8; 1 + 8 + 8];
        tail.extend_from_slice(&[0x56, 0x34, 0x12, 0, 0, 0, 0]); // single ts
        tail.push(42); // transaction_length (lenenc, single byte)
        // immediate_server_version = 80037, no original-version bit -> mirror
        tail.extend_from_slice(&80037u32.to_le_bytes());
        let ev = parse(&tail);
        assert_eq!(ev.transaction_length, Some(42));
        assert_eq!(ev.immediate_server_version, Some(80037));
        assert_eq!(ev.original_server_version, Some(80037));
    }

    #[test]
    fn explicit_original_server_version_when_immediate_differs() {
        // immediate_server_version = 80037 with the high marker bit set
        // ("an original follows") and original_server_version = 50730.
        let mut tail = vec![0u8; 1 + 8 + 8];
        tail.extend_from_slice(&[0x56, 0x34, 0x12, 0, 0, 0, 0]); // single ts
        tail.push(42); // transaction_length
        tail.extend_from_slice(&(80037u32 | ORIGINAL_SERVER_VERSION_FLAG).to_le_bytes());
        tail.extend_from_slice(&50730u32.to_le_bytes());
        let ev = parse(&tail);
        assert_eq!(ev.immediate_server_version, Some(80037));
        assert_eq!(ev.original_server_version, Some(50730));
    }

    #[test]
    fn undefined_server_version_is_normalised_to_none() {
        let mut tail = vec![0u8; 1 + 8 + 8];
        tail.extend_from_slice(&[0x56, 0x34, 0x12, 0, 0, 0, 0]); // single ts
        tail.push(42); // transaction_length
        tail.extend_from_slice(&UNDEFINED_SERVER_VERSION.to_le_bytes());
        let ev = parse(&tail);
        assert!(ev.immediate_server_version.is_none());
        assert!(ev.original_server_version.is_none());
    }
}
