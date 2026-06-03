// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Asymmetric Logical Unit Access (SPC-4 §5.16) topology used by both
//! products.
//!
//! Multi-portal advertisement (one iSCSI portal per [`Portal`]) plus
//! per-portal TPGT gives the daemon a real Target Port topology. The
//! ALUA surface lifts that topology into the SCSI layer so a Linux
//! initiator's `dm-multipath` ALUA policy can drive path priority
//! automatically instead of falling back to active/passive.
//!
//! Surface:
//!
//! - [`AluaTopology::from_portals`] builds a topology snapshot from
//!   the running `ServerConfig::listen_portals`: sequential Relative
//!   Target Port Identifiers (RTPIs), distinct per-TPG entries, and
//!   a stable per-port NAA-3 identifier derived from a daemon-supplied
//!   namespace (chassis serial for VTL, target IQN for VSA).
//! - [`AluaTopology::push_vpd83_target_port_descriptors`] appends one
//!   set of `Association=TargetPort` designators (NAA-3 +
//!   RelativeTargetPort + TargetPortGroup) per advertised portal to a
//!   VPD 0x83 descriptor buffer.
//! - [`AluaTopology::report_target_port_groups_body`] assembles the
//!   REPORT TARGET PORT GROUPS (MAINTENANCE IN service action 0x0A)
//!   response body — one TPG descriptor per distinct TPGT, each
//!   carrying that TPG's asymmetric access state plus the RTPIs of
//!   the member ports.
//!
//! First-cut scope: implicit ALUA only. Every TPG defaults to
//! [`AsymmetricAccessState::ActiveOptimized`] (no operator action
//! needed — out of the box every path is usable) and there is no
//! SET TARGET PORT GROUPS surface yet. The state field is stored
//! behind an `RwLock` so a later commit can wire SET TPG without
//! re-plumbing the topology object.

use std::collections::BTreeMap;
use std::sync::RwLock;

use scsi_spc::naa::naa3_target_port;
use scsi_spc::vpd::{Association, CodeSet, DesignatorType, push_designator};

use crate::transport::Portal;

/// SPC-4 §6.27.7 Table 364 ASYMMETRIC ACCESS STATE. Encoded in
/// REPORT TARGET PORT GROUPS byte 4 bits 3:0 of each TPG descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AsymmetricAccessState {
    /// 0x0 — Active/Optimized. Best path; dm-multipath prefers it.
    ActiveOptimized = 0x0,
    /// 0x1 — Active/Non-Optimized. Usable but lower priority; used as
    /// the alternate-controller path in classic two-controller arrays.
    ActiveNonOptimized = 0x1,
    /// 0x2 — Standby. Reachable but not currently servicing I/O;
    /// dm-multipath marks paths in this state as standby.
    Standby = 0x2,
    /// 0x3 — Unavailable. Port reachable but the LU isn't behind it;
    /// dm-multipath fails the path.
    Unavailable = 0x3,
    /// 0xE — Offline. Port unreachable.
    Offline = 0xE,
}

impl AsymmetricAccessState {
    /// The byte value REPORT TPG writes into the TPG descriptor's
    /// access-state field (byte 0 of each descriptor — bits 7..4
    /// reserved, bits 3..0 carry the state value).
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// One advertised iSCSI target port. RTPI is assigned at topology
/// construction (sequential from 1; RTPI 0 is reserved by SPC-4
/// §3.1.118). TPGT mirrors the value the iSCSI Login Response
/// `TargetPortalGroupTag` key carries for a connection arriving on
/// that portal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPort {
    pub rtpi: u16,
    pub tpgt: u16,
}

/// Topology snapshot read by every ALUA-touching SCSI handler in
/// both products. Built once at daemon startup from
/// `ServerConfig::listen_portals` and shared via `Arc`. The state
/// field hides behind an `RwLock` so a future SET TPG opcode can
/// flip it without re-plumbing the topology; today the lock is
/// only ever taken for reads.
#[derive(Debug)]
pub struct AluaTopology {
    ports: Vec<TargetPort>,
    /// Per-TPG asymmetric access state, indexed by TPGT. Defaults to
    /// [`AsymmetricAccessState::ActiveOptimized`] on construction.
    states: RwLock<BTreeMap<u16, AsymmetricAccessState>>,
    /// Daemon-stable namespace BLAKE3-mixed into the per-port NAA-3
    /// identifier so two daemons on the same host can't collide.
    /// VTL passes the chassis serial; VSA passes the target IQN.
    naa_namespace: String,
}

