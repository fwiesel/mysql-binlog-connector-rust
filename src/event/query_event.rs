use std::io::{Cursor, Read, Seek, SeekFrom};

use byteorder::{LittleEndian, ReadBytesExt};
use serde::{Deserialize, Serialize};

use crate::{binlog_error::BinlogError, ext::cursor_ext::CursorExt};

/// Microsecond fraction of the statement's start time.
/// `Q_MICROSECONDS` is emitted by MySQL ≥ 5.7; `Q_HRNOW` by MariaDB.
const Q_MICROSECONDS_CODE: u8 = 0x0d;
const Q_HRNOW_CODE: u8 = 0x80;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct QueryEvent {
    pub thread_id: u32,
    pub exec_time: u32,
    pub error_code: u16,
    pub schema: String,
    pub query: String,
    /// Microsecond fraction (0..999_999) of the master wall clock at
    /// statement start, when the master emits it. **This is a fraction
    /// of a second, not microseconds since the Unix epoch** — combine it
    /// with `EventHeader::timestamp` (seconds since epoch) for full
    /// microsecond precision.
    pub microsecond_fraction: Option<u32>,
}

impl QueryEvent {
    pub fn parse(cursor: &mut Cursor<&Vec<u8>>) -> Result<Self, BinlogError> {
        // refer: https://dev.mysql.com/doc/dev/mysql-server/latest/classbinary__log_1_1Query__event.html
        // Post-Header for Query_event
        let thread_id = cursor.read_u32::<LittleEndian>()?;
        let exec_time = cursor.read_u32::<LittleEndian>()?;
        let schema_length = cursor.read_u8()?;
        let error_code = cursor.read_u16::<LittleEndian>()?;
        let status_vars_length = cursor.read_u16::<LittleEndian>()? as usize;

        let microsecond_fraction = parse_status_vars(cursor, status_vars_length)?;

        // Format: schema_length + 1, The currently selected database, as a null-terminated string.
        let schema = cursor.read_string_without_terminator(schema_length as usize)?;
        let mut query = String::new();
        cursor.read_to_string(&mut query)?;

        Ok(Self {
            thread_id,
            exec_time,
            error_code,
            schema,
            query,
            microsecond_fraction,
        })
    }
}

