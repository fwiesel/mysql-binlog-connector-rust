use crate::binlog_error::BinlogError;
use byteorder::{LittleEndian, ReadBytesExt};
use serde::{Deserialize, Serialize};
use std::io::Cursor;

/// `flags2` bit indicating an 8-byte `commit_id` follows the fixed
/// post-header. Used by MariaDB to group transactions that committed
/// together for parallel replication.
const FL_GROUP_COMMIT_ID: u8 = 0x10;

/// `flags2` bit set on `XA PREPARE` transactions; when present the
/// post-header is followed by `xid_format_id` (4 bytes), then two
/// length-encoded byte strings (`gtrid` + `bqual`). We don't expose
/// these fields yet (TODO) but acknowledging the flag here keeps the
/// `flags` byte's documentation honest.
#[allow(dead_code)]
const FL_PREPARED_XA: u8 = 0x40;

/// MariaDB's `Gtid_log_event` (event type 162). Replaces MySQL's
/// `Gtid_event` on MariaDB; the wire format is unrelated.
///
/// On the wire (after the 19-byte common header):
///   8 bytes: sequence_number (u64 LE)
///   4 bytes: domain_id       (u32 LE)
///   1 byte : flags2
///   [if FL_GROUP_COMMIT_ID & flags2]
///   8 bytes: commit_id       (u64 LE)
///   [if FL_PREPARED_XA   & flags2]   -- not yet parsed, see TODO
///   4 bytes: xid_format_id  (u32 LE)
///   N bytes: lenenc-string  (gtrid)
///   N bytes: lenenc-string  (bqual)
///
/// MariaDB's GTID is conventionally rendered `domain_id-server_id-seq_no`,
/// where `server_id` comes from the *event header* rather than the body.
/// Pass it in via [`MariadbGtidEvent::parse`].
///
/// Note: unlike MySQL's `Gtid_event.last_committed`/`sequence_number`
/// (which reset on every binlog rotation), MariaDB's `sequence_number`
/// here is the GTID's own per-`(domain_id, server_id)` counter and is
/// **lifetime-monotonic** — it never resets while the writer's identity
/// is unchanged. `u64` will not wrap on any realistic horizon.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MariadbGtidEvent {
    /// Per-`(domain_id, server_id)` monotonic counter. Lifetime-stable;
    /// not reset on `Rotate_event`.
    pub sequence_number: u64,
    /// Replication domain. Set by the operator (`gtid_domain_id`); used
    /// to allow several writers to coexist with independent GTID
    /// sequences in the same logical cluster.
    pub domain_id: u32,
    /// The originating server's `server_id`, copied from the binlog
    /// event header for convenience. The on-the-wire body does not
    /// repeat it.
    pub server_id: u32,
    /// Raw `flags2` byte. Mostly internal MariaDB scheduling flags.
    pub flags: u8,
    /// Group-commit identifier, present only when the
    /// [`FL_GROUP_COMMIT_ID`] bit of `flags` is set. Transactions that
    /// share a `commit_id` were committed together on the master and
    /// can be replayed in parallel by the replica.
    pub commit_id: Option<u64>,
    /// Conventional rendering: `"{domain_id}-{server_id}-{seq_no}"`.
    pub gtid: String,
}

