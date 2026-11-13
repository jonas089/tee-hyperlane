//! `teeism pccs deploy` and `teeism pccs tcb-eval`: Automata's PCCS and DCAP verifier through the
//! CREATE2 deployer, salts `keccak256("<namespace>:<NAME>")`. Each contract is checked on chain
//! first, so a run that stops anywhere is finished by running it again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use alloy::primitives::{address, Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{bail, Context, Result};

use super::{
    ca, dcap_dir, pccs_dir, seed, ICrlHelperV2, IDependencyConfig, IEntrypoint, IRoles, IRouter, IStorage,
    IVersioned, ATTESTER_ROLE,
};
use crate::shared::evm::{salt, Artifact, Chain};
use crate::shared::record::{Pccs, VersionedDaos};
use crate::shared::settings::Ctx;
use crate::shared::signer::Signer;
use crate::shared::{chains, print};
use crate::steps::pccs::seed::{evals_to_deploy, Intel};

const RIP7212_P256: Address = address!("0000000000000000000000000000000000000100");
const DAIMO_P256: Address = address!("c2b78104907F722DABAc4C69f826a522B2754De4");

pub(super) struct Code {
    pub(super) pccs: PathBuf,
    pub(super) dcap: PathBuf,
}

impl Code {
    fn pccs(&self, file: &str, name: &str) -> Result<Artifact> {
        Artifact::load(&self.pccs, file, name)
    }
    fn dcap(&self, name: &str) -> Result<Artifact> {
        Artifact::load(&self.dcap, name, name)
    }
}

/// The P-256 verifier the chain offers: the RIP-7212 precompile, else Daimo's contract.
async fn p256(c: &Chain) -> Result<Address> {
    // Automata's own self-test vector: sha256(message), signature, public key.
    let msg = hex::decode("a9b4ac5fb82203536c408b1db1d0338c61fd0064ea2471794d435fc0e03c217f")?;
    let sig = hex::decode("8c6a3bb0346ec08d01b6351eeff099fd7131de48e5e569dbcd9dc3f29e08995692db2eaebd633a52fff4915d274859bbc241967c6ce3a6831e754b88066fc534")?;
    let key = hex::decode("710f9d7cb59f86798aaf92138320831b778016d02cf0f5b416a76917f85edd4d7440615935921eaaa33c66c6cf4b745e70176a391610ab14f845d7ff39b112a3")?;
    use sha2::Digest;
    let mut input = sha2::Sha256::digest(&msg).to_vec();
    input.extend(sig);
    input.extend(key);
    for verifier in [RIP7212_P256, DAIMO_P256] {
        let tx = TransactionRequest::default()
            .to(verifier)
            .input(Bytes::from(input.clone()).into());
        if let Ok(ret) = c.p.call(tx).await {
            if ret.len() == 32 && ret[31] == 1 {
                return Ok(verifier);
            }
        }
    }
    bail!(
        "{} verifies P-256 neither through the RIP-7212 precompile nor Daimo's contract; \
         the DCAP verifier cannot run there (a chain before Osaka has no precompile)",
        c.chain.label
    )
}

/// Whether `dao` may write to `storage`. The storage has no getter for its writers, but
/// `readAttestation` is restricted to them and reverts FORBIDDEN for anyone else.
async fn is_writer(c: &Chain, storage: Address, dao: Address) -> Result<bool> {
    let data = IStorage::readAttestationCall {
        attestationId: FixedBytes::ZERO,
    }
    .abi_encode();
    let tx = TransactionRequest::default()
        .from(dao)
        .to(storage)
        .input(Bytes::from(data).into());
    match c.p.call(tx).await {
        Ok(_) => Ok(true),
        Err(e) if e.to_string().contains("FORBIDDEN") => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "could not tell whether {dao} may write to the PCCS storage {storage}: {e}"
        )),
    }
}

/// Whether the router serves `caller`. Like the storage, it reverts `Forbidden()` (0xee90c468)
/// for an unauthorized caller and has no getter.
async fn router_serves(c: &Chain, router: Address, caller: Address) -> Result<bool> {
    let data = IRouter::getCertHashCall { ca: ca::ROOT }.abi_encode();
    let tx = TransactionRequest::default()
        .from(caller)
        .to(router)
        .input(Bytes::from(data).into());
    match c.p.call(tx).await {
        Ok(_) => Ok(true),
        Err(e) if e.to_string().contains("ee90c468") => Ok(false),
        // Anything else (a missing certificate, say) means it got past the caller check.
        Err(_) => Ok(true),
    }
}

