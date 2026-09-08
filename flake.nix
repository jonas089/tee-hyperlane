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
        # enclave identity, new vkeys, and six new ISMs with every warp router repointed.
        # That is a large bill for changing a comment in code the enclave never runs.
        keep = [
          "tee-hyperlane/Cargo.toml"
          "tee-hyperlane/Cargo.lock"
          "tee-hyperlane/rust-toolchain"
          "tee-hyperlane/crates/hyperlane-types"
          "tee-hyperlane/crates/tee-node"
          # tee-attestation arrives as a path dependency, and its own manifest inherits from
          # the tee-circuit workspace root, so that one file comes along. The circuit guests
          # and circuit-tool do not: they are built separately and never linked here.
          "tee-circuit/Cargo.toml"
          "tee-circuit/tee-attestation"
        ];

        # Tests are not built (`doCheck = false`), so letting them into the source would put
        # the enclave's identity at the mercy of a fixture.
        #
        # `enclave-identity.toml` is excluded for a sharper reason, and `postPatch` below
        # writes the placeholder that replaces it: the file names the compose hash, the
        # compose hash covers this image's digest, and the image's digest would then depend
        # on the file. Excluding it here is what actually breaks that cycle - overwriting it
        # during the build does not, because the source is hashed before the build runs.
        excluded = rel:
          let seg = name: pkgs.lib.hasInfix "/${name}/" rel || pkgs.lib.hasSuffix "/${name}" rel;
          in seg "target" || seg "tests" || seg "testdata" || seg "node_modules"
            || pkgs.lib.hasSuffix "/enclave-identity.toml" rel;

        src = pkgs.lib.cleanSourceWith {
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
            in pkgs.lib.any onKeptPath keep && !(excluded rel);
        };

        teeNode = rustPlatform.buildRustPackage {
          pname = "tee-node";
          version = "0.1.0";
          inherit src;
          sourceRoot = "source/tee-hyperlane";

          cargoLock = {
            lockFile = ./tee-hyperlane/Cargo.lock;
            # helios publishes only nightly tags, so it arrives by git rev rather than from
            # crates.io. Pinned by content hash here, by rev in Cargo.lock.
            outputHashes = {
              "helios-consensus-core-0.11.1" =
                "sha256-iV+FnmteHnSZFZ8wJi0PUwDeU9gnLh8gPN+0X//2mSQ=";
            };
          };

          # Two edits the source filter cannot make on its own.
          #
          # The members: cargo insists every workspace member resolve even when building one
          # of them, and the excluded crates' manifests are not in `src`. Trimming the list
          # is what lets them stay out.
          #
          # The identity: `enclave-identity.toml` pins the measurements the *circuits*
          # demand, and one of them is the compose hash, which covers this image's digest.
          # Leaving the file in the source would make the image digest depend on a value
          # derived from the image digest - a cycle, and one that quietly falsifies the
          # reproducibility claim in docs/verify-deployment.md, because the file is written
          # after the image it describes was built. tee-node reads none of these constants
          # (the policy is enforced in the SP1 guests, not in the enclave), so the build gets
          # a placeholder instead. It is all-zero rather than `require_enclave = false` so
          # that anyone who later wires policy enforcement into tee-node gets a build that
          # accepts nothing, rather than one that accepts anything.
          postPatch = ''
            sed -i 's|^members = .*|members = ["crates/hyperlane-types", "crates/tee-node"]|' Cargo.toml
            grep -q 'members = \["crates/hyperlane-types", "crates/tee-node"\]' Cargo.toml
            cat > ../tee-circuit/tee-attestation/enclave-identity.toml <<'IDENTITY'
            require_enclave = true
            mr_td         = "${builtins.concatStringsSep "" (builtins.genList (_: "00") 48)}"
            os_image_hash = "${builtins.concatStringsSep "" (builtins.genList (_: "00") 32)}"
            compose_hash  = "${builtins.concatStringsSep "" (builtins.genList (_: "00") 32)}"
            mr_kms        = "${builtins.concatStringsSep "" (builtins.genList (_: "00") 32)}"
            key_provider  = "00"
            IDENTITY
          '';

          cargoBuildFlags = [ "-p" "tee-node" "--bin" "tee-node" ];
          doCheck = false;

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
        };
      in
      {
        packages = {
          inherit teeNode;
          default = teeNode;

          # Timestamps fixed at epoch 0 and layers content-addressed, so two builds of the
          # same source produce the same digest.
          image = pkgs.dockerTools.buildLayeredImage {
            name = "ghcr.io/jonas089/tee-node";
            tag = "reproducible";
            created = "1970-01-01T00:00:00Z";
            contents = [ pkgs.cacert ];
            config = {
              Entrypoint = [ "${teeNode}/bin/tee-node" ];
              ExposedPorts."8080/tcp" = { };
            };
          };
        };
      });
}
