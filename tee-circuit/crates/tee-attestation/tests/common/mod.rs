//! Shared builders: a self-consistent event log, quote report and attested update.

// Each test binary uses a different subset of these.
#![allow(dead_code)]

use dcap_qvl::quote::{Report, TDReport10};
use dcap_qvl::tcb_info::{TcbStatus, TcbStatusWithAdvisory};
use dcap_qvl::verify::VerifiedReport;
use tee_attestation::attestation::{event_preimage_v1, sha384, APPLICATION_IMR};
use tee_attestation::*;

pub const ET: u32 = DSTACK_RUNTIME_EVENT_TYPE;
pub const HEAD_TS: u64 = 1_800_000_000;
pub const MR_TD: [u8; 48] = [0x11; 48];

pub fn ev(imr: u32, name: &str, payload: &[u8]) -> EventLog {
    EventLog {
        imr,
        event_type: ET,
        digest: sha384(&event_preimage_v1(ET, name, payload)),
        event: name.to_string(),
        event_payload: payload.to_vec(),
    }
}

pub fn identity() -> EnclaveIdentity {
    EnclaveIdentity {
        mr_td: MR_TD,
        os_image_hash: vec![0xbb; 32],
        compose_hash: vec![0xaa; 32],
        mr_kms: vec![0xcc; 48],
        key_provider: b"kms-key-provider".to_vec(),
    }
}

pub fn policy() -> IdentityPolicy {
    IdentityPolicy::Require(identity())
}

pub fn good_log() -> Vec<EventLog> {
    let id = identity();
    vec![
        ev(0, "boot", b"x"),
        ev(APPLICATION_IMR, "os-image-hash", &id.os_image_hash),
        ev(APPLICATION_IMR, "compose-hash", &id.compose_hash),
        ev(APPLICATION_IMR, "mr-kms", &id.mr_kms),
        ev(APPLICATION_IMR, "key-provider", &id.key_provider),
        ev(APPLICATION_IMR, "instance-id", b"instance-a"),
    ]
}

pub fn report(
    log: &[EventLog],
    mr_td: [u8; 48],
    status: TcbStatus,
    report_data: [u8; 64],
) -> VerifiedReport {
    let r = replay_event_logs(log);
    VerifiedReport {
        status: format!("{status:?}"),
        advisory_ids: vec![],
        report: Report::TD10(TDReport10 {
            tee_tcb_svn: [0; 16],
            mr_seam: [0; 48],
            mr_signer_seam: [0; 48],
            seam_attributes: [0; 8],
            td_attributes: [0; 8],
            xfam: [0; 8],
            mr_td,
            mr_config_id: [0; 48],
            mr_owner: [0; 48],
            mr_owner_config: [0; 48],
            rt_mr0: r[0],
            rt_mr1: r[1],
            rt_mr2: r[2],
            rt_mr3: r[3],
            report_data,
        }),
        ppid: vec![],
        qe_status: TcbStatusWithAdvisory { status: TcbStatus::UpToDate, advisory_ids: vec![] },
        platform_status: TcbStatusWithAdvisory { status, advisory_ids: vec![] },
    }
}

pub fn state(root: u8, height: u64, ts: u64) -> IsmState {
    IsmState {
        state_root: [root; 32],
        origin_domain: 11155111,
        height,
        timestamp: ts,
        lc_store_commit: [7u8; 32],
        identity_digest: policy().digest(),
    }
}

pub fn update() -> AttestedUpdate {
    AttestedUpdate {
        prev_state: state(1, 100, HEAD_TS - 60),
        new_state: state(2, 101, HEAD_TS),
        merkle_tree_address: [0x5a; 32],
        message_ids: vec![[9u8; 32]],
    }
}

pub fn report_for(log: &[EventLog], u: &AttestedUpdate) -> VerifiedReport {
    let mut rd = [0u8; 64];
    rd[..32].copy_from_slice(&hash_attested_update(u));
    report(log, MR_TD, TcbStatus::UpToDate, rd)
}
