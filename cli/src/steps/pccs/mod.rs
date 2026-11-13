//! Our Automata on-chain PCCS and DCAP verifier on each EVM chain: deploy, seed with Intel's
//! collateral, report its expiry, and check a quote against it.
//!
//! Deployed from Automata's sources, which `teeism build` fetches at pinned commits into
//! tee-hyperlane/contracts/vendor/automata (gitignored), through the CREATE2 deployer. Every
//! salt is `keccak256("<namespace>:<NAME>")`, so the addresses follow from the namespace, the
//! owner and the code, and a new namespace is a new, independent deployment. Each piece is
//! checked on chain before it is sent, so a run that stops anywhere is finished by running it
//! again.
//!
//! Layout: the CRL v2 PCS and PCK DAOs (`PcsDaoV2`, `PckDaoV2`, `X509CRLHelperV2`) behind a
//! `PccsDependencyConfig`, the TCB evaluation DAO and the versioned QE identity DAO that read
//! through it, and the versioned TCB info DAO that takes a single `upsertFmspcTcb`. The router
//! the quote verifiers read from points at all of them.

use std::path::PathBuf;

use alloy::sol;
use anyhow::{Context, Result};

use crate::shared::chains::EvmChain;
use crate::shared::record::{Deployment, Pccs};
use crate::shared::settings::Ctx;

pub mod deploy;
pub mod seed;
pub mod status;

pub const DEFAULT_NAMESPACE: &str = "tee-isms/v2";
/// Phala's hosts.
pub const DEFAULT_FMSPC: &str = "20a06f000000";
/// The role that may upload collateral to a DAO; it can only upload what Intel signed.
const ATTESTER_ROLE: u64 = 1;

// The contract functions every PCCS step calls.
sol! {
    #[sol(rpc)]
    interface IStorage {
        function grantDao(address granted) external;
        function isAuthorizedCaller(address caller) external view returns (bool);
        function setCallerAuthorization(address caller, bool authorized) external;
        function readAttestation(bytes32 attestationId) external view returns (bytes memory);
        function owner() external view returns (address);
    }

    #[sol(rpc)]
    interface IDependencyConfig {
        function initialize(address initialPcsDao, address initialCrlHelper) external;
        function pcsDao() external view returns (address);
        function crlHelper() external view returns (address);
    }

    #[sol(rpc)]
    interface ICrlHelperV2 {
        function authorizedIndexers(address indexer) external view returns (bool);
        function setAuthorizedIndexer(address indexer, bool authorized) external;
    }

    #[sol(rpc)]
    interface IRoles {
        function grantRoles(address user, uint256 roles) external;
        function hasAnyRole(address user, uint256 roles) external view returns (bool);
    }

    #[sol(rpc)]
    interface IVersioned {
        function TCB_EVALUATION_NUMBER() external view returns (uint32);
    }

    #[sol(rpc)]
    interface IRouter {
        function setAuthorized(address caller, bool authorized) external;
        function setQeIdDaoVersionedAddr(uint32 tcbEval, address addr) external;
        function setFmspcTcbDaoVersionedAddr(uint32 tcbEval, address addr) external;
        function qeIdDaoVersionedAddr(uint32 tcbEval) external view returns (address);
        function fmspcTcbDaoVersionedAddr(uint32 tcbEval) external view returns (address);
        function getCertHash(uint8 ca) external view returns (bytes32);
        function owner() external view returns (address);
    }

    #[sol(rpc)]
    interface IEntrypoint {
        function setQuoteVerifier(address verifier) external;
        function quoteVerifiers(uint16 version) external view returns (address);
        function verifyAndAttestOnChain(bytes calldata rawQuote) external payable returns (bool success, bytes memory output);
    }

    struct JsonObj { string body; bytes signature; }

    #[sol(rpc)]
    interface ICollateral {
        function upsertPcsCertificates(uint8 ca, bytes calldata cert) external returns (bytes32);
        function upsertRootCACrl(bytes calldata rootcacrl) external returns (bytes32);
        function upsertPckCrl(uint8 ca, bytes calldata crl) external returns (bytes32);
        function upsertTcbEvaluationData(JsonObj calldata tcbEvalObj) external returns (bytes32);
        function upsertEnclaveIdentity(uint256 id, uint256 version, JsonObj calldata enclaveIdentityObj) external returns (bytes32);
        function upsertFmspcTcb(JsonObj calldata tcbInfoObj) external returns (bytes32);

        function getCollateralValidity(bytes32 key) external view returns (uint64, uint64);
        function PCS_KEY(uint8 ca, bool isCrl) external pure returns (bytes32);
        function TCB_EVAL_KEY(uint8 id) external view returns (bytes32);
        function ENCLAVE_ID_KEY(uint256 id, uint256 version) external view returns (bytes32);
        function FMSPC_TCB_KEY(uint8 tcbType, bytes6 fmspc, uint32 version) external view returns (bytes32);
    }
}

/// CA enum in the PCCS: ROOT=0, PROCESSOR=1, PLATFORM=2, SIGNING=3.
mod ca {
    pub const ROOT: u8 = 0;
    pub const PROCESSOR: u8 = 1;
    pub const PLATFORM: u8 = 2;
    pub const SIGNING: u8 = 3;
}

/// Where `teeism build` checks out Automata's two repositories. Gitignored: nothing of theirs
/// is committed here.
pub fn automata_dir(ctx: &Ctx) -> PathBuf {
    ctx.repo.join("tee-hyperlane/contracts/vendor/automata")
}

/// The forge project of `automata-on-chain-pccs`.
pub fn pccs_dir(ctx: &Ctx) -> PathBuf {
    automata_dir(ctx).join("pccs")
}

/// The forge project of `automata-dcap-attestation`, which sits in its `evm/` directory.
pub fn dcap_dir(ctx: &Ctx) -> PathBuf {
    automata_dir(ctx).join("dcap/evm")
}

pub fn pccs_for<'a>(d: &'a Deployment, chain: &EvmChain) -> Result<&'a Pccs> {
    d.evm(chain.name).and_then(|e| e.pccs.as_ref()).with_context(|| {
        format!(
            "no PCCS on {}; run `teeism pccs deploy --chain {}`",
            chain.label, chain.name
        )
    })
}
