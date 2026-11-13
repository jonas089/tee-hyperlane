# Admin controls

Rotations: [MAINTAIN.md](MAINTAIN.md). Multisig before mainnet ([Celestia](#transfer-ownership), [EVM](#evm-transfer-ownership)).

## Who the owner is

| Where | Owner | Key |
|---|---|---|
| Celestia ISMs (`x/teeism`) | `relayer` | `--from relayer` (`devnet/.state/celestia`) |
| EVM ISMs (`TeeDcapIsm`), Automata | deployer | Ledger (`EVM_SIGNER=ledger`) or `EVM_PRIVATE_KEY` |

## What the owner controls

| Control | Celestia | EVM |
|---|---|---|
| Set the pinned enclaves (any one may attest) | `tx teeism set-identities` | `setEnclaves` |
| Transfer ownership | `tx teeism update-owner` | `transferOwnership` |
| New Intel root CA | `set-verifier-overrides --trusted-root-cert` | new PCCS `PcsDao` |
| New quote version | v4, v5 built in; later: celestia-app upgrade | `setQuoteVerifier` (`teeism pccs deploy` registers v4 and v5) |
| Stop an ISM | pin an unused identity | `setEnclaves` with unused measurements |

## Setup

On ark as `chef`:

```sh
cd ~/tee-ism-nonzk/devnet
D=.state/deployment.json               # every id `teeism` created
A=.state/bin/celestia-appd; H="--home .state/celestia --keyring-backend test"
TX="$H --chain-id teeism-local --node http://localhost:26657 --gas auto --gas-adjustment 1.3 --gas-prices 0.002utia -y"
ORIGIN=base                            # any of sepolia, arbitrum, base, eden
ENCLAVE=$ORIGIN                        # the enclave that attests it: ethereum for sepolia
ISM=$(jq -r .celestia.isms.$ORIGIN $D)
```

Inspect:

```sh
$A q teeism ism $ISM --node http://localhost:26657 -o json | jq '.ism | {owner, identity, verifier_overrides}'
```

## Celestia

### Re-pin the enclave identity

An ISM pins a set of 1 to 8 enclaves; any one may attest. `set-identities` replaces the whole set, so pass every enclave that should stay. `teeism isms celestia` does this for every ISM `relayer` owns, from the enclaves recorded for the origin. By hand:

**Payload:** one identity file per enclave, `.state/identity/<enclave>.json` and `.state/identity/<enclave>@<target>-<n>.json` for one added with `--add` (`teeism enclaves up` or `teeism enclaves identity`), or read a live enclave:

```sh
../tee-hyperlane/target/release/tee-hyperlane identity --url $(jq -r ".enclaves.chains.$ENCLAVE[0].url" $D) --json /tmp/identity.json
.state/bin/teeism-identity -identity /tmp/identity.json      # its digest, to compare with `teeism status`
.state/bin/teeism-identity -identity a.json -identity b.json # the set's digest, which the ISM state carries
$A tx teeism set-identities $ISM a.json b.json --from relayer $TX
```

### Transfer ownership

```sh
NEW=celestia1...                       # a multisig, see below
$A tx teeism update-owner $ISM $NEW --from relayer $TX
```

Multisig owner:

```sh
$A keys add admin --multisig signer1,signer2,signer3 --multisig-threshold 2 $H
$A tx teeism set-identities $ISM /tmp/identity.json --from admin --generate-only $TX > unsigned.json
$A tx sign unsigned.json --from signer1 --multisig $($A keys show admin -a $H) $TX > s1.json   # each signer
$A tx multisign unsigned.json admin s1.json s2.json $TX > signed.json
$A tx broadcast signed.json --node http://localhost:26657
```

**Warning:** `teeism isms celestia` only re-pins `relayer`-owned ISMs and replaces the rest. Re-pin from the new owner first.

### Update the Intel root CA

`--trusted-root-cert`: repeatable, PEM/DER, replaces prior. Only `Intel SGX Root CA`, P-256. Without an override the module trusts go-tdx-guest's built-in root ([DEPLOY B](DEPLOY.md#b-intel-root-ca-and-collateral)).

**Payload:** URL below, or last cert of a `teeism-collateral` issuer chain. Check the fingerprint against Intel's announcement; today's is `44:A0:19:6B:...:FA:B6:74:D3`.

```sh
curl -sO https://certificates.trustedservices.intel.com/Intel_SGX_Provisioning_Certification_RootCA.cer
openssl x509 -inform der -in Intel_SGX_Provisioning_Certification_RootCA.cer -noout -subject -fingerprint -sha256
$A tx teeism set-verifier-overrides $ISM --trusted-root-cert Intel_SGX_Provisioning_Certification_RootCA.cer --from relayer $TX
$A tx teeism set-verifier-overrides $ISM --clear --from relayer $TX     # back to the built-in root
```

### Stop an ISM

Pin an identity no enclave has; re-pin the real set to resume.

```sh
jq '.compose_hash = "0x" + ("00" * 32)' /tmp/identity.json > /tmp/stopped.json
$A tx teeism set-identities $ISM /tmp/stopped.json --from relayer $TX
```

## EVM

`teeism` signs these with `EVM_SIGNER` (`devnet/.env`). By hand:

```sh
SIGN="--ledger --from $(cast wallet address --ledger)"   # or: SIGN="--private-key $EVM_PRIVATE_KEY"
CHAIN=base                                 # any of arbitrum, base, sepolia, eden
ISM=$(jq -r .evm.$CHAIN.ism $D)
RPC=https://sepolia.base.org               # the chain's endpoint (DEPLOY G)
cast call $ISM "owner()(address)" --rpc-url $RPC
cast call $ISM "VERSION()(uint8)" --rpc-url $RPC
cast call $ISM "enclaves()(bytes32[],bytes32[])" --rpc-url $RPC   # pinned measurements, identity digests
```

### Re-pin the enclave identity

`teeism isms evm` does this for every ISM its signer owns. `VERSION` other than this checkout's: it replaces the ISM from its last state, and `teeism warp evm` re-points the routers.

**Payload:** for each Celestia enclave to pin, its `keccak256(mr_td ++ mr_config_id ++ rtmr0 ++ rtmr1 ++ rtmr2)` and identity digest, read from its quote by `teeism enclaves up` (or `teeism enclaves identity`). `setEnclaves` replaces the whole set; the identities must be ascending, each measurement at the same position as its identity:

```sh
PAIRS=$(jq -r '.enclaves.chains.celestia | map({i: .identity_digest, m: .measurements}) | unique_by(.i) | sort_by(.i)' $D)
MEASUREMENTS=$(jq -r 'map(.m) | "[" + join(",") + "]"' <<<"$PAIRS")
IDENTITIES=$(jq -r 'map(.i) | "[" + join(",") + "]"' <<<"$PAIRS")
cast send $ISM "setEnclaves(bytes32[],bytes32[])" "$MEASUREMENTS" "$IDENTITIES" --rpc-url $RPC $SIGN
```

### <a name="evm-transfer-ownership"></a>Transfer ownership

Also: routers, Eden's mailbox, the Automata entrypoint, router and PCCS storage and DAOs.

```sh
cast send $ISM "transferOwnership(address)" $SAFE --rpc-url $RPC $SIGN
```

**Warning:** `teeism isms evm` only re-pins ISMs its signer owns and replaces the rest. Re-pin from the Safe first.

### Intel's side on EVM

- **Collateral:** PCCS `upsert*`, monthly (`teeism pccs seed`, [MAINTAIN](MAINTAIN.md#monthly-intel-collateral)). Needs `ATTESTER_ROLE` on each versioned DAO and the TCB evaluation DAO; `teeism pccs deploy` grants it to `EVM_PRIVATE_KEY`'s address.
- **TCB status:** only `UpToDate`, `SWHardeningNeeded`; else new ISM version.

## <a name="tcb-recovery"></a>TCB recovery (Intel moves to a new evaluation number)

```sh
curl -s https://api.trustedservices.intel.com/tdx/certification/v4/tcbevaluationdatanumbers \
  | jq '.tcbEvaluationDataNumbers.tcbEvalNumbers[] | {n: .tcbEvaluationDataNumber, released: .tcbRecoveryEventDate}'
curl -s "https://api.trustedservices.intel.com/tdx/certification/v4/tcb?fmspc=20a06f000000" | jq .tcbInfo.tcbEvaluationDataNumber   # standard today
```

Unpatched hosts go `OutOfDate`, routes stop. Re-pin won't help.

- **Both:** Phala patches the host, or move CVM. Ask early.
- **EVM, beforehand:** `teeism pccs tcb-eval <number>` (owner), which also seeds ([MAINTAIN](MAINTAIN.md)).

## When something breaks live

| What happens | Celestia | EVM |
|---|---|---|
| New image, platform update, new CVM | re-pin (`teeism isms celestia`) | re-pin (`teeism isms evm`) |
| Fresh collateral | nothing | monthly `teeism pccs seed` |
| PCS down, collateral expires | routes stop until Intel publishes again | routes stop 30 days after the last seed; seed when back |
| [TCB recovery](#tcb-recovery) | patch host | patch host, new DAOs |
| PCK certificate revoked | new CVM, re-pin | same |
| Root CA rotated, same name/key type | `--trusted-root-cert` | a new PCCS: change `ROOT_CA_PUBKEY_HASH`, `teeism pccs deploy --namespace tee-isms/v3`; `teeism isms evm` replaces each ISM from its last state, `teeism warp evm` re-points the routers |
| Other root/name change; new collateral format (TCB info v4, QE identity v3) | **not fixable live:** celestia-app upgrade | new Automata verifier/DAOs |
| Quote v5 | nothing | nothing: v5 verifier registered by `teeism pccs deploy` |
| Quote after v5 | **not fixable live:** celestia-app upgrade | `setQuoteVerifier` |
| TDX broken | stop the ISMs | same |
| Owner key compromised | transfer ownership | same |
| Owner key lost (unrecoverable) | new ISM, re-point route | new ISM, `setInterchainSecurityModule` |
