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
        # enclave identity, and a new ISM for every route that image attests, with every
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

        # Each image is built from a source tree without the other chains' files in it.
        #
        # Feature gates alone are not enough for this. buildRustPackage is input-addressed and
        # rustc writes the source path into the binary, so any edit anywhere in the shared tree
        # gives every image a new store path and therefore a new digest, even when the compiled
        # code is identical. Measured, not assumed: changing one error string in Eden's old
        # executor moved every digest before this filter existed.
        #
        # So a change to `chains/l2/base.rs` moves only the base image. The shared files -
        # attest.rs, origin.rs, evm.rs, main.rs - genuinely are shared and move all of them, as
        # does Cargo.lock. Eden keeps Celestia's file, because its headers come through
        # Celestia's light client.
        chainFiles = [
          "l1/celestia.rs" "l1/ethereum.rs"
          "l2/base.rs" "l2/arbitrum.rs" "l2/eden.rs" "l2/sequencer.rs"
        ];
        keeps = {
          celestia = [ "l1/celestia.rs" ];
          ethereum = [ "l1/ethereum.rs" ];
          base = [ "l2/base.rs" "l2/sequencer.rs" ];
          arbitrum = [ "l2/arbitrum.rs" "l2/sequencer.rs" ];
          eden = [ "l1/celestia.rs" "l2/eden.rs" ];
        };

        srcFor = feature:
          let
            chains = "tee-hyperlane/crates/tee-node/src/chains";
            drop = pkgs.lib.subtractLists keeps.${feature} chainFiles;
            foreign = rel: pkgs.lib.any (f: rel == "${chains}/${f}") drop;
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
              # helios is the one crate fetched from git: the Ethereum light client.
              "helios-consensus-core-0.11.1" =
                "sha256-iV+FnmteHnSZFZ8wJi0PUwDeU9gnLh8gPN+0X//2mSQ=";
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
          # One image per origin chain. Adding one is this list, `chainFiles` and `keeps` above,
          # a cargo feature, a compose file and a port in devnet/scripts/lib.sh.
          let
            chains = [ "celestia" "ethereum" "base" "arbitrum" "eden" ];
            outputs = pkgs.lib.listToAttrs (pkgs.lib.concatMap (f: [
              { name = "tee-node-${f}"; value = teeNodeFor f; }
              { name = "image-${f}"; value = imageFor f; }
            ]) chains);
          in
          outputs // { default = teeNodeFor (builtins.head chains); };
      });
}