impl MariadbGtidEvent {
    pub fn parse(
        cursor: &mut Cursor<&Vec<u8>>,
        server_id: u32,
    ) -> Result<Self, BinlogError> {
        // refer: https://mariadb.com/kb/en/gtid_event/
        let sequence_number = cursor.read_u64::<LittleEndian>()?;
        let domain_id = cursor.read_u32::<LittleEndian>()?;
        let flags = cursor.read_u8()?;

        let commit_id = if flags & FL_GROUP_COMMIT_ID != 0 {
            Some(cursor.read_u64::<LittleEndian>()?)
        } else {
            None
        };

        // TODO: parse the FL_PREPARED_XA tail (`xid_format_id` + lenenc
        // gtrid + lenenc bqual). For now we leave those trailing bytes in
        // the buffer; downstream parsing isn't affected because the cursor
        // is per-event, but consumers running against `XA PREPARE`
        // workloads on MariaDB will not see those fields.

        let gtid = format!("{}-{}-{}", domain_id, server_id, sequence_number);

        Ok(Self {
            sequence_number,
            domain_id,
            server_id,
            flags,
            commit_id,
            gtid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(extra: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(13 + extra.len());
        buf.extend_from_slice(&42u64.to_le_bytes()); // seq_no = 42
        buf.extend_from_slice(&7u32.to_le_bytes());  // domain_id = 7
        buf.push(0);                                 // flags2 = 0
        buf.extend_from_slice(extra);
        buf
    }

    #[test]
    fn parses_minimal_event() {
        let buf = payload(&[]);
        let mut cursor = Cursor::new(&buf);
        let ev = MariadbGtidEvent::parse(&mut cursor, 11).unwrap();
        assert_eq!(ev.sequence_number, 42);
        assert_eq!(ev.domain_id, 7);
        assert_eq!(ev.server_id, 11);
        assert_eq!(ev.flags, 0);
        assert_eq!(ev.commit_id, None);
        assert_eq!(ev.gtid, "7-11-42");
    }

    #[test]
    fn parses_event_with_group_commit_id() {
        let mut buf = Vec::with_capacity(21);
        buf.extend_from_slice(&100u64.to_le_bytes()); // seq_no = 100
        buf.extend_from_slice(&3u32.to_le_bytes());   // domain_id = 3
        buf.push(FL_GROUP_COMMIT_ID);
        buf.extend_from_slice(&999u64.to_le_bytes()); // commit_id = 999
        let mut cursor = Cursor::new(&buf);
        let ev = MariadbGtidEvent::parse(&mut cursor, 5).unwrap();
        assert_eq!(ev.sequence_number, 100);
        assert_eq!(ev.commit_id, Some(999));
        assert_eq!(ev.gtid, "3-5-100");
    }

    #[test]
    fn ignores_unknown_flag_bits_safely() {
        // Set a flag bit other than FL_GROUP_COMMIT_ID; the parser must
        // not try to consume the optional commit_id payload.
        let mut buf = Vec::with_capacity(13);
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.push(0x01); // FL_STANDALONE -- no extra bytes
        let mut cursor = Cursor::new(&buf);
        let ev = MariadbGtidEvent::parse(&mut cursor, 9).unwrap();
        assert_eq!(ev.flags, 0x01);
        assert!(ev.commit_id.is_none());
    }

    /// `FL_PREPARED_XA` (set on `XA PREPARE` transactions) is followed by
    /// `xid_format_id` + lenenc gtrid + lenenc bqual. We don't expose
    /// those fields yet (TODO in `parse`) but the parser must still
    /// succeed and surface the basic GTID identity. Nothing downstream
    /// should panic on a payload that has trailing bytes left over.
    #[test]
    fn xa_prepared_flag_does_not_break_parser() {
        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(&7u64.to_le_bytes()); // seq_no = 7
        buf.extend_from_slice(&2u32.to_le_bytes()); // domain_id = 2
        buf.push(FL_PREPARED_XA);
        // tail bytes the parser intentionally does not consume yet:
        buf.extend_from_slice(&1u32.to_le_bytes()); // xid_format_id
        buf.push(3); // gtrid length (lenenc)
        buf.extend_from_slice(b"gtr"); // gtrid bytes
        buf.push(2); // bqual length
        buf.extend_from_slice(b"bq"); // bqual bytes
        let mut cursor = Cursor::new(&buf);
        let ev = MariadbGtidEvent::parse(&mut cursor, 4).unwrap();
        assert_eq!(ev.flags, FL_PREPARED_XA);
        assert_eq!(ev.gtid, "2-4-7");
        assert!(ev.commit_id.is_none());
    }
}