/// Walk the `Query_event` status-vars TLV block. Each entry is a 1-byte tag
/// followed by a fixed-or-variable length payload; tag layout is documented
/// at https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_replication.html#sect_protocol_replication_event_query
/// and (for MariaDB) https://mariadb.com/kb/en/query_event/.
///
/// We expose only what we need (the µs-precision wall clock fraction). On
/// any tag we don't know how to size, we bail out and seek to the end of the
/// block so the rest of the event still parses; this matches the defensive
/// stance the reference `mysql-binlog-connector-java` library takes.
fn parse_status_vars(
    cursor: &mut Cursor<&Vec<u8>>,
    total_len: usize,
) -> Result<Option<u32>, BinlogError> {
    let end = cursor.position() + total_len as u64;
    let mut microsecond_fraction = None;

    while cursor.position() < end {
        let tag = cursor.read_u8()?;
        let payload_len = match tag {
            0x00 => 4, // Q_FLAGS2_CODE
            0x01 => 8, // Q_SQL_MODE_CODE
            0x02 => {
                // Q_CATALOG_CODE: lenenc-string + null terminator (deprecated)
                let n = cursor.read_u8()? as i64;
                cursor.seek(SeekFrom::Current(n + 1))?;
                continue;
            }
            0x03 => 4, // Q_AUTO_INCREMENT
            0x04 => 6, // Q_CHARSET_CODE
            0x05 => {
                // Q_TIME_ZONE_CODE: lenenc-string
                let n = cursor.read_u8()? as i64;
                cursor.seek(SeekFrom::Current(n))?;
                continue;
            }
            0x06 => {
                // Q_CATALOG_NZ_CODE
                let n = cursor.read_u8()? as i64;
                cursor.seek(SeekFrom::Current(n))?;
                continue;
            }
            0x07 => 2,  // Q_LC_TIME_NAMES_CODE
            0x08 => 2,  // Q_CHARSET_DATABASE_CODE
            0x09 => 8,  // Q_TABLE_MAP_FOR_UPDATE_CODE
            0x0a => 4,  // Q_MASTER_DATA_WRITTEN_CODE
            0x0b => {
                // Q_INVOKER: two `<lenenc-int> <bytes>` strings (user + host).
                // The size prefix is MySQL's `net_store_length` lenenc-int —
                // exactly what `read_packed_number` decodes (0..250 raw,
                // 0xfc+u16, 0xfd+u24, 0xfe+u64). Replacing it with a bare
                // `read_u8` would silently misread any name with a 0xfb-0xfe
                // marker as its first byte; mathematically possible, vanishingly
                // rare in practice, but worth honouring the spec.
                for _ in 0..2 {
                    let n = cursor.read_packed_number()? as i64;
                    cursor.seek(SeekFrom::Current(n))?;
                }
                continue;
            }
            0x0c => {
                // Q_UPDATED_DB_NAMES: 1-byte count + N null-terminated strings.
                // The sentinel value 254 means "list omitted, server hit
                // OVER_MAX_DBS_IN_EVENT_MTS"; in that case nothing follows.
                let count = cursor.read_u8()?;
                if count != 254 {
                    for _ in 0..count {
                        let _ = cursor.read_null_terminated_string()?;
                    }
                }
                continue;
            }
            Q_MICROSECONDS_CODE | Q_HRNOW_CODE => {
                microsecond_fraction = Some(cursor.read_u24::<LittleEndian>()?);
                continue;
            }
            0x10 => 1, // Q_EXPLICIT_DEFAULTS_FOR_TIMESTAMP
            0x11 => 8, // Q_DDL_LOGGED_WITH_XID
            0x12 => 2, // Q_DEFAULT_COLLATION_FOR_UTF8MB4
            0x13 => 1, // Q_SQL_REQUIRE_PRIMARY_KEY
            0x14 => 1, // Q_DEFAULT_TABLE_ENCRYPTION
            _ => {
                // Unknown tag: we don't know its payload size, so skip to
                // the end of the block to keep the cursor aligned for the
                // schema/query that follow. We rely on the order in which
                // MySQL writes the status vars — `Q_MICROSECONDS` comes
                // early — so this is safe today; if a future server inserts
                // a new tag *before* it, we'd silently lose the fraction.
                cursor.set_position(end);
                break;
            }
        };
        cursor.seek(SeekFrom::Current(payload_len))?;
    }

    cursor.set_position(end);
    Ok(microsecond_fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_query(status_vars: Vec<u8>, schema: &str, query: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&7u32.to_le_bytes()); // thread_id
        buf.extend_from_slice(&0u32.to_le_bytes()); // exec_time
        buf.push(schema.len() as u8);               // schema length
        buf.extend_from_slice(&0u16.to_le_bytes()); // error_code
        buf.extend_from_slice(&(status_vars.len() as u16).to_le_bytes());
        buf.extend(status_vars);
        buf.extend_from_slice(schema.as_bytes());
        buf.push(0); // null terminator
        buf.extend_from_slice(query.as_bytes());
        buf
    }

    fn parse(buf: Vec<u8>) -> QueryEvent {
        let mut cursor = Cursor::new(&buf);
        QueryEvent::parse(&mut cursor).unwrap()
    }

    #[test]
    fn no_status_vars() {
        let ev = parse(build_query(vec![], "db", "BEGIN"));
        assert_eq!(ev.thread_id, 7);
        assert_eq!(ev.schema, "db");
        assert_eq!(ev.query, "BEGIN");
        assert!(ev.microsecond_fraction.is_none());
    }

    #[test]
    fn mysql_q_microseconds() {
        let mut sv = vec![0x00, 1, 0, 0, 0]; // Q_FLAGS2 = 1, ignored
        sv.push(Q_MICROSECONDS_CODE);
        sv.extend_from_slice(&[0x40, 0xe2, 0x01]); // 123456 little-endian u24
        let ev = parse(build_query(sv, "nova", "BEGIN"));
        assert_eq!(ev.microsecond_fraction, Some(123_456));
        assert_eq!(ev.schema, "nova");
        assert_eq!(ev.query, "BEGIN");
    }

    #[test]
    fn mariadb_q_hrnow() {
        let mut sv = vec![Q_HRNOW_CODE];
        sv.extend_from_slice(&[0x40, 0xe2, 0x01]);
        let ev = parse(build_query(sv, "x", "COMMIT"));
        assert_eq!(ev.microsecond_fraction, Some(123_456));
    }

    #[test]
    fn unknown_tag_skips_block_cleanly() {
        let sv = vec![0x7f, 0xaa, 0xbb, 0xcc]; // unknown tag, then noise
        let ev = parse(build_query(sv, "db", "x"));
        assert_eq!(ev.schema, "db");
        assert_eq!(ev.query, "x");
        assert!(ev.microsecond_fraction.is_none());
    }
}
