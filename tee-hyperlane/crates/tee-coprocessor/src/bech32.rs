//! Bech32 encoding for Celestia addresses.
//!
//! Hand-written because adding a crate would change `Cargo.lock`, which is part of every
//! enclave image's source.

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const GENERATOR: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];

fn polymod(values: impl Iterator<Item = u8>) -> u32 {
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ff_ffff) << 5) ^ u32::from(v);
        for (i, g) in GENERATOR.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|b| b & 31));
    out
}

/// Regroup bits, `from` bits per input value to `to` bits per output value.
fn convert(data: &[u8], from: u32, to: u32, pad: bool) -> Option<Vec<u8>> {
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    let max = (1u32 << to) - 1;
    for &v in data {
        if u32::from(v) >> from != 0 {
            return None;
        }
        acc = (acc << from) | u32::from(v);
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & max) as u8);
        }
    }
    if pad {
        if bits > 0 {
            out.push(((acc << (to - bits)) & max) as u8);
        }
    } else if bits >= from || ((acc << (to - bits)) & max) != 0 {
        return None;
    }
    Some(out)
}

pub fn encode(hrp: &str, bytes: &[u8]) -> String {
    let data = convert(bytes, 8, 5, true).expect("bytes always regroup");
    let mut values = hrp_expand(hrp);
    values.extend(&data);
    values.extend([0u8; 6]);
    let pm = polymod(values.into_iter()) ^ 1;
    let mut out = format!("{hrp}1");
    for v in data {
        out.push(CHARSET[v as usize] as char);
    }
    for i in 0..6 {
        out.push(CHARSET[((pm >> (5 * (5 - i))) & 31) as usize] as char);
    }
    out
}

/// The human-readable part and the bytes, or `None` for anything that is not valid bech32.
pub fn decode(text: &str) -> Option<(String, Vec<u8>)> {
    let lower = text.to_ascii_lowercase();
    if lower != text && text.to_ascii_uppercase() != text {
        return None;
    }
    let split = lower.rfind('1')?;
    let (hrp, rest) = (&lower[..split], &lower[split + 1..]);
    if hrp.is_empty() || rest.len() < 6 {
        return None;
    }
    let values: Vec<u8> = rest
        .bytes()
        .map(|c| CHARSET.iter().position(|&x| x == c).map(|p| p as u8))
        .collect::<Option<_>>()?;
    let mut all = hrp_expand(hrp);
    all.extend(&values);
    if polymod(all.into_iter()) != 1 {
        return None;
    }
    let bytes = convert(&values[..values.len() - 6], 5, 8, false)?;
    Some((hrp.to_string(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIP-173's own valid vectors.
    #[test]
    fn bip173_vectors_decode() {
        assert!(decode("A12UEL5L").is_some());
        assert!(decode("abcdef1qpzry9x8gf2tvdw0s3jn54khce6mua7lmqqqxw").is_some());
        assert!(
            decode("abcdef1qpzry9x8gf2tvdw0s3jn54khce6mua7lmqqqxx").is_none(),
            "bad checksum"
        );
        assert!(decode("A12uEL5L").is_none(), "mixed case");
    }

    #[test]
    fn an_address_round_trips() {
        let key = hex::decode("751e76e8199196d454941c45d1b3a323f1433bd6").unwrap();
        let address = encode("celestia", &key);
        assert!(address.starts_with("celestia1"));
        assert_eq!(decode(&address), Some(("celestia".into(), key)));
    }
}
