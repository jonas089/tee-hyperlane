//! Listens on Base's p2p network for blocks the sequencer signed.
//!
//! discv5 finds OP Stack peers that advertise Base's chain id, gossipsub carries the blocks.
//! Every block is checked with the enclave's own `verify_block` before it is kept, so only
//! blocks the enclave will accept are offered to it.
//!
//! Base's nodes accept at most 30 peers and deny the rest the moment they connect, so most are
//! full. The listener redials them as slots free up, and with a port configured it also
//! listens and advertises itself as a Base node, so a node with room can dial in.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::Bytes;
use discv5::{ConfigBuilder, Discv5, Enr, ListenConfig};
use enr::{CombinedKey, CombinedPublicKey, EnrPublicKey, NodeId};
use futures::StreamExt;
use libp2p::gossipsub::{self, IdentTopic, MessageAcceptance, MessageAuthenticity, ValidationMode};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{identify, identity, noise, ping, tcp, yamux, Multiaddr, PeerId, Swarm};
use serde_json::json;
use sha2::{Digest, Sha256};
use tee_node::chains::l2::base::{verify_block, Input, CHAIN_ID};
use tracing::{debug, info, warn};

use crate::origin::l2::sequenced::{Recent, SignedHead};

/// Base's bootnodes, from `base/node`'s `.env.sepolia`. The same nodes serve mainnet; peers are
/// told apart by the chain id in their `opstack` record.
const BOOTNODES: &[&str] = &[
    "enr:-J24QNz9lbrKbN4iSmmjtnr7SjUMk4zB7f1krHZcTZx-JRKZd0kA2gjufUROD6T3sOWDVDnFJRvqBBo62zuF-hYCohOGAYiOoEyEgmlkgnY0gmlwhAPniryHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQKNVFlCxh_B-716tTs-h1vMzZkSs1FTu_OYTNjgufplG4N0Y3CCJAaDdWRwgiQG",
    "enr:-J24QH-f1wt99sfpHy4c0QJM-NfmsIfmlLAMMcgZCUEgKG_BBYFc6FwYgaMJMQN5dsRBJApIok0jFn-9CS842lGpLmqGAYiOoDRAgmlkgnY0gmlwhLhIgb2Hb3BzdGFja4OFQgCJc2VjcDI1NmsxoQJ9FTIv8B9myn1MWaC_2lJ-sMoeCDkusCsk4BYHjjCq04N0Y3CCJAaDdWRwgiQG",
    "enr:-J24QDXyyxvQYsd0yfsN0cRr1lZ1N11zGTplMNlW4xNEc7LkPXh0NAJ9iSOVdRO95GPYAIc6xmyoCCG6_0JxdL3a0zaGAYiOoAjFgmlkgnY0gmlwhAPckbGHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQJwoS7tzwxqXSyFL7g0JM-KWVbgvjfB8JA__T7yY_cYboN0Y3CCJAaDdWRwgiQG",
    "enr:-J24QHmGyBwUZXIcsGYMaUqGGSl4CFdx9Tozu-vQCn5bHIQbR7On7dZbU61vYvfrJr30t0iahSqhc64J46MnUO2JvQaGAYiOoCKKgmlkgnY0gmlwhAPnCzSHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQINc4fSijfbNIiGhcgvwjsjxVFJHUstK9L1T8OTKUjgloN0Y3CCJAaDdWRwgiQG",
    "enr:-J24QG3ypT4xSu0gjb5PABCmVxZqBjVw9ca7pvsI8jl4KATYAnxBmfkaIuEqy9sKvDHKuNCsy57WwK9wTt2aQgcaDDyGAYiOoGAXgmlkgnY0gmlwhDbGmZaHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQIeAK_--tcLEiu7HvoUlbV52MspE0uCocsx1f_rYvRenIN0Y3CCJAaDdWRwgiQG",
];

/// Block topics since Ecotone (v3) and Isthmus (v4). Older versions carry no beacon root, and
/// `verify_block` reads only this layout.
const TOPIC_VERSIONS: [u8; 2] = [2, 3];
/// OP Stack gossip messages are up to 10 MiB.
const MAX_MESSAGE: usize = 10 * 1024 * 1024;
/// Enough peers that the mesh survives a few dropping.
const TARGET_PEERS: usize = 12;
const DISCOVER_EVERY: Duration = Duration::from_secs(5);
/// How long to leave a node that denied us before dialling it again.
const REDIAL_AFTER: Duration = Duration::from_secs(60);
/// Lookups kept running while short of peers. Base nodes are a small share of the network.
const LOOKUPS: usize = 4;

