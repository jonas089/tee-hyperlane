//! Celestia consensus: a Tendermint light client.
//!
//! Only *consensus* is needed, not data availability sampling: Hyperlane messages live in
//! the application's IAVL tree, which the app hash commits to, and the app hash is in the
//! header. No shares are read, so no DAS.
//!
//! The store is carried in the ISM state rather than on disk, so the destination chain is
//! this light client's database and the enclave stays stateless.

use sha2::{Digest, Sha256};
use tendermint::validator::Set as ValidatorSet;
use tendermint_light_client_verifier::options::Options;
use tendermint_light_client_verifier::types::{LightBlock, TrustThreshold};
use tendermint_light_client_verifier::{ProdVerifier, Verdict, Verifier};

use super::AttestedRoot;

/// Celestia's unbonding period is 21 days; staying well inside it keeps a light client's
/// equivocation guarantee backed by stake that is still slashable.
pub const TRUSTING_PERIOD_SECS: u64 = 14 * 24 * 60 * 60;
/// Tolerated difference between our clock and a header's timestamp.
pub const CLOCK_DRIFT_SECS: u64 = 10 * 60;

/// What the enclave must be handed to extend the chain of trust.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CelestiaStore {
    /// The last header this ISM trusts, with the validator set that signed it and the set
    /// expected to sign the next one.
    pub trusted: LightBlock,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CelestiaError {
    #[error(
        "trusted header commits to next validators {committed} but the set hashes to {actual}"
    )]
    InconsistentTrustedSet { committed: String, actual: String },
    #[error("no light blocks supplied")]
    NoUpdates,
    #[error("light block at height {height} is on chain `{got}`, expected `{expected}`")]
    WrongChain {
        height: u64,
        got: String,
        expected: String,
    },
    #[error("light block at height {height} does not advance from {trusted}")]
    NotAdvancing { height: u64, trusted: u64 },
    #[error("light client rejected the update at height {height}: {reason}")]
    Rejected { height: u64, reason: String },
    #[error("header at height {0} has no app hash")]
    NoAppHash(u64),
}

fn options() -> Options {
    Options {
        // Two thirds of the *trusted* validator set must have signed, so a fork needs a
        // third of stake that is still slashable to equivocate.
        trust_threshold: TrustThreshold::TWO_THIRDS,
        trusting_period: core::time::Duration::from_secs(TRUSTING_PERIOD_SECS),
        clock_drift: core::time::Duration::from_secs(CLOCK_DRIFT_SECS),
    }
}

/// Walk the trusted header forward over the supplied light blocks.
///
/// `now` drives the trusting-period check, and it must come from the enclave rather than
/// from the request. The check is `trusted.time + trusting_period > now`, so a caller who
/// chooses `now` can hold an expired header open forever, and an old validator set - whose
/// keys are worth far less once the period has passed - could then sign a fork the enclave
/// would accept. Deriving it from the header being verified has the same flaw, which is what
/// makes `evolve-tee`'s trusting period vacuous.
///
/// The residual is the host's clock, which a TDX guest cannot escape. That is a much smaller
/// surface than a free field: it takes the operator rewinding the machine, not an attacker
/// choosing a number in a request anyone can send.
pub fn verify_celestia_updates(
    store: &mut CelestiaStore,
    updates: &[LightBlock],
    now: tendermint::Time,
) -> Result<(), CelestiaError> {
    if updates.is_empty() {
        return Err(CelestiaError::NoUpdates);
    }
    // tendermint-rs states this as the caller's job: `verify_update_header` trusts that the
    // trusted block's next validator set is the one its header commits to, and never checks.
    // Committing both values separately, as the store does, is not the same as requiring them
    // to agree.
    let committed = store.trusted.signed_header.header.next_validators_hash;
    let actual = store.trusted.next_validators.hash();
    if committed != actual {
        return Err(CelestiaError::InconsistentTrustedSet {
            committed: committed.to_string(),
            actual: actual.to_string(),
        });
    }

    let verifier = ProdVerifier::default();
    let opts = options();
    let chain_id = store.trusted.signed_header.header.chain_id.to_string();

    for next in updates {
        let height = next.height().value();
        let got = next.signed_header.header.chain_id.to_string();
        if got != chain_id {
            return Err(CelestiaError::WrongChain {
                height,
                got,
                expected: chain_id,
            });
        }
        if height <= store.trusted.height().value() {
            return Err(CelestiaError::NotAdvancing {
                height,
                trusted: store.trusted.height().value(),
            });
        }
        let verdict = verifier.verify_update_header(
            next.as_untrusted_state(),
            store.trusted.as_trusted_state(),
            &opts,
            now,
        );
        match verdict {
            Verdict::Success => store.trusted = next.clone(),
            Verdict::NotEnoughTrust(why) => {
                return Err(CelestiaError::Rejected {
                    height,
                    reason: why.to_string(),
                })
            }
            Verdict::Invalid(why) => {
                return Err(CelestiaError::Rejected {
                    height,
                    reason: format!("{why:?}"),
                })
            }
        }
    }
    Ok(())
}

/// The app hash of the trusted header, which commits to the state *one block earlier*.
///
/// That off-by-one is inherent to Cosmos: `header[H].app_hash` is the result of executing
/// block `H-1`. Messages dispatched in block `H-1` are therefore provable under this root,
/// and `height` is reported as `H-1` so the ISM state names the state it actually describes.
pub fn celestia_root(store: &CelestiaStore) -> Result<AttestedRoot, CelestiaError> {
    let header = &store.trusted.signed_header.header;
    let height = header.height.value();
    let app_hash = header.app_hash.as_bytes();
    if app_hash.len() != 32 {
        return Err(CelestiaError::NoAppHash(height));
    }
    Ok(AttestedRoot {
        state_root: alloy_primitives::B256::from_slice(app_hash),
        height: height.saturating_sub(1),
        timestamp: header.time.unix_timestamp() as u64,
    })
}

/// Commit to the trusted header and the validator sets that make it meaningful.
///
/// A commitment over the header alone would let a relayer swap in a different validator set
/// and break the next verification's trust assumption.
pub fn commit_celestia_store(store: &CelestiaStore) -> [u8; 32] {
    let b = &store.trusted;
    let mut h = Sha256::new();
    h.update(b"tee-isms/celestia-store/v1");
    h.update(b.signed_header.header.chain_id.as_str().as_bytes());
    h.update(b.height().value().to_be_bytes());
    h.update(b.signed_header.header.hash().as_bytes());
    h.update(validator_set_hash(&b.validators));
    h.update(validator_set_hash(&b.next_validators));
    h.finalize().into()
}

fn validator_set_hash(set: &ValidatorSet) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(set.hash().as_bytes());
    out
}
