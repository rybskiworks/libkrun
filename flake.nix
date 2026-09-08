# DRAFT — unvalidated (no nix in authoring env); validate with nix flake check + nix build on a nix host
{
  description = "libkrun — microVM API as a shared library (nix draft)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/a799d3e3886da994fa307f817a6bc705ae538eeb";

    # Shared tooling pin (mirrors workestrate); follows the consumer nixpkgs.
    tooling = {
      url = "github:rybskiworks/nix-tooling/18f8b85f6777240a0ecef4e93ebee69313802aed";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-parts = {
      url = "github:hercules-ci/flake-parts/9d0d87172c374f89da73c1cfe6d81ae62feac1f1";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    # DRAFT: libkrunfw flake is itself a DRAFT; input accepted for checks/devshell
    # (LD_LIBRARY_PATH). Integration/VM-booting checks need it and stay in CI.
    libkrunfw = {
      url = "github:rybskiworks/libkrunfw";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs { inherit system; };

          # Toolchain (DRAFT): workestrate pins rust via fenix (fa09e647..., rustc 1.97.1,
          # owned pin shared with nix-tooling); tooling exposes only tombi + devenvModules
          # (no rust toolchain at inputs.tooling.packages.${system}), so this draft falls
          # back to pkgs cargo/rustc/rustfmt. Pin via fenix on a nix host if reproducibility needs it.
          rustNative = with pkgs; [
            cargo
            rustc
            rustfmt
          ];

          # FULL_VERSION / ABI_VERSION mirror the Makefile (1.17.3 / 1).
          fullVersion = "1.17.3";
          abiVersion = "1";

          # DRAFT: libkrunfw flake is a DRAFT; direct reference is accepted for
          # checks/devshell (LD_LIBRARY_PATH). Integration/VM-booting checks need it.
          libkrunfwLib = inputs.libkrunfw.packages.${system}.default;

          libkrun = pkgs.stdenv.mkDerivation {
            pname = "libkrun";
            version = fullVersion;
            src = ./.;

            nativeBuildInputs =
              rustNative
              ++ (with pkgs; [
                gcc
                gnumake
                pkg-config
                patchelf
              ]);

            buildInputs = with pkgs; [
              libcap_ng
              llvmPackages.libclang
              glibc.static
            ];

            buildPhase = ''
              runHook preBuild
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              # Default features only (minimal). SEV/TDX/EFI/GPU/SND/INPUT/BLK/NET/
              # TIMESYNC/AWS_NITRO flags rename artifacts (VARIANT) and are out of
              # scope for the draft package.
              make -j$NIX_BUILD_CORES
              runHook postBuild
            '';

            # DRAFT — unvalidated: cdylib via the repo's make; soname symlinks mirror `make install`.
            installPhase = ''
              runHook preInstall
              mkdir -p $out/lib
              install -m 755 target/release/libkrun.so.${fullVersion} $out/lib/
              ln -s libkrun.so.${fullVersion} $out/lib/libkrun.so.${abiVersion}
              ln -s libkrun.so.${abiVersion} $out/lib/libkrun.so
              runHook postInstall
            '';
          };
        in
        {
          packages.libkrun = libkrun;
          packages.default = libkrun;

          devShells.default = pkgs.mkShell {
            packages =
              rustNative
              ++ [
                pkgs.clang
                pkgs.llvmPackages.libclang
                pkgs.libcap_ng
                pkgs.pkg-config
                pkgs.gnumake
                pkgs.gcc
                pkgs.glibc.static
                pkgs.patchelf
              ];
            shellHook = ''
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.glibc.dev}/include"
              # DRAFT: libkrunfw flake is itself a DRAFT; input accepted for checks/devshell
              # (LD_LIBRARY_PATH). Integration/VM-booting checks need it and stay in CI.
              export LD_LIBRARY_PATH="${libkrunfwLib}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              echo "libkrun nix draft shell (unvalidated — see flake.nix header)"
            '';
          };

          checks = {
            fmt = pkgs.runCommand "libkrun-fmt-check"
              {
                src = ./.;
                nativeBuildInputs = rustNative;
              }
              ''
                mkdir -p $out
                cd $src
                cargo fmt --check
                touch $out/ok
              '';

            # Unit scope (DRAFT): only workspace unit tests that don't need KVM
            # (e.g. `cargo test -p msb_krun --lib` builder validation).
            # KVM/VM-booting tests are excluded from nix checks and rely on CI.
            # Clippy stays advisory in CI (ci-advisory owns it); deliberately NOT
            # a nix check (nix checks have no continue-on-error).
            unit-msb-krun = pkgs.runCommand "libkrun-unit-msb-krun"
              {
                src = ./.;
                nativeBuildInputs =
                  rustNative
                  ++ (with pkgs; [
                    gcc
                    pkg-config
                    libcap_ng
                    clang
                    llvmPackages.libclang
                  ]);
              }
              ''
                mkdir -p $out
                cd $src
                export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
                cargo test -p msb_krun --lib
                touch $out/ok
              '';
          };
        };
    };
}
