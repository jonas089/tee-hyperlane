//! Turns policy/identity.toml into the identity policy this build is compiled against.
//!
//! Development builds leave it unpinned, which still requires a genuine non-debug TDX quote
//! on an acceptable TCB level but stops asking *which* enclave. Production builds capture
//! real measurements from a live CVM. The distinction is visible on chain, because the two
//! produce different identity digests.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let path =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../policy/identity.toml");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let doc: toml::Value = raw.parse().expect("identity.toml is not valid TOML");
    let require = doc.get("require_enclave").and_then(|v| v.as_bool()).unwrap_or(false);

    let body = if require {
        let field = |name: &str, len: Option<usize>| -> String {
            let s = doc.get(name).and_then(|v| v.as_str()).unwrap_or_else(|| {
                panic!("identity.toml: `{name}` is required when require_enclave = true")
            });
            let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s))
                .unwrap_or_else(|e| panic!("identity.toml: `{name}` is not valid hex: {e}"));
            assert!(!bytes.is_empty(), "identity.toml: `{name}` must not be empty");
            if let Some(len) = len {
                assert_eq!(bytes.len(), len, "identity.toml: `{name}` must be {len} bytes");
            }
            format!("{bytes:?}")
        };
        format!(
            r#"IdentityPolicy::Require(EnclaveIdentity {{
        mr_td: {mr_td}.try_into().unwrap(),
        os_image_hash: {os}.to_vec(),
        compose_hash: {compose}.to_vec(),
        mr_kms: {kms}.to_vec(),
        key_provider: {kp}.to_vec(),
    }})"#,
            mr_td = field("mr_td", Some(48)),
            os = field("os_image_hash", None),
            compose = field("compose_hash", None),
            kms = field("mr_kms", None),
            kp = field("key_provider", None),
        )
    } else {
        println!(
            "cargo:warning=building with require_enclave = false: any genuine TDX enclave \
             will be accepted. Development only - see policy/identity.toml"
        );
        "IdentityPolicy::Any".to_string()
    };

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("pinned_identity.rs");
    fs::write(
        &out,
        format!(
            "static IDENTITY_POLICY: std::sync::OnceLock<IdentityPolicy> =\n    \
             std::sync::OnceLock::new();\n\n\
             /// True when this build demands one specific enclave.\n\
             pub const REQUIRES_SPECIFIC_ENCLAVE: bool = {require};\n\n\
             fn load_identity_policy() -> IdentityPolicy {{\n    {body}\n}}\n"
        ),
    )
    .unwrap();
}