/// Start the listener in the background, and return where its blocks land.
pub fn start(port: u16) -> Arc<Recent<SignedHead>> {
    let recent = Arc::new(Recent::default());
    let out = recent.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(port, &out).await {
                warn!(error = %format!("{e:#}"), "base gossip listener stopped; restarting");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    recent
}

async fn run(port: u16, recent: &Recent<SignedHead>) -> anyhow::Result<()> {
    let keypair = identity::secp256k1::Keypair::generate();
    let mut secret = keypair.secret().to_bytes();
    let enr_key = CombinedKey::secp256k1_from_bytes(&mut secret)
        .map_err(|e| anyhow::anyhow!("discovery key: {e}"))?;
    // The `opstack` record is how Base nodes pick each other out of discovery. The address is
    // filled in by discovery once peers agree on what they see.
    let mut record = Enr::builder();
    record.add_value_rlp("opstack", opstack_record().into());
    if port != 0 {
        record.tcp4(port).udp4(port);
    }
    let local = record
        .build(&enr_key)
        .map_err(|e| anyhow::anyhow!("local record: {e}"))?;
    let config = ConfigBuilder::new(ListenConfig::Ipv4 {
        ip: Ipv4Addr::UNSPECIFIED,
        port,
    })
    .build();
    let mut discovery = Discv5::new(local, enr_key, config).map_err(|e| anyhow::anyhow!(e))?;
    discovery
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("discovery: {e}"))?;
    for node in BOOTNODES {
        let enr: Enr = node
            .parse()
            .map_err(|e| anyhow::anyhow!("bootnode record: {e}"))?;
        if let Err(e) = discovery.add_enr(enr) {
            debug!(error = e, "bootnode not added");
        }
    }

    let mut swarm = swarm(keypair)?;
    if port != 0 {
        swarm.listen_on(format!("/ip4/0.0.0.0/tcp/{port}").parse()?)?;
    }
    for v in TOPIC_VERSIONS {
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&topic(v))
            .map_err(|e| anyhow::anyhow!("subscribe: {e:?}"))?;
    }

    // Every node any lookup touches arrives here, far more than the routing table keeps.
    let mut found = discovery
        .event_stream()
        .await
        .map_err(|e| anyhow::anyhow!("discovery events: {e}"))?;
    let mut dialled: HashMap<PeerId, Instant> = HashMap::new();
    let mut discover = tokio::time::interval(DISCOVER_EVERY);
    let mut lookups: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut received = 0u64;
    let mut seen = 0u64;
    info!("listening for base blocks on p2p");
    loop {
        tokio::select! {
            _ = discover.tick() => {
                let peers = swarm.connected_peers().count();
                lookups.retain(|l| !l.is_finished());
                if peers < TARGET_PEERS {
                    while lookups.len() < LOOKUPS {
                        let query = discovery.find_node(NodeId::random());
                        lookups.push(tokio::spawn(async move {
                            let _ = query.await;
                        }));
                    }
                }
                debug!(peers, received, seen, "base gossip");
            }
            Some(event) = found.recv() => {
                if let discv5::Event::Discovered(enr) = event {
                    seen += 1;
                    if let Some((peer, addr)) = base_peer(&enr) {
                        let due = dialled.get(&peer).is_none_or(|t| t.elapsed() > REDIAL_AFTER);
                        if due && swarm.connected_peers().count() < TARGET_PEERS && !swarm.is_connected(&peer) {
                            dialled.insert(peer, Instant::now());
                            debug!(%peer, "dialling a base node");
                            let _ = swarm.dial(addr);
                        }
                    }
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                    debug!(peer = %peer_id, agent = info.agent_version, protocols = ?info.protocols, "identified");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, topic })) => {
                    debug!(peer = %peer_id, %topic, "peer subscribed");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message { propagation_source, message_id, message })) => {
                    let accepted = match signed_head(&message.data) {
                        Ok(head) => {
                            received += 1;
                            if received == 1 {
                                info!(height = head.height, "first signed base block from p2p");
                            }
                            recent.insert(head.height, head);
                            MessageAcceptance::Accept
                        }
                        Err(e) => {
                            debug!(error = %e, "ignoring a base gossip message");
                            MessageAcceptance::Ignore
                        }
                    };
                    swarm.behaviour_mut().gossipsub.report_message_validation_result(&message_id, &propagation_source, accepted);
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    debug!(peer = ?peer_id, error = %error, "dial failed");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    debug!(peer = %peer_id, "connected");
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established: 0, cause, .. } => {
                    debug!(peer = %peer_id, ?cause, "disconnected");
                }
                _ => {}
            }
        }
    }
}

/// Gossipsub for the blocks. Identify and ping because OP Stack nodes drop a peer that does not
/// answer identify.
#[derive(NetworkBehaviour)]
struct Behaviour {
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
}