async fn grant_writer(c: &Chain, storage: Address, dao: Address, name: &str) -> Result<()> {
    if !is_writer(c, storage, dao).await? {
        let data = IStorage::grantDaoCall { granted: dao }.abi_encode();
        c.send(
            &format!("letting {name} write to the PCCS storage"),
            storage,
            data.into(),
        )
        .await?;
    }
    Ok(())
}

async fn grant_attester(c: &Chain, dao: Address, who: Address, name: &str) -> Result<()> {
    let roles = IRoles::new(dao, &c.p);
    let has = roles
        .hasAnyRole(who, U256::from(ATTESTER_ROLE))
        .call()
        .await
        .with_context(|| format!("reading {who}'s role on {name} {dao}"))?;
    if !has {
        let data = IRoles::grantRolesCall {
            user: who,
            roles: U256::from(ATTESTER_ROLE),
        }
        .abi_encode();
        c.send(
            &format!("granting {who} ATTESTER_ROLE on {name}"),
            dao,
            data.into(),
        )
        .await?;
    }
    Ok(())
}

/// The quote verifier salt Automata derives per version: `keccak256(abi.encodePacked("<ns>:QUOTE_VERIFIER_V", uint16))`.
fn verifier_salt(namespace: &str, version: u16) -> FixedBytes<32> {
    let mut packed = format!("{namespace}:QUOTE_VERIFIER_V").into_bytes();
    packed.extend_from_slice(&version.to_be_bytes());
    alloy::primitives::keccak256(packed)
}