impl AluaTopology {
    /// Build a topology from the advertised portal list. RTPIs are
    /// assigned sequentially starting at 1, in the order portals
    /// appear in `portals`. Every TPG observed in the list defaults
    /// to [`AsymmetricAccessState::ActiveOptimized`].
    ///
    /// `naa_namespace` is mixed into the per-port NAA-3 derivation —
    /// see [`scsi_spc::naa::naa3_target_port`].
    pub fn from_portals(portals: &[Portal], naa_namespace: impl Into<String>) -> Self {
        let ports: Vec<TargetPort> = portals
            .iter()
            .enumerate()
            .map(|(idx, p)| TargetPort {
                // RTPI 0 is reserved (SPC-4 §3.1.118); start at 1.
                rtpi: (idx as u16) + 1,
                tpgt: p.tpgt,
            })
            .collect();
        let mut states = BTreeMap::new();
        for port in &ports {
            states
                .entry(port.tpgt)
                .or_insert(AsymmetricAccessState::ActiveOptimized);
        }
        Self {
            ports,
            states: RwLock::new(states),
            naa_namespace: naa_namespace.into(),
        }
    }

    /// All advertised target ports. Order matches the configuration
    /// (`ServerConfig::listen_portals`), which is also the order RTPIs
    /// were assigned in.
    pub fn ports(&self) -> &[TargetPort] {
        &self.ports
    }

    /// Snapshot of all configured TPGs with their current asymmetric
    /// access state and member RTPIs. Used by REPORT TARGET PORT
    /// GROUPS to walk groups in deterministic order.
    pub fn groups(&self) -> Vec<(u16, AsymmetricAccessState, Vec<u16>)> {
        let states = self
            .states
            .read()
            .expect("ALUA state lock poisoned (another thread panicked while holding it)");
        let mut by_tpgt: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
        for port in &self.ports {
            by_tpgt.entry(port.tpgt).or_default().push(port.rtpi);
        }
        by_tpgt
            .into_iter()
            .map(|(tpgt, members)| {
                let state = states
                    .get(&tpgt)
                    .copied()
                    .unwrap_or(AsymmetricAccessState::ActiveOptimized);
                (tpgt, state, members)
            })
            .collect()
    }

    /// Stable 8-byte NAA-3 (Locally Assigned) identifier for the
    /// target port at `rtpi`. Returns the same bytes the VPD 0x83
    /// TargetPort-association NAA designator carries.
    pub fn port_naa(&self, rtpi: u16) -> [u8; 8] {
        naa3_target_port(&self.naa_namespace, rtpi)
    }

    /// Append `Association::TargetPort` designators to a VPD 0x83
    /// descriptor buffer — three descriptors per advertised port:
    ///
    /// 1. NAA-3 (`DesignatorType::Naa`, 8 bytes) — stable identifier
    ///    for the port across daemon restarts.
    /// 2. Relative Target Port Identifier (`DesignatorType::RelativeTargetPort`,
    ///    4 bytes: 2 reserved + BE16 RTPI). Linux dm-multipath reads
    ///    this to learn which path it arrived on.
    /// 3. Target Port Group (`DesignatorType::TargetPortGroup`, 4
    ///    bytes: 2 reserved + BE16 TPGT). Together with REPORT TPG
    ///    this lets the initiator correlate "path X → TPG K → state
    ///    S".
    ///
    /// The product's existing per-LUN VPD 0x83 builder calls this
    /// after appending its LogicalUnit-association descriptors
    /// (NAA / T10 / LUG).
    pub fn push_vpd83_target_port_descriptors(&self, buf: &mut Vec<u8>) {
        for port in &self.ports {
            let naa = self.port_naa(port.rtpi);
            push_designator(
                buf,
                CodeSet::Binary,
                Association::TargetPort,
                DesignatorType::Naa,
                &naa,
            );
            let mut rtpi_value = [0u8; 4];
            rtpi_value[2..4].copy_from_slice(&port.rtpi.to_be_bytes());
            push_designator(
                buf,
                CodeSet::Binary,
                Association::TargetPort,
                DesignatorType::RelativeTargetPort,
                &rtpi_value,
            );
            let mut tpg_value = [0u8; 4];
            tpg_value[2..4].copy_from_slice(&port.tpgt.to_be_bytes());
            push_designator(
                buf,
                CodeSet::Binary,
                Association::TargetPort,
                DesignatorType::TargetPortGroup,
                &tpg_value,
            );
        }
    }

