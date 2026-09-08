# TEE Hyperlane Migration
The goal is to establish a parallel setup with new ISMs deployed to EVM chains (Celestia<=>Ethereum, Celestia<=>Arbitrum, Celestia<=>Base); starting with Ethereum.
Long-term Celestia wants to fully get rid of Hyperlane Validators and Relayers, hence this codebase should verify AND submit messages to the destination. Just cache
all messages and relay them alongside with the state transition proof that is submitted to the ISM (in the Celestia case it is a ZKISM).

# Context (see context directory):
celestia-app; the Celestia node with the ZKISM module that we want to re-use for the TEE ISM; similar to what's already done and tested in celestia-zkevm's TEE path with evolve-tee.
Important: For this to work with the latest Celestia you must use the appropriate version of SP1. The local clone of celestia-app is not yet live and uses V6.x of SP1, but for now
against live Mocha/Mainnet we must still use 5.x. Use the link in the Phala context to scrape the docs and learn how to write phala instance code & deploy / interact with instances.
Always choose the cheapest possible CPU (never GPU) instance for the nodes you run.

# Keys
I have provided you with a phala API key that you can use to deploy instances of our new docker image & test / run it.
I have provided you with a mocha testnet funded Celestia API key for ISM deployments & test transactions.
I have provided you with a Sepolia funded EVM key that has USDC and Ethereum. 

The test case for all our routes will be BOTH TIA and USDC NATIVE<=>SYNTHETIC. TIA is native on Celestia, USDC is native on Sepolia.

# What I want you to do and how I want you to do it
In this parent directory, I want you to create new projects. Everything in context is just context; you must never import from context and if you want to re-use parts of the code you will
have to cleanly re-write them in our new project. The new projects are:

tee-circuit (the SP1 circuit to wrap the attestation; similar to what we have in evolve-tee but double-checked and updated since evolve-tee has aged;
tee-hyperlane (the core service that contains all code for the phala instances & the harness to deploy them & the live service that reads results from them and submits new attestations + messages
to destination chain's mailboxes/ISMs;

Goal: Setup 2 Phala Nodes; one for Ethereum Sepolia and one for Celestia Mocha; that update the state roots of the TEE/ZK ISMs on the destination chains & submit verified messages with merkle 
inclusion proofs. The test case for these messages are USDC and TIA bridge transactions between Mocha and Sepolia. Order of tests:
TIA Mocha => Sepolia; TIA Sepolia => Mocha; USDC Sepolia => Mocha; USDC Mocha => Sepolia. Test cases must be validated and it must be ensured that they actually arrive.

# Style conventions / do's and dont's
This repo is NOT just a demo. It is a CPU-prover & TEE production service that values simplicity and predictability over event-driven optimizations. It must be easily extensible and 
already set the foundation for the Arbitrum & Base paths; since Arbitrum and Base roots are stored UNDER Ethereum in slots in the patricia Trie. Therefore it must be possible to re-use
the state root from the Ethereum slots and query an untrusted RPC for the hyperlane bridge transactions e.g. outbox and generate the TEE attestation + merkle inclusion proof for the Ethereum
root. You CAN already include this path for USDC Sepolia <=> Arbitrum Testnet and USDC Sepolia <=> BASE Testnet. But ensure that everything is properly cryptographically verified.
The goal is to have 2 Nodes / Light Consensus / Validation nodes wrapped in TEE instances on Phala that buy us the whole bridge stack for 4 networks (Ethereum, Arbitrum, Base, Celestia).
You don't have to touch ANY of the mainnets; just layout the whole infrastructure and test + verify it for the testnets.

The codebase should be human-first. Never introduce complex syntax. Have desciptive function names like "get_merkle_proof" "verify_arbitrum_root" "verify_base_root" "celestia_root" "verify_inclusion", ...;
It should be extremely clear where and how to extend it to new networks. Code comments should be minimal; the code should describe itself and comments should only add valuable additional context where they are truly needed and do so in a few short sentences max.
The readme should be succinct and minimal and just show how to run the e2e and how to submit transactions and verify they arrived via a SMALL CLI.

# Ask me questions when
- You believe security is at risk
- You need more testnet tokens (be conservative though; don't over-spend on tests)
- You are unsure about complexity and want to get it 100% right and stay within my "keep it simple" boundaries
- You have general questions about the architecture
- You are unsure what we're trying to prove; what the security constraints are; what needs verification; why we are doing this