/// Deploy, or finish deploying, the PCCS and DCAP verifier on one chain, with TCB info and QE
/// identity contracts for each of `evals`. `seeder` is granted the role that uploads
/// collateral (it needs nothing else).
pub async fn deploy(
    ctx: &Ctx,
    c: &Chain,
    namespace: &str,
    evals: &BTreeSet<u32>,
    seeder: Address,
    existing: Option<&Pccs>,
) -> Result<Pccs> {
    let code = Code {
        pccs: pccs_dir(ctx),
        dcap: dcap_dir(ctx),
    };
    let ns = namespace;
    let owner = c.from;
    let p256 = p256(c).await?;
    print::info(format!("P-256 verifier {p256}, owner {owner}, namespace {ns}"));
    // A contract this namespace has recorded is taken from the record once the chain has code
    // at that address. The DCAP tree's artifacts used to carry a metadata hash that depended
    // on the build environment, so the live router, entrypoint and verifiers do not reproduce
    // from this tree; the PCCS tree's never carried one, and now neither does the DCAP tree's.
    let recorded = existing.filter(|p| p.namespace == ns);
    let rec = |field: fn(&Pccs) -> Address| recorded.map(field);

    let helper = |file: &'static str, salt_name: &'static str, recorded: Option<Address>| {
        let code = &code;
        async move {
            let a = code.pccs(file, file)?;
            c.create2_or(file, recorded, salt(ns, salt_name), &a.initcode(&[]))
                .await
        }
    };
    let enclave_identity_helper = helper(
        "EnclaveIdentityHelper",
        "ENCLAVE_IDENTITY_HELPER_SALT",
        rec(|p| p.enclave_identity_helper),
    )
    .await?;
    let fmspc_tcb_helper = helper(
        "FmspcTcbHelper",
        "FMSPC_TCB_HELPER_SALT",
        rec(|p| p.fmspc_tcb_helper),
    )
    .await?;
    let pck_helper = helper("PCKHelper", "X509_HELPER_SALT", rec(|p| p.pck_helper)).await?;
    let crl_helper = helper("X509CRLHelper", "X509_CRL_HELPER_SALT", rec(|p| p.crl_helper)).await?;
    let tcb_eval_helper = helper(
        "TcbEvalHelper",
        "TCB_EVAL_HELPER_SALT",
        rec(|p| p.tcb_eval_helper),
    )
    .await?;

    let storage = c
        .create2_or(
            "AutomataDaoStorage",
            rec(|p| p.storage),
            salt(ns, "PCCS_STORAGE_SALT"),
            &code
                .pccs("AutomataDaoStorage", "AutomataDaoStorage")?
                .initcode(&owner.abi_encode()),
        )
        .await?;
    let crl_helper_v2 = c
        .create2_or(
            "X509CRLHelperV2",
            rec(|p| p.crl_helper_v2),
            salt(ns, "X509_CRL_HELPER_V2_SALT"),
            &code
                .pccs("X509CRLHelperV2", "X509CRLHelperV2")?
                .initcode(&owner.abi_encode()),
        )
        .await?;
    let dependency_config = c
        .create2_or(
            "PccsDependencyConfig",
            rec(|p| p.dependency_config),
            salt(ns, "PCCS_DEPENDENCY_CONFIG_SALT"),
            &code
                .pccs("PccsDependencyConfig", "PccsDependencyConfig")?
                .initcode(&owner.abi_encode()),
        )
        .await?;
    let pcs_dao = c
        .create2_or(
            "AutomataPcsDaoV2",
            rec(|p| p.pcs_dao),
            salt(ns, "PCS_DAO_V2_SALT"),
            &code
                .pccs("AutomataPcsDaoV2", "AutomataPcsDaoV2")?
                .initcode(&(storage, p256, pck_helper, dependency_config).abi_encode_params()),
        )
        .await?;

    let dep = IDependencyConfig::new(dependency_config, &c.p);
    let configured = dep
        .pcsDao()
        .call()
        .await
        .context("reading the dependency config's PCS DAO")?;
    if configured == Address::ZERO {
        let data = IDependencyConfig::initializeCall {
            initialPcsDao: pcs_dao,
            initialCrlHelper: crl_helper_v2,
        }
        .abi_encode();
        c.send(
            "initializing the PCCS dependency config",
            dependency_config,
            data.into(),
        )
        .await?;
    } else {
        let crl = dep
            .crlHelper()
            .call()
            .await
            .context("reading the dependency config's CRL helper")?;
        if configured != pcs_dao || crl != crl_helper_v2 {
            bail!(
                "the dependency config {dependency_config} points at PCS DAO {configured} and CRL helper {crl}, \
                 not {pcs_dao} and {crl_helper_v2}; it belongs to another deployment"
            );
        }
    }

    let pck_dao = c
        .create2_or(
            "AutomataPckDaoV2",
            rec(|p| p.pck_dao),
            salt(ns, "PCK_DAO_V2_SALT"),
            &code
                .pccs("AutomataPckDaoV2", "AutomataPckDaoV2")?
                .initcode(&(storage, p256, dependency_config, pck_helper).abi_encode_params()),
        )
        .await?;
    grant_writer(c, storage, pcs_dao, "the PCS DAO").await?;
    grant_writer(c, storage, pck_dao, "the PCK DAO").await?;
    let indexer = ICrlHelperV2::new(crl_helper_v2, &c.p);
    if !indexer
        .authorizedIndexers(pcs_dao)
        .call()
        .await
        .context("reading the CRL helper's indexers")?
    {
        let data = ICrlHelperV2::setAuthorizedIndexerCall {
            indexer: pcs_dao,
            authorized: true,
        }
        .abi_encode();
        c.send("letting the PCS DAO index CRLs", crl_helper_v2, data.into())
            .await?;
    }

    let tcb_eval_dao = c
        .create2_or(
            "AutomataTcbEvalDao",
            rec(|p| p.tcb_eval_dao),
            salt(ns, "TCB_EVAL_DAO_SALT"),
            &code.pccs("AutomataTcbEvalDao", "AutomataTcbEvalDao")?.initcode(
                &(
                    storage,
                    p256,
                    dependency_config,
                    tcb_eval_helper,
                    pck_helper,
                    owner,
                )
                    .abi_encode_params(),
            ),
        )
        .await?;
    grant_writer(c, storage, tcb_eval_dao, "the TCB evaluation DAO").await?;
    grant_attester(c, tcb_eval_dao, seeder, "the TCB evaluation DAO").await?;

    let router = c
        .create2_or(
            "PCCSRouter",
            rec(|p| p.router),
            salt(ns, "PCCS_ROUTER_SALT"),
            &code.dcap("PCCSRouter")?.initcode(
                &(
                    owner,
                    tcb_eval_dao,
                    pcs_dao,
                    pck_dao,
                    pck_helper,
                    crl_helper_v2,
                    fmspc_tcb_helper,
                )
                    .abi_encode_params(),
            ),
        )
        .await?;
    let st = IStorage::new(storage, &c.p);
    if !st
        .isAuthorizedCaller(router)
        .call()
        .await
        .context("reading the storage's readers")?
    {
        let data = IStorage::setCallerAuthorizationCall {
            caller: router,
            authorized: true,
        }
        .abi_encode();
        c.send("letting the router read the PCCS storage", storage, data.into())
            .await?;
    }

    let entrypoint = c
        .create2_or(
            "AutomataDcapAttestationFee",
            rec(|p| p.entrypoint),
            salt(ns, "DCAP_ATTESTATION_SALT"),
            &code
                .dcap("AutomataDcapAttestationFee")?
                .initcode(&owner.abi_encode()),
        )
        .await?;

    let mut verifiers = [Address::ZERO; 2];
    for (i, version) in [4u16, 5].into_iter().enumerate() {
        let name = format!("V{version}QuoteVerifier");
        let v = c
            .create2_or(
                &name,
                recorded.map(|p| {
                    if version == 4 {
                        p.v4_verifier
                    } else {
                        p.v5_verifier
                    }
                }),
                verifier_salt(ns, version),
                &code.dcap(&name)?.initcode(&(p256, router).abi_encode_params()),
            )
            .await?;
        if !router_serves(c, router, v).await? {
            let data = IRouter::setAuthorizedCall {
                caller: v,
                authorized: true,
            }
            .abi_encode();
            c.send(&format!("letting {name} read the router"), router, data.into())
                .await?;
        }
        let current = IEntrypoint::new(entrypoint, &c.p)
            .quoteVerifiers(version)
            .call()
            .await
            .with_context(|| format!("reading the entrypoint's v{version} verifier"))?;
        if current != v {
            let data = IEntrypoint::setQuoteVerifierCall { verifier: v }.abi_encode();
            c.send(
                &format!("registering {name} on the entrypoint"),
                entrypoint,
                data.into(),
            )
            .await?;
        }
        verifiers[i] = v;
    }

    let mut pccs = Pccs {
        namespace: ns.to_string(),
        owner,
        p256,
        enclave_identity_helper,
        fmspc_tcb_helper,
        pck_helper,
        crl_helper,
        tcb_eval_helper,
        storage,
        crl_helper_v2,
        dependency_config,
        pcs_dao,
        pck_dao,
        tcb_eval_dao,
        router,
        entrypoint,
        v4_verifier: verifiers[0],
        v5_verifier: verifiers[1],
        evals: BTreeMap::new(),
    };
    for n in evals {
        let daos = deploy_eval(&code, c, &pccs, *n, seeder).await?;
        pccs.evals.insert(*n, daos);
    }
    Ok(pccs)
}

