//! The guestd ⇄ hostd vsock protocol.
//!
//! Newline-delimited JSON over a per-VM vsock CID on a fixed port. The two
//! enums are split by **direction** so an illegal message is unrepresentable:
//! a guest cannot construct a [`HostToGuest::Secrets`], and hostd cannot forge
//! a [`GuestToHost::TurnStarted`]. Both enums are **internally tagged** with a
//! `type` field, so every message is a flat JSON object — `{"type":"Ping"}`,
//! `{"type":"TurnStarted","turn_id":7,"ts":..}` — directly readable and writable
//! by a non-Rust guest (O8).
//!
//! Mirrors the message table in `docs/protocol.md`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ids::{GuestdVersion, Timestamp, TurnId, VmId};

/// Messages the guestd sends to hostd.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GuestToHost {
    /// Boot handshake — the first message after the guest comes up. Lets hostd
    /// bind this vsock connection to a [`VmId`] and check the guest's
    /// [`GuestdVersion`] against [`PROTOCOL_VERSION`][crate::PROTOCOL_VERSION].
    Hello {
        /// Which VM this guest is.
        vm_id: VmId,
        /// The guestd build version.
        guestd_version: GuestdVersion,
    },

    /// Ground-truth busy signal: a turn has started. From this instant the VM
    /// is non-quiescent at the app layer.
    TurnStarted {
        /// The turn that began.
        turn_id: TurnId,
        /// When the guest observed the start.
        ts: Timestamp,
    },

    /// Ground-truth idle signal: the turn finished. The app-layer quiescence
    /// gate can only close once the in-flight turn has ended.
    TurnEnded {
        /// The turn that ended.
        turn_id: TurnId,
        /// When the guest observed the end.
        ts: Timestamp,
    },

    /// Response to a [`HostToGuest::DrainRequest`]. `in_flight: None` means new
    /// turns are gated *and* none is running — the app layer is quiescent.
    /// `Some(turn)` means that turn must finish (or the drain time out) first.
    DrainAck {
        /// The turn still running, if any.
        in_flight: Option<TurnId>,
    },

    /// First message after a restore on the target host. Doubles as the trigger
    /// for guest clock fix-up.
    Resumed {
        /// The guest's wall clock at resume, *before* fix-up.
        ts: Timestamp,
    },

    /// Liveness probe.
    Ping,
    /// Liveness reply to a [`HostToGuest::Ping`].
    Pong,
}

/// Messages hostd sends to the guestd.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostToGuest {
    /// API-key/secret handoff at boot. Delivered over vsock precisely so it is
    /// never baked into the rootfs or the kernel cmdline (ADR-005). The
    /// guestd sets these in the harness env and execs it.
    Secrets {
        /// Environment variables to inject. `BTreeMap` for a deterministic wire
        /// order (stable round-trips, reproducible transcripts).
        env: BTreeMap<String, String>,
    },

    /// Ask the guest to gate new turns and report what is in flight. The guest
    /// has `deadline` to ack; on the wire this is the integer field
    /// `deadline_ms`.
    DrainRequest {
        /// How long hostd will wait for an in-flight turn before aborting the
        /// migration back to `Stable`.
        #[serde(rename = "deadline_ms", with = "crate::wire::millis")]
        deadline: Duration,
    },

    /// Migration aborted — un-gate, release any queued turns. Sent when a drain
    /// times out or the rebalancer cancels before snapshotting.
    DrainCancel,

    /// Liveness probe.
    Ping,
    /// Liveness reply to a [`GuestToHost::Ping`].
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every guest→host message survives a JSON round-trip byte-identically.
    #[test]
    fn guest_to_host_round_trips() {
        let cases = [
            GuestToHost::Hello {
                vm_id: VmId::from_uuid(uuid::Uuid::nil()),
                guestd_version: GuestdVersion::new("0.1.0"),
            },
            GuestToHost::TurnStarted {
                turn_id: TurnId::from_u64(7),
                ts: Timestamp::from_nanos(1_700_000_000_000_000_000),
            },
            GuestToHost::TurnEnded {
                turn_id: TurnId::from_u64(7),
                ts: Timestamp::from_nanos(1_700_000_000_500_000_000),
            },
            GuestToHost::DrainAck { in_flight: None },
            GuestToHost::DrainAck {
                in_flight: Some(TurnId::from_u64(7)),
            },
            GuestToHost::Resumed {
                ts: Timestamp::from_nanos(1),
            },
            GuestToHost::Ping,
            GuestToHost::Pong,
        ];
        for msg in cases {
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: GuestToHost = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(msg, back, "round-trip mismatch for {json}");
        }
    }

    /// Every host→guest message survives a JSON round-trip.
    #[test]
    fn host_to_guest_round_trips() {
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".to_owned(), "sk-redacted".to_owned());
        let cases = [
            HostToGuest::Secrets { env },
            HostToGuest::DrainRequest {
                deadline: Duration::from_millis(5_000),
            },
            HostToGuest::DrainCancel,
            HostToGuest::Ping,
            HostToGuest::Pong,
        ];
        for msg in cases {
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: HostToGuest = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(msg, back, "round-trip mismatch for {json}");
        }
    }

    /// Every message is a flat, internally-tagged object: a `type` field plus the
    /// variant's fields, never a `{"Variant":{..}}` wrapper.
    #[test]
    fn messages_are_internally_tagged() {
        let hello = serde_json::to_string(&GuestToHost::Hello {
            vm_id: VmId::from_uuid(uuid::Uuid::nil()),
            guestd_version: GuestdVersion::new("0.1.0"),
        })
        .expect("serialize");
        assert_eq!(
            hello,
            r#"{"type":"Hello","vm_id":"00000000-0000-0000-0000-000000000000","guestd_version":"0.1.0"}"#
        );
        // A fieldless message is still a typed object, not a bare string.
        assert_eq!(
            serde_json::to_string(&GuestToHost::Ping).expect("serialize"),
            r#"{"type":"Ping"}"#
        );
    }

    /// `in_flight: None` must serialize as JSON `null`, not be omitted — the
    /// distinction (gated-and-idle vs. field-absent) is load-bearing for the
    /// quiescence gate, so it stays explicit on the wire.
    #[test]
    fn drain_ack_none_is_explicit_null() {
        let json =
            serde_json::to_string(&GuestToHost::DrainAck { in_flight: None }).expect("serialize");
        assert_eq!(json, r#"{"type":"DrainAck","in_flight":null}"#);
    }

    /// A `Duration` deadline crosses the wire as the integer `deadline_ms`.
    #[test]
    fn drain_request_deadline_is_integer_millis() {
        let json = serde_json::to_string(&HostToGuest::DrainRequest {
            deadline: Duration::from_millis(250),
        })
        .expect("serialize");
        assert_eq!(json, r#"{"type":"DrainRequest","deadline_ms":250}"#);
    }
}