    /// Assemble the REPORT TARGET PORT GROUPS response body
    /// (MAINTENANCE IN service action 0x0A, SPC-4 §6.27.7) — the
    /// caller is responsible for truncating to ALLOCATION LENGTH.
    ///
    /// Layout (extended-header-off form — PARAMETER DATA FORMAT = 0):
    ///
    /// ```text
    ///   bytes 0..3       RETURN DATA LENGTH (BE32, excludes these 4)
    ///   per TPG descriptor (8 + 4*N bytes, N = member-port count):
    ///     byte 0         PREF(7)=0 | reserved(6:4) | ASYMMETRIC ACCESS
    ///                    STATE(3:0)
    ///     byte 1         T_SUP(7)=1 | O_SUP(6)=1 | reserved(5:4) |
    ///                    U_SUP(3)=1 | S_SUP(2)=1 | AN_SUP(1)=1 |
    ///                    AO_SUP(0)=1
    ///     bytes 2..3     TARGET PORT GROUP (BE16 TPGT)
    ///     byte 4         reserved
    ///     byte 5         STATUS CODE = 0x00 (no recent transition)
    ///     byte 6         vendor specific
    ///     byte 7         TARGET PORT COUNT
    ///     per port (4 bytes):
    ///       bytes 0..1   reserved
    ///       bytes 2..3   RELATIVE TARGET PORT IDENTIFIER (BE16)
    /// ```
    ///
    /// `T_SUP` / `O_SUP` / `U_SUP` / `S_SUP` / `AN_SUP` / `AO_SUP`
    /// advertise that the target supports the corresponding access
    /// state on this TPG. We set all of them: the topology is
    /// implicit-only today, but a future SET TPG can transition to
    /// any of the standard states without re-encoding the support
    /// bitmap.
    pub fn report_target_port_groups_body(&self) -> Vec<u8> {
        let groups = self.groups();
        let body_len: usize = groups.iter().map(|(_, _, ports)| 8 + 4 * ports.len()).sum();

        let mut data = Vec::with_capacity(4 + body_len);
        data.extend_from_slice(&(body_len as u32).to_be_bytes());

        for (tpgt, state, ports) in groups {
            // byte 0: PREF=0 + access state in low nibble
            data.push(state.code() & 0x0F);
            // byte 1: support bitmap — every standard access state
            // available. T_SUP(7) | O_SUP(6) | U_SUP(3) | S_SUP(2) |
            // AN_SUP(1) | AO_SUP(0) = 0b1100_1111 = 0xCF.
            data.push(0xCF);
            // bytes 2..3: TPGT
            data.extend_from_slice(&tpgt.to_be_bytes());
            // byte 4: reserved
            data.push(0);
            // byte 5: STATUS CODE = 0 (no transition pending)
            data.push(0);
            // byte 6: vendor specific
            data.push(0);
            // byte 7: TARGET PORT COUNT
            data.push(ports.len() as u8);
            for rtpi in ports {
                // bytes 0..1: reserved
                data.extend_from_slice(&[0u8; 2]);
                // bytes 2..3: RELATIVE TARGET PORT IDENTIFIER
                data.extend_from_slice(&rtpi.to_be_bytes());
            }
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portal(addr: &str, tpgt: u16) -> Portal {
        Portal {
            bind: addr.to_string(),
            advertise: None,
            tpgt,
        }
    }

    #[test]
    fn from_portals_assigns_sequential_rtpis_starting_at_one() {
        let topo = AluaTopology::from_portals(
            &[
                portal("0.0.0.0:3260", 1),
                portal("10.0.0.1:3260", 2),
                portal("10.0.0.2:3260", 2),
            ],
            "namespace",
        );
        assert_eq!(
            topo.ports(),
            &[
                TargetPort { rtpi: 1, tpgt: 1 },
                TargetPort { rtpi: 2, tpgt: 2 },
                TargetPort { rtpi: 3, tpgt: 2 },
            ]
        );
    }

    #[test]
    fn groups_default_to_active_optimized() {
        let topo = AluaTopology::from_portals(
            &[portal("0.0.0.0:3260", 1), portal("0.0.0.0:3261", 5)],
            "ns",
        );
        let groups = topo.groups();
        assert_eq!(groups.len(), 2);
        for (_, state, _) in &groups {
            assert_eq!(*state, AsymmetricAccessState::ActiveOptimized);
        }
        // Member RTPIs are the ports whose TPGT matches the group.
        let g1 = groups.iter().find(|(t, _, _)| *t == 1).unwrap();
        assert_eq!(g1.2, vec![1u16]);
        let g5 = groups.iter().find(|(t, _, _)| *t == 5).unwrap();
        assert_eq!(g5.2, vec![2u16]);
    }

    #[test]
    fn shared_tpgt_groups_member_ports_together() {
        let topo = AluaTopology::from_portals(
            &[
                portal("0.0.0.0:3260", 1),
                portal("10.0.0.1:3260", 1),
                portal("10.0.0.2:3260", 1),
            ],
            "ns",
        );
        let groups = topo.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, 1);
        assert_eq!(groups[0].2, vec![1, 2, 3]);
    }

    #[test]
    fn port_naa_is_stable_and_carries_naa_type_3() {
        let topo = AluaTopology::from_portals(&[portal("0.0.0.0:3260", 1)], "iqn.example:target");
        let a = topo.port_naa(1);
        let b = topo.port_naa(1);
        assert_eq!(a, b, "deterministic per (namespace, rtpi)");
        assert_eq!(a[0] & 0xF0, 0x30, "NAA type 3 in top nibble");
        // Different RTPIs yield different identifiers.
        let c = topo.port_naa(2);
        assert_ne!(a, c);
    }

    #[test]
    fn vpd83_target_port_descriptors_emit_three_per_port() {
        let topo = AluaTopology::from_portals(
            &[portal("0.0.0.0:3260", 1), portal("10.0.0.1:3260", 2)],
            "ns",
        );
        let mut buf = Vec::new();
        topo.push_vpd83_target_port_descriptors(&mut buf);
        // Each descriptor: 4-byte header + body (8 for NAA, 4 for RTPI,
        // 4 for TPG) = 12 + 8 + 8 = 28 bytes per port.
        assert_eq!(buf.len(), 28 * 2);

        // Walk the first port's descriptors.
        // 0: NAA — byte 0 = code-set 1 (binary), byte 1 = assoc<<4 | type
        //          = (1<<4)|0x03 = 0x13. byte 3 = body length 8.
        assert_eq!(buf[0], 0x01);
        assert_eq!(buf[1], 0x13);
        assert_eq!(buf[3], 8);
        // 1: RelativeTargetPort — byte 1 = (1<<4)|0x04 = 0x14, body 4
        //    with RTPI=1 at bytes [2..4].
        let rtpi_off = 4 + 8;
        assert_eq!(buf[rtpi_off + 1], 0x14);
        assert_eq!(buf[rtpi_off + 3], 4);
        assert_eq!(
            u16::from_be_bytes([buf[rtpi_off + 4 + 2], buf[rtpi_off + 4 + 3]]),
            1
        );
        // 2: TargetPortGroup — byte 1 = (1<<4)|0x05 = 0x15, body 4
        //    with TPGT=1 at bytes [2..4].
        let tpg_off = rtpi_off + 8;
        assert_eq!(buf[tpg_off + 1], 0x15);
        assert_eq!(
            u16::from_be_bytes([buf[tpg_off + 4 + 2], buf[tpg_off + 4 + 3]]),
            1
        );
        // Second port carries RTPI=2 / TPGT=2.
        let port2_tpg = 28 + 4 + 8 + 8;
        assert_eq!(
            u16::from_be_bytes([buf[port2_tpg + 4 + 2], buf[port2_tpg + 4 + 3]]),
            2
        );
    }

    #[test]
    fn report_tpg_body_lists_each_group_with_member_ports() {
        let topo = AluaTopology::from_portals(
            &[
                portal("0.0.0.0:3260", 1),
                portal("10.0.0.1:3260", 2),
                portal("10.0.0.2:3260", 2),
            ],
            "ns",
        );
        let body = topo.report_target_port_groups_body();
        // 4-byte return-data length header + 2 TPG descriptors.
        // TPG 1: 8 + 4*1 = 12; TPG 2: 8 + 4*2 = 16; total body 28.
        let body_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(body_len as usize, body.len() - 4);
        assert_eq!(body_len, 28);

        // TPG 1 descriptor starts at offset 4.
        assert_eq!(
            body[4] & 0x0F,
            AsymmetricAccessState::ActiveOptimized.code()
        );
        // support bitmap: T_SUP|O_SUP|U_SUP|S_SUP|AN_SUP|AO_SUP = 0xCF.
        assert_eq!(body[5], 0xCF);
        // TPGT = 1
        assert_eq!(u16::from_be_bytes([body[6], body[7]]), 1);
        // port count = 1
        assert_eq!(body[11], 1);
        // member RTPI at bytes 14..16 = 1
        assert_eq!(u16::from_be_bytes([body[14], body[15]]), 1);

        // TPG 2 descriptor starts at offset 4 + 12 = 16.
        let off = 16;
        assert_eq!(u16::from_be_bytes([body[off + 2], body[off + 3]]), 2);
        // port count = 2
        assert_eq!(body[off + 7], 2);
        // member RTPIs 2 and 3.
        assert_eq!(u16::from_be_bytes([body[off + 10], body[off + 11]]), 2);
        assert_eq!(u16::from_be_bytes([body[off + 14], body[off + 15]]), 3);
    }
}