/// The QE identity and TCB info contracts for one TCB evaluation number, registered on the
/// router. The verifier reads Intel's "standard" number by block time, and moves to the next
/// twelve months after Intel releases it, so the next number needs these before that day.
pub(super) async fn deploy_eval(
    code: &Code,
    c: &Chain,
    p: &Pccs,
    n: u32,
    seeder: Address,
) -> Result<VersionedDaos> {
    let qe_identity = c
        .create2(
            &format!("AutomataEnclaveIdentityDaoVersioned ({n})"),
            salt(&p.namespace, "ENCLAVE_ID_DAO_SALT"),
            &code
                .pccs(
                    "AutomataEnclaveIdentityDaoVersioned",
                    "AutomataEnclaveIdentityDaoVersioned",
                )?
                .initcode(
                    &(
                        p.storage,
                        p.p256,
                        p.dependency_config,
                        p.enclave_identity_helper,
                        p.pck_helper,
                        p.owner,
                        n,
                    )
                        .abi_encode_params(),
                ),
        )
        .await?;
    let fmspc_tcb = c
        .create2(
            &format!("AutomataFmspcTcbDaoVersioned ({n})"),
            salt(&p.namespace, "FMSPC_TCB_DAO_SALT"),
            &code
                .pccs("AutomataFmspcTcbDaoVersioned", "AutomataFmspcTcbDaoVersioned")?
                .initcode(
                    &(
                        p.storage,
                        p.p256,
                        p.pcs_dao,
                        p.fmspc_tcb_helper,
                        p.pck_helper,
                        p.crl_helper,
                        p.owner,
                        n,
                    )
                        .abi_encode_params(),
                ),
        )
        .await?;
    for (dao, name) in [(qe_identity, "QE identity DAO"), (fmspc_tcb, "TCB info DAO")] {
        let got = IVersioned::new(dao, &c.p)
            .TCB_EVALUATION_NUMBER()
            .call()
            .await
            .with_context(|| format!("reading the evaluation number of {dao}"))?;
        if got != n {
            bail!("{name} {dao} is built for evaluation {got}, not {n}");
        }
        grant_writer(c, p.storage, dao, &format!("the evaluation-{n} {name}")).await?;
        grant_attester(c, dao, seeder, &format!("the evaluation-{n} {name}")).await?;
    }
    let router = IRouter::new(p.router, &c.p);
    if router
        .qeIdDaoVersionedAddr(n)
        .call()
        .await
        .context("reading the router's QE identity DAOs")?
        != qe_identity
    {
        let data = IRouter::setQeIdDaoVersionedAddrCall {
            tcbEval: n,
            addr: qe_identity,
        }
        .abi_encode();
        c.send(
            &format!("registering the evaluation-{n} QE identity DAO"),
            p.router,
            data.into(),
        )
        .await?;
    }
    if router
        .fmspcTcbDaoVersionedAddr(n)
        .call()
        .await
        .context("reading the router's TCB info DAOs")?
        != fmspc_tcb
    {
        let data = IRouter::setFmspcTcbDaoVersionedAddrCall {
            tcbEval: n,
            addr: fmspc_tcb,
        }
        .abi_encode();
        c.send(
            &format!("registering the evaluation-{n} TCB info DAO"),
            p.router,
            data.into(),
        )
        .await?;
    }
    Ok(VersionedDaos {
        qe_identity,
        fmspc_tcb,
    })
}

