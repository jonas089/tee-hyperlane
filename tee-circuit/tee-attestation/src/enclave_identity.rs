//! Which enclave is allowed to speak for this bridge.
//!
//! A DCAP quote on its own only proves that *some* genuine, non-debug TDX machine signed a
//! payload. Anyone renting any TDX host can produce one. Turning that into "our code, on an
//! acceptable platform, said this" needs two further judgements, and they are different
//! kinds of judgement, so they are separate functions:
//!
//! * [`verify_platform_tcb`] - is the *hardware and firmware* still trustworthy? Intel's
//!   answer, from the signed TCB collateral.
//! * [`verify_enclave_identity`] - is this *our software stack*? Our answer, from the
//!   measurements pinned at build time.
//!
//! `evolve-tee` makes neither check, which is why its own docs call it unsafe for
//! production.

use sha2::{Digest, Sha256};

use crate::attestation::{get_event_value, get_measurements, EventLog, Measurements};
use dcap_qvl::tcb_info::TcbStatus;
use dcap_qvl::verify::VerifiedReport;

/// dstack event-log keys that identify the software stack.
pub const EVENT_COMPOSE_HASH: &str = "compose-hash";
pub const EVENT_OS_IMAGE_HASH: &str = "os-image-hash";
pub const EVENT_MR_KMS: &str = "mr-kms";
pub const EVENT_KEY_PROVIDER: &str = "key-provider";

/// TCB levels this bridge will accept.
///
/// Anything below `SWHardeningNeeded` means Intel has published a reason the platform may be
/// compromised. A bridge that keeps minting against such a platform is trading other
/// people's funds for its own uptime.
pub const ALLOWED_TCB_STATUS: &[TcbStatus] = &[TcbStatus::UpToDate, TcbStatus::SWHardeningNeeded];

/// The measurements that identify our enclave, layer by layer.
///
/// `app-id` and `instance-id` are deliberately absent: they differ per CVM, while
/// `compose_hash` already covers the image digest and the whole compose file. Leaving them
/// out is what lets every node in a deployment share one identity, one circuit build and one
/// pair of vkeys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnclaveIdentity {
    /// Platform: the TD's build-time measurement, covering the dstack OS image *and* the
    /// VM's vCPU/memory shape. Every node must therefore use the same instance type.
    pub mr_td: [u8; 48],
    /// Platform: the dstack OS image, as dstack itself records it.
    pub os_image_hash: Vec<u8>,
    /// Application: hash of the compose file, which pins our container image digest.
    pub compose_hash: Vec<u8>,
    /// Key management: which KMS may release this application's keys.
    pub mr_kms: Vec<u8>,
    /// Key management: which key-provider mode the CVM booted with.
    pub key_provider: Vec<u8>,
}

/// Whether this build demands a specific enclave.
///
/// `Any` exists so the bridge can be developed and tested before a CVM exists. It still
/// requires a genuine, non-debug TDX quote on an acceptable TCB level - it only stops
/// asking *which* enclave. It is never a production configuration, and the ISM state it
/// produces says so in the clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityPolicy {
    Any,
    Require(EnclaveIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("quote is not a TDX report")]
    NotATdxReport,
    #[error("{layer} TCB status is {status:?}, which this bridge does not accept")]
    TcbNotAcceptable {
        layer: &'static str,
        status: TcbStatus,
    },
    #[error("event log does not replay to the RTMRs in the quote")]
    EventLogNotBoundToQuote,
    #[error("mr_td does not match the pinned platform measurement")]
    PlatformMeasurementMismatch,
    #[error("event `{0}` is missing, duplicated, or its digest does not commit to its text")]
    EventUnusable(&'static str),
    #[error("event `{0}` does not match the pinned value")]
    EventMismatch(&'static str),
}

impl EnclaveIdentity {
    /// The pinned event-log values, paired with the key each is read from.
    fn pinned_events(&self) -> [(&'static str, &[u8]); 4] {
        [
            (EVENT_OS_IMAGE_HASH, &self.os_image_hash),
            (EVENT_COMPOSE_HASH, &self.compose_hash),
            (EVENT_MR_KMS, &self.mr_kms),
            (EVENT_KEY_PROVIDER, &self.key_provider),
        ]
    }
}

impl IdentityPolicy {
    /// A stable id for this policy, mirrored into the ISM state so anyone can see which
    /// enclave an ISM was created for without disassembling an ELF.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            // Deliberately recognisable: an ISM carrying this digest is a development ISM.
            Self::Any => h.update(b"tee-isms/enclave-identity/v1/ANY-DEVELOPMENT-ONLY"),
            Self::Require(id) => {
                h.update(b"tee-isms/enclave-identity/v1");
                h.update(id.mr_td);
                for (name, value) in id.pinned_events() {
                    h.update((name.len() as u64).to_be_bytes());
                    h.update(name.as_bytes());
                    h.update((value.len() as u64).to_be_bytes());
                    h.update(value);
                }
            }
        }
        h.finalize().into()
    }

    pub fn requires_specific_enclave(&self) -> bool {
        matches!(self, Self::Require(_))
    }
}

/// Intel's verdict on the hardware and firmware. Applies whatever our identity policy is.
pub fn verify_platform_tcb(report: &VerifiedReport) -> Result<(), IdentityError> {
    for (layer, status) in [
        ("platform", &report.platform_status.status),
        ("quoting enclave", &report.qe_status.status),
    ] {
        if !ALLOWED_TCB_STATUS.contains(status) {
            return Err(IdentityError::TcbNotAcceptable {
                layer,
                status: status.clone(),
            });
        }
    }
    Ok(())
}

/// Our verdict on the software stack.
///
/// `replayed_rtmrs` is the caller's replay of `eventlog`, passed in so it happens once.
/// Checking it against the quote is what makes the event log believable at all; without it
/// the log is just untrusted text alongside a signature.
pub fn verify_enclave_identity(
    policy: &IdentityPolicy,
    report: &VerifiedReport,
    eventlog: &[EventLog],
    replayed_rtmrs: &[[u8; 48]; 4],
) -> Result<(), IdentityError> {
    verify_platform_tcb(report)?;

    let measured: Measurements =
        get_measurements(&report.report).ok_or(IdentityError::NotATdxReport)?;
    if replayed_rtmrs != &measured.rtmrs {
        return Err(IdentityError::EventLogNotBoundToQuote);
    }

    let Some(identity) = (match policy {
        IdentityPolicy::Any => None,
        IdentityPolicy::Require(id) => Some(id),
    }) else {
        return Ok(());
    };

    if measured.mr_td != identity.mr_td {
        return Err(IdentityError::PlatformMeasurementMismatch);
    }
    for (name, expected) in identity.pinned_events() {
        let got = get_event_value(eventlog, name).ok_or(IdentityError::EventUnusable(name))?;
        if got != expected {
            return Err(IdentityError::EventMismatch(name));
        }
    }
    Ok(())
}

include!(concat!(env!("OUT_DIR"), "/pinned_identity.rs"));

/// The identity policy this build was compiled against.
pub fn build_identity_policy() -> &'static IdentityPolicy {
    IDENTITY_POLICY.get_or_init(load_identity_policy)
}

/// `identity_digest` as carried in [`crate::ism::IsmState`].
pub fn build_identity_digest() -> [u8; 32] {
    build_identity_policy().digest()
}
