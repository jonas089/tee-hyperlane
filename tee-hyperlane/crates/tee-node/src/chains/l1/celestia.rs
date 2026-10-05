//! Celestia as an origin: a Tendermint light client, and the Hyperlane tree in the app hash.
//!
//! Only consensus is needed, not data availability sampling: Hyperlane's tree lives in the
//! application state the app hash commits to, and the app hash is in the header.

use crate::state::IsmState;
use alloy_primitives::B256;
use hyperlane_types::{MerkleTree, TREE_DEPTH};
use ics23::commitment_proof::Proof;
use ics23::{
    calculate_existence_root, iavl_spec, tendermint_spec, CommitmentProof, HostFunctionsManager,
};
use prost::Message;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tendermint_light_client_verifier::options::Options;
use tendermint_light_client_verifier::types::{LightBlock, TrustThreshold};
use tendermint_light_client_verifier::{ProdVerifier, Verdict, Verifier};

use crate::origin::{self, AttestedRoot, Chain, Head, Origin, Tree};

pub static CELESTIA: Chain = Chain {
    name: "celestia",
    domain: DOMAIN,
    origin: &Celestia,
};

/// Hyperlane's domain for mocha. The ISM state carries it; the local devnet's own domain
/// differs, and that is harmless because the light-client store is what pins the chain.
pub const DOMAIN: u32 = 1297040200;

/// Celestia's unbonding period is 21 days; staying well inside it keeps the light client's
/// equivocation guarantee backed by stake that is still slashable.
const TRUSTING_PERIOD_SECS: u64 = 14 * 24 * 60 * 60;
/// Tolerated difference between our clock and a header's timestamp.
const CLOCK_DRIFT_SECS: u64 = 10 * 60;

/// hyperlane-cosmos keeps merkle tree hooks under this prefix in the `hyperlane` store:
/// post-dispatch submodule id 2, collection 4.
const HOOK_PREFIX: [u8; 2] = [2, 4];
const HYPERLANE_STORE: &str = "hyperlane";

pub struct Celestia;

/// A Celestia step: the store the ISM committed to, and the light blocks that walk it forward.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub store: CelestiaStore,
    pub updates: Vec<LightBlock>,
}

/// The light client's state, carried by the coprocessor and pinned by the ISM's
/// `lc_store_commit`: the last header this ISM trusts, with the validator set that signed it
/// and the set expected to sign the next one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CelestiaStore {
    pub trusted: LightBlock,
}

/// A merkle tree hook's stored bytes, and the two steps placing them under the app hash: the
/// bytes up to the Hyperlane module's store root (IAVL), then that root up to the app hash (a
/// simple merkle proof over the list of stores).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TreeProof {
    pub hook_id: [u8; 32],
    pub hook_bytes: Vec<u8>,
    pub steps: Vec<ProofStep>,
}

/// One of those two steps. The RPC calls these "proof ops"; the field names are the RPC's.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProofStep {
    /// `ics23:iavl` for the first step, `ics23:simple` for the second.
    pub proof_type: String,
    pub key: Vec<u8>,
    pub data: Vec<u8>,
}

impl Origin for Celestia {
    /// Walk the light client forward over the supplied light blocks. Two thirds of the
    /// *trusted* validator set must sign each one, so a fork needs a third of stake that is
    /// still slashable to equivocate.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input { mut store, updates } = origin::parse("celestia input", input)?;
        anyhow::ensure!(
            store.commitment() == trusted.lc_store_commit,
            "supplied light-client store does not match the commitment in the ISM state"
        );
        anyhow::ensure!(!updates.is_empty(), "no light blocks supplied");
        // tendermint-rs leaves this to the caller: it trusts that the trusted block's next
        // validator set is the one its header commits to, and never checks.
        anyhow::ensure!(
            store.trusted.signed_header.header.next_validators_hash
                == store.trusted.next_validators.hash(),
            "the trusted header's next validator set does not match the one supplied"
        );

        // The trusting-period check needs wall time, and it must be the enclave's own. A caller
        // who picks it can hold an expired header open forever, and an old validator set, whose
        // keys are worth far less once the period has passed, could then sign a fork.
        let now = tendermint::Time::from_unix_timestamp(origin::now()? as i64, 0)?;
        let options = Options {
            trust_threshold: TrustThreshold::TWO_THIRDS,
            trusting_period: core::time::Duration::from_secs(TRUSTING_PERIOD_SECS),
            clock_drift: core::time::Duration::from_secs(CLOCK_DRIFT_SECS),
        };
        let chain_id = store.trusted.signed_header.header.chain_id.clone();
        for next in &updates {
            let height = next.height().value();
            anyhow::ensure!(
                next.signed_header.header.chain_id == chain_id,
                "light block {height} is on another chain"
            );
            anyhow::ensure!(
                height > store.trusted.height().value(),
                "light block {height} does not advance"
            );
            match ProdVerifier::default().verify_update_header(
                next.as_untrusted_state(),
                store.trusted.as_trusted_state(),
                &options,
                now,
            ) {
                Verdict::Success => store.trusted = next.clone(),
                Verdict::NotEnoughTrust(why) => {
                    anyhow::bail!("light block {height} rejected: {why}")
                }
                Verdict::Invalid(why) => anyhow::bail!("light block {height} rejected: {why:?}"),
            }
        }

