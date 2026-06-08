//! Live tests for the commit-time microsecond fields exposed by `Gtid_event`
//! and `Query_event`. Pass against any server reachable via the `db_url`
//! configured in `tests/.env`:
//!
//! - MySQL ≥ 5.7.6 with `gtid_mode=ON` populates the GTID-event tail
//!   (`immediate_commit_timestamp`, `last_committed`, `sequence_number`,
//!   `transaction_length`, `immediate_server_version`).
//! - MariaDB writes the µs fraction in the `Q_HRNOW` Query-event status
//!   var instead. MySQL ≥ 5.7 also writes it as `Q_MICROSECONDS` on the
//!   transaction's `BEGIN`.
//!
//! Tests that require a feature only one engine produces are gated on its
//! presence rather than asserted unconditionally, so the same suite is
//! green on both.

#[cfg(test)]
mod test {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serial_test::serial;

    use crate::runner::{mock::test::Mock, test_runner::test::TestRunner};

    fn now_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64
    }

    /// On MySQL ≥ 5.7.6 with `gtid_mode=ON`, every transaction's
    /// `Gtid_event` carries the master's commit timestamp at microsecond
    /// precision, the logical-clock fields, the transaction length, and
    /// the server version. Verify they round-trip through the parser into
    /// plausible values.
    ///
    /// MariaDB does not emit a MySQL-style `Gtid_event` (it uses its own
    /// `MARIADB_GTID_EVENT`, exposed as `EventData::MariadbGtid` and
    /// covered by the dedicated MariaDB test below), so this test no-ops
    /// on MariaDB.
    #[test]
    #[serial]
    fn gtid_event_carries_commit_timestamps_at_microsecond_precision() {
        let mut runner = TestRunner::new();

        let before = now_us();
        runner.execute_sqls_and_get_binlogs(
            &vec![Mock::default_create_sql()],
            &vec![Mock::insert_sql(&Mock::default_insert_values()[0..1])],
        );
        let after = now_us();

        let Some(gtid) = runner.gtid_events.last() else {
            eprintln!(
                "no Gtid_event captured -- likely MariaDB; skipping MySQL-only assertions"
            );
            return;
        };

        let Some(immediate) = gtid.immediate_commit_timestamp else {
            eprintln!(
                "Gtid_event has no commit-timestamp tail -- likely MySQL < 5.7.6; \
                 skipping microsecond-precision assertions"
            );
            return;
        };
        let original = gtid
            .original_commit_timestamp
            .expect("original_commit_timestamp must be set when immediate is present");

        // The wall-clock window is [before, after]; allow a 60s slack on each
        // side to keep the test green on slow CI machines without losing the
        // sanity check.
        let lo = before.saturating_sub(60_000_000);
        let hi = after.saturating_add(60_000_000);
        assert!(
            (lo..=hi).contains(&immediate),
            "immediate_commit_timestamp {immediate} outside expected window [{lo}, {hi}]"
        );
        assert_eq!(
            immediate, original,
            "single-master commit must mirror the timestamp"
        );

        // The high marker bit must always be cleared by the parser.
        assert_eq!(
            immediate & (1u64 << 55),
            0,
            "marker bit was not stripped from immediate_commit_timestamp"
        );

        // Logical-clock fields are mandatory once the tail is present.
        assert!(gtid.last_committed.is_some());
        assert!(gtid.sequence_number.is_some());

        // Transaction length is recorded in MySQL ≥ 8.0; if absent the
        // server is older, which is fine. When present it should be a sane
        // size (the smallest transaction we can write is well above 1 byte).
        if let Some(len) = gtid.transaction_length {
            assert!(len > 0);
        }

        // Server version reads non-zero on every supported release.
        if let Some(v) = gtid.immediate_server_version {
            assert!(v >= 50600);
        }
    }

    /// Successive transactions produce non-decreasing commit timestamps;
    /// fast back-to-back commits can land in the same microsecond on
    /// modern hardware so we don't require strict `<`. The point of the
    /// test is to catch a byte-order or marker-bit regression that would
    /// scramble adjacent values: `<=` is enough for that.
    ///
    /// Looks at whichever timestamp source the engine populates:
    /// `Gtid_event.immediate_commit_timestamp` on MySQL ≥ 5.7.6, or
    /// `Query_event.microsecond_fraction` (from `Q_HRNOW` / `Q_MICROSECONDS`)
    /// otherwise.
    #[test]
    #[serial]
    fn commit_timestamps_are_monotonic_across_transactions() {
        let mut runner = TestRunner::new();

        let prepare = vec![Mock::default_create_sql()];
        let values = Mock::default_insert_values();
        let test_sqls = vec![
            Mock::insert_sql(&values[0..1]),
            Mock::insert_sql(&values[1..2]),
            Mock::insert_sql(&values[2..3]),
        ];

        runner.execute_sqls_and_get_binlogs(&prepare, &test_sqls);

        let mut timestamps: Vec<u64> = runner
            .gtid_events
            .iter()
            .filter_map(|g| g.immediate_commit_timestamp)
            .collect();

        if timestamps.is_empty() {
            timestamps = runner
                .query_events
                .iter()
                .filter_map(|q| q.microsecond_fraction.map(|us| us as u64))
                .collect();
        }

        if timestamps.len() < 3 {
            eprintln!(
                "expected at least 3 commit timestamps, got {} (gtid={}, query_us={}); skipping",
                timestamps.len(),
                runner.gtid_events.len(),
                runner.query_events.iter().filter(|q| q.microsecond_fraction.is_some()).count(),
            );
            return;
        }
        for w in timestamps.windows(2) {
            assert!(
                w[0] <= w[1],
                "commit timestamps must be non-decreasing across transactions: {} > {}",
                w[0],
                w[1]
            );
        }
    }

    /// Engine-agnostic sanity: every transaction's `BEGIN` carries a µs
    /// fraction (MySQL's `Q_MICROSECONDS` or MariaDB's `Q_HRNOW`), so once
    /// the parser walks the status-vars block at least one captured
    /// `Query_event` should have `microsecond_fraction.is_some()` and a value in
    /// [0, 1_000_000).
    #[test]
    #[serial]
    fn query_event_exposes_microsecond_fraction() {
        let mut runner = TestRunner::new();

        runner.execute_sqls_and_get_binlogs(
            &vec![Mock::default_create_sql()],
            &vec![Mock::insert_sql(&Mock::default_insert_values()[0..1])],
        );

        let with_us: Vec<u32> = runner
            .query_events
            .iter()
            .filter_map(|q| q.microsecond_fraction)
            .collect();

        // Some MySQL releases compress small INSERT transactions into a
        // single Transaction_payload event, in which case the embedded
        // BEGIN never reaches the public Query_event vector. Skip rather
        // than fail in that case.
        if with_us.is_empty() {
            eprintln!(
                "no Query_event with microseconds captured -- likely binlog-transaction-compression; skipping"
            );
            return;
        }
        for us in with_us {
            assert!(
                us < 1_000_000,
                "microsecond fraction must be < 1_000_000, got {us}"
            );
        }
    }

    /// MariaDB-specific: every transaction emits a `Gtid_log_event`
    /// (event type 162) with `(domain_id, server_id, sequence_number)`.
    /// `sequence_number` is lifetime-monotonic per `(domain, server)`
    /// writer (it does **not** reset on `Rotate_event`, unlike MySQL's
    /// similarly-named field). Verify three back-to-back transactions
    /// produce strictly-increasing seq_nos and a well-formed `gtid`
    /// rendering. No-ops on MySQL.
    #[test]
    #[serial]
    fn mariadb_gtid_event_exposes_lifetime_monotonic_seq_no() {
        let mut runner = TestRunner::new();

        let prepare = vec![Mock::default_create_sql()];
        let values = Mock::default_insert_values();
        let test_sqls = vec![
            Mock::insert_sql(&values[0..1]),
            Mock::insert_sql(&values[1..2]),
            Mock::insert_sql(&values[2..3]),
        ];
        runner.execute_sqls_and_get_binlogs(&prepare, &test_sqls);

        if runner.mariadb_gtid_events.is_empty() {
            eprintln!(
                "no MariaDB Gtid_log_event captured -- likely MySQL; skipping MariaDB-only assertions"
            );
            return;
        }

        let seqs: Vec<u64> = runner
            .mariadb_gtid_events
            .iter()
            .map(|e| e.sequence_number)
            .collect();

        for w in seqs.windows(2) {
            assert!(
                w[0] < w[1],
                "MariaDB seq_no must be strictly increasing across transactions: {} >= {}",
                w[0],
                w[1]
            );
        }

        for ev in &runner.mariadb_gtid_events {
            assert_eq!(
                ev.gtid,
                format!("{}-{}-{}", ev.domain_id, ev.server_id, ev.sequence_number),
                "gtid rendering must match {{domain}}-{{server}}-{{seq}}"
            );
            assert!(ev.server_id != 0, "server_id must be propagated from the event header");
        }
    }
}
