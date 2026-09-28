{
  description = "The measured enclave image, built reproducibly";

  # Every input is pinned by hash in flake.lock, including the compiler and glibc. That is the
  # point: a plain `docker build` bakes in whatever the builder's machine had that day, so its
  # digest can only be taken on trust. This one anyone can reproduce and compare against the
  # digest the circuits pin.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        # The enclave always runs on linux/amd64, whatever machine builds it.
        pkgs = import nixpkgs {
          system = "x86_64-linux";
          overlays = [ (import rust-overlay) ];
        };

        toolchain = pkgs.rust-bin.stable."1.98.0".default;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };

        # Exactly what `cargo build -p tee-node` reads, and nothing else.
        #
        # This filter used to take both workspaces whole, which quietly made the enclave's
        # identity a function of every file in them. The derivation is input-addressed, so a
        # one-character edit to the coprocessor, the gas oracle or a test moved the image
        # digest - and a moved image digest means a new compose hash, a new RTMR3, a new
        # enclave identity, and a new ISM for every route that family attests, with every
        # warp router repointed.
        # That is a large bill for changing a comment in code the enclave never runs.
        keep = [
          "tee-hyperlane/Cargo.toml"
          "tee-hyperlane/Cargo.lock"
          "tee-hyperlane/rust-toolchain"
          "tee-hyperlane/crates/hyperlane-types"
          "tee-hyperlane/crates/tee-node"
        ];

        # Tests are not built (`doCheck = false`), so letting them into the source would put
        # the enclave's identity at the mercy of a fixture.
        excluded = rel:
          let seg = name: pkgs.lib.hasInfix "/${name}/" rel || pkgs.lib.hasSuffix "/${name}" rel;
          in seg "target" || seg "tests" || seg "testdata" || seg "node_modules";

        # Each image is built from a source tree without the other families' chain files in it.
        #
        # Feature gates alone are not enough for this. buildRustPackage is input-addressed and
        # rustc writes the source path into the binary, so any edit anywhere in the shared tree
        # gives every family a new store path and therefore a new image digest, even when the
        # compiled code is identical. Measured, not assumed: changing one error string in
        # Eden's executor moved all three digests before this filter existed.
        #
        # So a change to `ethereum/base.rs` moves only the ethereum image. The shared files -
        # attest.rs, origin.rs, evm.rs, main.rs - genuinely are shared and move all three, as
        # does Cargo.lock. Evolve keeps `celestia/`, because Eden is verified through Celestia.
        familyOnly = {
          celestia = [ "ethereum" "celestia/eden.rs" "celestia/eden" ];
          ethereum = [ "celestia" ];
          evolve = [ "ethereum" ];
        };

        srcFor = feature:
          let
            node = "tee-hyperlane/crates/tee-node/src";
            drop = familyOnly.${feature};
            foreign = rel:
              pkgs.lib.any
                (f: rel == "${node}/${f}" || pkgs.lib.hasPrefix "${node}/${f}/" rel)
                drop;
          in
          pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _type:
              let
                rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
                # Either an ancestor of something wanted, so recursion can reach it, or a
                # descendant of it. Compared on whole path segments, so `tee-node-extra` does
                # not slip in behind `tee-node`.
                onKeptPath = k:
                  rel == k
                  || pkgs.lib.hasPrefix "${k}/" rel
                  || pkgs.lib.hasPrefix "${rel}/" k;
              in pkgs.lib.any onKeptPath keep && !(excluded rel) && !(foreign rel);
          };

        teeNodeFor = feature: rustPlatform.buildRustPackage {
          pname = "tee-node-${feature}";
          version = "0.1.0";
          src = srcFor feature;
          sourceRoot = "source/tee-hyperlane";

          cargoLock = {
            lockFile = ./tee-hyperlane/Cargo.lock;
            # helios publishes only nightly tags, so it arrives by git rev rather than from
            # crates.io. Pinned by content hash here, by rev in Cargo.lock.
            outputHashes = {
              # Every crate fetched from git, keyed as importCargoLock wants. Crates
              # from one repository share that repository's hash: helios for the
              # Ethereum light client, and ev-reth with the slice of reth it pins,
              # which is Eden's execution.
              "ev-precompiles-0.1.0" =
                "sha256-93kUAy0tiopa16BK6fTshgpXnhIRSg2JsU+Zq9qyoxA=";
              "ev-primitives-0.1.0" =
                "sha256-93kUAy0tiopa16BK6fTshgpXnhIRSg2JsU+Zq9qyoxA=";
              "ev-revm-0.1.0" =
                "sha256-93kUAy0tiopa16BK6fTshgpXnhIRSg2JsU+Zq9qyoxA=";
              "helios-consensus-core-0.11.1" =
                "sha256-iV+FnmteHnSZFZ8wJi0PUwDeU9gnLh8gPN+0X//2mSQ=";
              "reth-chainspec-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-consensus-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-consensus-common-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-db-api-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-db-models-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-ethereum-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-ethereum-consensus-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-ethereum-forks-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-ethereum-primitives-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-evm-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-evm-ethereum-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-execution-errors-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-execution-types-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-network-peers-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-prune-types-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-revm-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-stages-types-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-static-file-types-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-storage-api-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-storage-errors-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-tokio-util-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
              "reth-trie-common-2.5.0" =
                "sha256-WBZA9nJ9067DcmxXK5zS0TPfY9T22NXhuRGBLEGAyXQ=";
            };
          };

          # cargo insists every workspace member resolve even when building one of them, and
          # the excluded crates' manifests are not in `src`. Trimming the list is what lets
          # them stay out.
          postPatch = ''
            sed -i 's|^members = .*|members = ["crates/hyperlane-types", "crates/tee-node"]|' Cargo.toml
            grep -q 'members = \["crates/hyperlane-types", "crates/tee-node"\]' Cargo.toml
          '';

          cargoBuildFlags = [
            "-p" "tee-node" "--bin" "tee-node"
            "--no-default-features" "--features" feature
          ];
          doCheck = false;

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
        };
        # Timestamps fixed at epoch 0 and layers content-addressed, so two builds of the same
        # source produce the same digest.
        imageFor = feature: pkgs.dockerTools.buildLayeredImage {
          name = "ghcr.io/jonas089/tee-node";
          tag = "reproducible-${feature}";
          created = "1970-01-01T00:00:00Z";
          contents = [ pkgs.cacert ];
          config = {
            Entrypoint = [ "${teeNodeFor feature}/bin/tee-node" ];
            ExposedPorts."8080/tcp" = { };
          };
        };
      in
      {
        packages =
          # One image per entry. Adding one is this list, `familyOnly` above, a cargo feature and
          # a compose file. Adding a chain to an existing image touches none of those.
          let
            families = [ "celestia" "ethereum" "evolve" ];
            outputs = pkgs.lib.listToAttrs (pkgs.lib.concatMap (f: [
              { name = "tee-node-${f}"; value = teeNodeFor f; }
              { name = "image-${f}"; value = imageFor f; }
            ]) families);
          in
          outputs // { default = teeNodeFor (builtins.head families); };
      });
}
