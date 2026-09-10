{
  description = "libkrun — microVM API as a shared library";

  inputs = {
    tooling.url = "github:rybskiworks/nix-tooling/eae927a0da5fd04d2dfd2e7876042c6243adba65";
    nixpkgs.follows = "tooling/nixpkgs";
    flake-parts.follows = "tooling/flake-parts";

    devenv.follows = "tooling/devenv";
    treefmt-nix.follows = "tooling/treefmt-nix";
    git-hooks.follows = "tooling/git-hooks";
    devenv-root = {
      url = "file+file:///dev/null";
      flake = false;
    };

    # Firmware is supplied to the C API through the development shell's loader path.
    libkrunfw = {
      url = "github:rybskiworks/libkrunfw/dde01516aa27d46903d769c10e66c1e51e1fe117";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.tooling.follows = "tooling";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.devenv.flakeModule ];
      systems = [ "x86_64-linux" ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.tooling.inputs.fenix.overlays.default ];
          };

          # Use the shared Rust pin with the stdenv cc-wrapper.
          # Do not add bare gcc here: it shadows the stdenv cc-wrapper and
          # breaks build-script linking.
          rustNative = with pkgs.fenix.stable; [
            cargo
            rustc
            rustfmt
          ];

          rustPlatform = pkgs.makeRustPlatform {
            inherit (pkgs.fenix.stable) cargo rustc;
          };
          cargoDeps = rustPlatform.importCargoLock {
            lockFile = ./Cargo.lock;
            outputHashes."msb-vm-memory-0.18.0-msb.1" = "sha256-aZc0jr3XqrZHyLnQz/NwjUCfsxy7YNAsqjVWrqHYH30=";
          };
          initLdflags = "-L${pkgs.glibc.static}/lib";
          # Bindgen loads the pinned libclang from LIBCLANG_PATH at build time.
          cargoFlags = "--locked --offline --features msb_krun_input/bindgen_clang_runtime";

          # FULL_VERSION mirrors the Makefile.
          fullVersion = "1.17.3";

          libkrunfwLib = inputs.libkrunfw.packages.${system}.default;

          source = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./Makefile
              ./libkrun.pc.in
              ./include
              ./init
              ./src
              ./examples/rust_vm
            ];
          };

          libkrun = pkgs.stdenv.mkDerivation {
            pname = "libkrun";
            version = fullVersion;
            src = source;
            inherit cargoDeps;

            nativeBuildInputs =
              rustNative
              ++ [
                rustPlatform.cargoSetupHook
                rustPlatform.bindgenHook
              ]
              ++ (with pkgs; [
                gnumake
                pkg-config
                patchelf
              ]);

            buildInputs = [ pkgs.libcap_ng ];
            # Keep the Nix linker wrapper in the link path so it records library rpaths.
            env.RUSTFLAGS = "-C linker-features=-lld";

            buildPhase = ''
              runHook preBuild
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              export CARGO_TARGET_DIR="$TMPDIR/target"
              export CARGO_HOME="$TMPDIR/cargo-home"
              mkdir -p "$CARGO_HOME"
              # Default features only (minimal). SEV/TDX/EFI/GPU/SND/INPUT/BLK/NET/
              # TIMESYNC/AWS_NITRO flags rename artifacts (VARIANT) and are out of
              # scope for this package.
              make -j$NIX_BUILD_CORES CARGO_FLAGS="${cargoFlags}" INIT_LDFLAGS="${initLdflags}" PREFIX="$out" LIBDIR_Linux=lib
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              make install PREFIX="$out" LIBDIR_Linux=lib
              runHook postInstall
            '';
          };
        in
        {
          packages.libkrun = libkrun;
          packages.default = libkrun;

          _module.args.pkgs = pkgs;

          devenv.shells.default = {
            containers = pkgs.lib.mkForce { };
            imports = [
              inputs.tooling.devenvModules.base
              inputs.tooling.devenvModules.nix
              inputs.tooling.devenvModules.rust
            ];
            treefmt.config.programs.rustfmt.edition = "2021";
            packages = rustNative ++ [
              pkgs.clang
              pkgs.llvmPackages.libclang
              pkgs.libcap_ng
              pkgs.pkg-config
              pkgs.gnumake
              pkgs.patchelf
            ];
            env.INIT_LDFLAGS = initLdflags;
            # Interactive Cargo uses its ordinary cache and may fetch dependencies.
            # Sandboxed package/check builds keep the locked, offline flags above.
            env.CARGO_FLAGS = "--locked --features msb_krun_input/bindgen_clang_runtime";
            env.RUSTFLAGS = "-C linker-features=-lld";
            enterShell = ''
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.glibc.dev}/include"
              export LD_LIBRARY_PATH="${libkrunfwLib}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';
          };

          checks = {
            c-sdk =
              pkgs.runCommandCC "libkrun-c-sdk-check"
                {
                  nativeBuildInputs = [ pkgs.pkg-config ];
                  buildInputs = [ libkrun ];
                }
                ''
                  $CC ${./tests/c-sdk-smoke.c} $(pkg-config --cflags --libs libkrun) -o c-sdk-smoke
                  ./c-sdk-smoke
                  mkdir -p $out
                  touch $out/ok
                '';

            fmt =
              pkgs.runCommand "libkrun-fmt-check"
                {
                  src = source;
                  nativeBuildInputs = rustNative;
                }
                ''
                  mkdir -p $out
                  cd $src
                  cargo fmt --check
                  touch $out/ok
                '';

            # State codecs, private mappings and device protocol tests do not
            # open KVM. Keep these distinct from boot/restore acceptance tests.
            unit-state = libkrun.overrideAttrs {
              pname = "libkrun-unit-state";
              buildPhase = ''
                runHook preBuild
                export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
                export CARGO_TARGET_DIR="$TMPDIR/target"
                export CARGO_HOME="$TMPDIR/cargo-home"
                export CARGO_PROFILE_TEST_DEBUG=0
                export PROPTEST_CASES=256
                export PROPTEST_RNG_SEED=20260910
                export PROPTEST_MAX_SHRINK_ITERS=4096
                mkdir -p "$CARGO_HOME"
                make init/init INIT_LDFLAGS="${initLdflags}"
                for scope in memory_state private_memory device_state execution_state vmm_config::vsock; do
                  cargo test --locked --offline -p msb_krun_vmm --lib --features blk,net,devices/net "$scope::tests::"
                done
                cargo test --locked --offline -p msb_krun_devices --lib --features blk,net virtio::vmgenid::
                cargo test --locked --offline -p msb_krun_devices --lib --features blk,net virtio::block::backend::tests::
                cargo test --locked --offline -p msb_krun_devices --lib --features blk,net virtio::vsock::
                cargo test --locked --offline -p msb_krun_utils --lib epoll::tests::
                runHook postBuild
              '';
              installPhase = ''
                mkdir -p $out
                touch $out/ok
              '';
            };

            # Unit scope: only workspace unit tests that don't need KVM
            # (e.g. `cargo test -p msb_krun --lib` builder validation).
            # KVM/VM-booting tests are excluded from nix checks and rely on CI.
            # Clippy stays advisory in CI (ci-advisory owns it); deliberately NOT
            # a nix check (nix checks have no continue-on-error).
            unit-msb-krun = libkrun.overrideAttrs {
              pname = "libkrun-unit-msb-krun";
              buildPhase = ''
                runHook preBuild
                export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
                export CARGO_TARGET_DIR="$TMPDIR/target"
                export CARGO_HOME="$TMPDIR/cargo-home"
                mkdir -p "$CARGO_HOME"
                make init/init INIT_LDFLAGS="${initLdflags}"
                cargo test --locked --offline -p msb_krun --lib
                cargo test --locked --offline -p msb_krun --lib --features net
                runHook postBuild
              '';
              installPhase = ''
                mkdir -p $out
                touch $out/ok
              '';
            };
          };
        };
    };
}
