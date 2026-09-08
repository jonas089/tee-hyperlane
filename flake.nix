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

        # tee-node depends on tee-attestation by path, so both trees are inputs even though
        # only one workspace is built.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: _type:
            let
              rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              wanted = pkgs.lib.hasPrefix "tee-hyperlane" rel
                || pkgs.lib.hasPrefix "tee-circuit" rel;
              junk = pkgs.lib.hasInfix "/target/" rel
                || pkgs.lib.hasInfix "/node_modules/" rel;
            in wanted && !junk;
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