fn swarm(keypair: identity::secp256k1::Keypair) -> anyhow::Result<Swarm<Behaviour>> {
    let config = gossipsub::ConfigBuilder::default()
        .validation_mode(ValidationMode::Anonymous)
        .validate_messages()
        .max_transmit_size(MAX_MESSAGE)
        .message_id_fn(|m: &gossipsub::Message| message_id(&m.data))
        .build()
        .map_err(|e| anyhow::anyhow!("gossipsub config: {e}"))?;
    let gossipsub = gossipsub::Behaviour::new(MessageAuthenticity::Anonymous, config)
        .map_err(|e| anyhow::anyhow!("gossipsub: {e}"))?;
    let keypair = identity::Keypair::from(keypair);
    let behaviour = Behaviour {
        gossipsub,
        identify: identify::Behaviour::new(identify::Config::new(
            "ipfs/0.1.0".into(),
            keypair.public(),
        )),
        ping: ping::Behaviour::default(),
    };
    Ok(libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build())
}

fn topic(version: u8) -> IdentTopic {
    IdentTopic::new(format!("/optimism/{CHAIN_ID}/{version}/blocks"))
}

/// As op-node computes it: sha256 over a domain and the decompressed data, first 20 bytes.
fn message_id(data: &[u8]) -> gossipsub::MessageId {
    let mut h = Sha256::new();
    match snap::raw::Decoder::new().decompress_vec(data) {
        Ok(plain) => {
            h.update([1, 0, 0, 0]);
            h.update(plain);
        }
        Err(_) => {
            h.update([0, 0, 0, 0]);
            h.update(data);
        }
    }
    gossipsub::MessageId::from(h.finalize()[..20].to_vec())
}

/// A gossip message as the enclave's input, if it is a block the sequencer signed.
fn signed_head(data: &[u8]) -> anyhow::Result<SignedHead> {
    let plain = snap::raw::Decoder::new().decompress_vec(data)?;
    anyhow::ensure!(plain.len() > 65, "too short to carry a signature");
    let input = Input {
        signature: Bytes::copy_from_slice(&plain[..65]),
        envelope: Bytes::copy_from_slice(&plain[65..]),
    };
    let block = verify_block(&input)?;
    Ok(SignedHead {
        height: block.number,
        state_root: block.state_root,
        timestamp: block.timestamp,
        input: json!(input),
    })
}

/// A discovered node's dial address, if it says it is on Base.
fn base_peer(enr: &Enr) -> Option<(PeerId, Multiaddr)> {
    (enr.get_raw_rlp("opstack")? == opstack_record().as_slice()).then_some(())?;
    let CombinedPublicKey::Secp256k1(_) = enr.public_key() else {
        return None;
    };
    let key = identity::secp256k1::PublicKey::try_from_bytes(&enr.public_key().encode()).ok()?;
    let peer = PeerId::from_public_key(&identity::PublicKey::from(key));
    let addr = format!("/ip4/{}/tcp/{}/p2p/{peer}", enr.ip4()?, enr.tcp4()?)
        .parse()
        .ok()?;
    Some((peer, addr))
}

/// The `opstack` record a Base node advertises: an RLP string of `uvarint(chain id)` then
/// `uvarint(version 0)`.
fn opstack_record() -> Vec<u8> {
    let mut body = Vec::new();
    let mut n = CHAIN_ID;
    while n >= 0x80 {
        body.push((n as u8 & 0x7f) | 0x80);
        n >>= 7;
    }
    body.push(n as u8);
    body.push(0);
    let mut out = vec![0x80 + body.len() as u8];
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bootnodes advertise Base mainnet (8453), which encodes as `85 42 00`; Sepolia's id
    /// takes three varint bytes.
    #[test]
    fn the_opstack_record_encodes_the_chain_id_as_op_node_does() {
        assert_eq!(opstack_record(), [0x84, 0xb4, 0x94, 0x05, 0x00]);
        let enr: Enr = BOOTNODES[0].parse().unwrap();
        assert_eq!(
            enr.get_raw_rlp("opstack").unwrap(),
            [0x83, 0x85, 0x42, 0x00]
        );
        assert!(base_peer(&enr).is_none(), "a mainnet node is not dialled");
    }
}

/// Listens on Base's p2p network until the first signed block arrives, then prints it in the
/// shape of `tee-node/testdata/base_gossip.json`. It needs the network, so it runs only when
/// asked, on a host with a public IP:
///
/// ```sh
/// BASE_P2P_PORT=9222 RUST_LOG=info,tee_coprocessor=debug \
///   cargo test -p tee-coprocessor --lib base_gossip_live -- --ignored --nocapture
/// ```
#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore]
    async fn base_gossip_live() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
        let port = std::env::var("BASE_P2P_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let head = super::start(port)
            .first(std::time::Duration::from_secs(30 * 60))
            .await
            .expect("no signed Base block within 30 minutes");
        let input: tee_node::chains::l2::base::Input = serde_json::from_value(head.input).unwrap();
        let block = tee_node::chains::l2::base::verify_block(&input).unwrap();
        let fixture = serde_json::json!({
            "message": format!("0x{}{}", hex::encode(&input.signature), hex::encode(&input.envelope)),
            "number": block.number,
            "timestamp": block.timestamp,
            "state_root": block.state_root,
            "hash": block.hash,
        });
        println!("BASE_GOSSIP_FIXTURE {fixture}");
    }
}