/// `teeism pccs deploy`: for each chain, deploy (or finish deploying) the PCCS and verifier,
/// record it, then seed it.
pub async fn run(ctx: &Ctx, chains: &[String], namespace: Option<String>, fmspc: &str) -> Result<()> {
    let wanted = evals_to_deploy(&Intel::new(), fmspc).await?;
    for chain in chains::evm_selection(chains)? {
        let mut d = ctx.deployment()?;
        let existing = d.evm(chain.name).and_then(|e| e.pccs.clone());
        let ns = namespace
            .clone()
            .or_else(|| existing.as_ref().map(|p| p.namespace.clone()))
            .unwrap_or_else(|| super::DEFAULT_NAMESPACE.to_string());
        // Keep every evaluation number this deployment already serves.
        let mut evals: BTreeSet<u32> = wanted.clone();
        if let Some(p) = existing.as_ref().filter(|p| p.namespace == ns) {
            evals.extend(p.evals.keys());
        }
        print::step(format!(
            "PCCS on {} (namespace {ns}, evaluations {evals:?})",
            chain.label
        ));
        let c = Chain::connect(ctx, chain).await?;
        let p = deploy(
            ctx,
            &c,
            &ns,
            &evals,
            Signer::hot_key(ctx)?.address(),
            existing.as_ref(),
        )
        .await?;
        d.evm_mut(chain.name).pccs = Some(p.clone());
        ctx.save(&d)?;
        print::step(format!("seeding {}", chain.label));
        seed::publish(&seed::seeder(ctx, chain).await?, &p, fmspc, false).await?;
        print::ok(format!("{}: entrypoint {}", chain.label, p.entrypoint));
    }
    Ok(())
}

// ---------------------------------------------------------------- teeism pccs tcb-eval

/// `teeism pccs tcb-eval <n>`: contracts for one more TCB evaluation number on each chain,
/// registered on its router and seeded.
pub async fn tcb_eval(ctx: &Ctx, chains: &[String], number: u32, fmspc: &str) -> Result<()> {
    let (published, _) = Intel::new().tdx_evals().await?;
    if !published.contains(&number) {
        bail!("Intel does not publish TDX evaluation {number} (it publishes {published:?})");
    }
    for chain in chains::evm_selection(chains)? {
        let mut d = ctx.deployment()?;
        let mut p = super::pccs_for(&d, chain)?.clone();
        print::step(format!("evaluation {number} on {}", chain.label));
        let c = Chain::connect(ctx, chain).await?;
        add_eval(ctx, &c, &mut p, number, Signer::hot_key(ctx)?.address()).await?;
        d.evm_mut(chain.name).pccs = Some(p.clone());
        ctx.save(&d)?;
        seed::publish(&seed::seeder(ctx, chain).await?, &p, fmspc, false).await?;
    }
    Ok(())
}

/// Add contracts for one more evaluation number to an existing deployment.
pub async fn add_eval(ctx: &Ctx, c: &Chain, p: &mut Pccs, n: u32, seeder: Address) -> Result<()> {
    let code = Code {
        pccs: pccs_dir(ctx),
        dcap: dcap_dir(ctx),
    };
    let daos = deploy_eval(&code, c, p, n, seeder).await?;
    p.evals.insert(n, daos);
    Ok(())
}