        let root = store.root()?;
        Ok(Head {
            root: root.state_root,
            height: root.height,
            timestamp: root.timestamp,
            store_commit: store.commitment(),
            attested_at: root.timestamp,
        })
    }

    /// Prove the hook's bytes under the app hash, through both steps, and decode its tree.
    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        let TreeProof {
            hook_id,
            hook_bytes,
            steps,
        } = origin::parse("celestia tree proof", proof)?;
        anyhow::ensure!(
            steps.len() == 2,
            "expected two proof steps, got {}",
            steps.len()
        );
        anyhow::ensure!(
            steps[0].proof_type == "ics23:iavl" && steps[1].proof_type == "ics23:simple",
            "expected an iavl step then a simple step"
        );
        // `collections.Map` keys are the prefix and then the big-endian uint64 internal id,
        // which is the low 8 bytes of the hook's 32-byte address.
        let key = [HOOK_PREFIX.as_slice(), &hook_id[24..]].concat();
        anyhow::ensure!(
            steps[0].key == key,
            "the proof is for a different key than this hook's"
        );
        anyhow::ensure!(
            steps[1].key == HYPERLANE_STORE.as_bytes(),
            "the proof is for another store"
        );

        let existence =
            |i: usize| match CommitmentProof::decode(steps[i].data.as_slice()).map(|p| p.proof) {
                Ok(Some(Proof::Exist(e))) => Ok(e),
                _ => Err(anyhow::anyhow!(
                    "proof step {i} is not an ics23 existence proof"
                )),
            };
        let wrap = |e| CommitmentProof {
            proof: Some(Proof::Exist(e)),
        };
        // The hook exists in the Hyperlane module's IAVL tree, under some store root...
        let inner = existence(0)?;
        let store_root = calculate_existence_root::<HostFunctionsManager>(&inner)
            .map_err(|_| anyhow::anyhow!("the iavl step does not verify"))?;
        anyhow::ensure!(
            ics23::verify_membership::<HostFunctionsManager>(
                &wrap(inner),
                &iavl_spec(),
                &store_root,
                &key,
                &hook_bytes
            ),
            "the iavl step does not verify"
        );
        // ...and that store root is what the app hash commits to for the Hyperlane store.
        anyhow::ensure!(
            ics23::verify_membership::<HostFunctionsManager>(
                &wrap(existence(1)?),
                &tendermint_spec(),
                &root.to_vec(),
                HYPERLANE_STORE.as_bytes(),
                &store_root,
            ),
            "the store step does not verify against the app hash"
        );

        Ok(Tree {
            address: hook_id,
            tree: decode_hook(&hook_bytes)?,
        })
    }
}

/// What the coprocessor needs too: which state a store is at, and what the ISM commits to.
impl CelestiaStore {
    /// The trusted header's app hash, which commits to the state *one block earlier*:
    /// `header[H].app_hash` is the result of executing block `H-1`. So `height` is reported as
    /// `H-1`, and the ISM state names the state it actually describes.
    pub fn root(&self) -> anyhow::Result<AttestedRoot> {
        let header = &self.trusted.signed_header.header;
        let app_hash = header.app_hash.as_bytes();
        anyhow::ensure!(
            app_hash.len() == 32,
            "header {} has no app hash",
            header.height
        );
        Ok(AttestedRoot {
            state_root: B256::from_slice(app_hash),
            height: header.height.value().saturating_sub(1),
            timestamp: header.time.unix_timestamp() as u64,
        })
    }

    /// Commit to the trusted header and both validator sets. A commitment over the header
    /// alone would let a relayer swap in a different set and defeat the next verification.
    pub fn commitment(&self) -> [u8; 32] {
        let b = &self.trusted;
        let mut h = Sha256::new();
        h.update(b"tee-isms/celestia-store/v1");
        h.update(b.signed_header.header.chain_id.as_str().as_bytes());
        h.update(b.height().value().to_be_bytes());
        h.update(b.signed_header.header.hash().as_bytes());
        h.update(b.validators.hash().as_bytes());
        h.update(b.next_validators.hash().as_bytes());
        h.finalize().into()
    }
}

/// hyperlane-cosmos's `MerkleTreeHook` protobuf, of which only the tree matters.
fn decode_hook(bytes: &[u8]) -> anyhow::Result<MerkleTree> {
    #[derive(Clone, PartialEq, Message)]
    struct Hook {
        #[prost(message, optional, tag = "4")]
        tree: Option<HookTree>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct HookTree {
        #[prost(bytes = "vec", repeated, tag = "1")]
        branch: Vec<Vec<u8>>,
        #[prost(uint32, tag = "2")]
        count: u32,
    }
    let tree = Hook::decode(bytes)?
        .tree
        .ok_or_else(|| anyhow::anyhow!("merkle tree hook has no tree"))?;
    anyhow::ensure!(
        tree.branch.len() == TREE_DEPTH,
        "merkle tree hook branch has {} entries",
        tree.branch.len()
    );
    let mut branch = [[0u8; 32]; TREE_DEPTH];
    for (i, node) in tree.branch.iter().enumerate() {
        branch[i] = node
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("branch entry {i} is not 32 bytes"))?;
    }
    Ok(MerkleTree {
        branch,
        count: tree.count,
    })
}
