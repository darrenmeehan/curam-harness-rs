{
  description = "Rust project — declarative toolchain via rust-overlay, loaded by direnv";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };

      # The version is in rust-toolchain.toml, never here: change that channel
      # and nothing else to move this repo to a different Rust. Each repo gets
      # its own copy of this file, so different projects can sit on different
      # versions at the same time.
      #
      # The pinned rust-overlay rev ships stable 1.29.0+ and nightly back to
      # 2025-01-01 as vendored manifests — picking any of those is offline and
      # needs no coordination. A version outside that range needs
      # `nix flake update rust-overlay` in this repo only (own flake.lock).
      toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # What the window front end links against. Shared by the devShell and
      # the package so the two cannot drift apart.
      guiBuildInputs = with pkgs; [
        openssl
        fontconfig
        freetype
        libxkbcommon
        libxcb
        vulkan-loader
        wayland
        libX11
        libXcursor
        libXrandr
        libXi
      ];

      # ghostty-vt-sys normally downloads libghostty-vt and Zig-builds it during
      # `cargo build`. The Nix sandbox has no network, so the two tarballs are
      # fetched here and handed over by env var instead. The hashes are the
      # ones in the crate's own GHOSTTY.lock — bump them together.
      ghosttyTarball = { name, url, hash }: pkgs.stdenv.mkDerivation {
        pname = "libghostty-vt-${name}";
        version = "1.0.1";
        src = pkgs.fetchurl { inherit url hash; };
        dontConfigure = true;
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -r . $out/
          runHook postInstall
        '';
      };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        # `rustc`, `cargo`, `rust-analyzer`, `cargo clippy` all come from here —
        # nothing in the global profile provides them.
        nativeBuildInputs = [
          toolchain
          pkgs.pkg-config
          # ghostty-vt-sys builds libghostty-vt from source on Linux and
          # shells out to Zig; the pinned nixpkgs ships 0.17.1, which is the
          # version the vendored build.zig expects. No prebuilt Linux release
          # exists, so this is required, not optional.
          pkgs.zig
        ];
        # The window front end links GPUI + libghostty-vt. These are the
        # headers/libs they probe for: fontconfig/freetype for text,
        # xkbcommon + xcb + X11 for input/windows, wayland for the Wayland
        # backend, vulkan-loader for the renderer. Missing any of them fails
        # at link time, not configure time.
        buildInputs = guiBuildInputs;

        env = {
          RUST_BACKTRACE = "1";
          # rust-analyzer runs `cargo check`/`clippy` in the background and uses
          # this for "go to definition" into the standard library (rust-src).
          RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
        };
      };

      # `nix build` / `nix profile install .#` / `nix run`. Without this a
      # consumer can only enter a dev shell, never install the binary.
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "curam";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [ pkgs.pkg-config pkgs.zig ];
        # rusqlite's `bundled` feature does not engage under buildRustPackage:
        # libsqlite3-sys emits a bare `-lsqlite3` with no search path, and the
        # link dies with "cannot find -lsqlite3". Hand it Nix's sqlite instead
        # of fighting the vendored copy — nixpkgs' build is the patched one
        # anyway. (nix-config pins a whole nixpkgs rev for curam-membrane to
        # avoid this; a buildInput is the cheaper fix here.)
        buildInputs = guiBuildInputs ++ [ pkgs.sqlite ];

        GHOSTTY_SOURCE_DIR = ghosttyTarball { name = "src";
          url = "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz";
          hash = "sha256-Mz386XZB75tTa7ngiQethquqraUOj3QXtw2ShK3vN6o=";
        };
        GHOSTTY_ZIG_SYSTEM_DIR = ghosttyTarball { name = "deps";
          url = "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1-deps.tar.gz";
          hash = "sha256-kaThCWClK/W/SxWqKizdr/ionWzZA9MFW9pP0X3jNW0=";
        };

        # nixpkgs ships a Zig setup hook that replaces configurePhase /
        # buildPhase / checkPhase / installPhase with `zig build` at the
        # derivation root whenever those phases are unset. There is no
        # build.zig here — Cargo drives Zig inside ghostty-vt-sys's build
        # script — so the hook has to stand down. Its guards are these four.
        dontUseZigConfigure = true;
        dontUseZigBuild = true;
        dontUseZigCheck = true;
        dontUseZigInstall = true;

        # The sandbox has no HOME, so Zig cannot create its default
        # $HOME/.cache/zig and aborts with "unable to open global cache
        # directory /homeless-shelter/.cache/zig". Point it at the build dir.
        preBuild = ''
          export HOME=$TMPDIR
          export ZIG_GLOBAL_CACHE_DIR=$TMPDIR/zig-global
          export ZIG_LOCAL_CACHE_DIR=$TMPDIR/zig-local
        '';

        # The unit tests resolve the bwrap cage binary `curam-run`, which lives
        # in the sibling curam-security repo. Its own dev shell runs them.
        doCheck = false;

        meta = {
          description = "zero-trust coding agent harness";
          license = pkgs.lib.licenses.mit;
          mainProgram = "curam";
          platforms = [ system ];
        };
      };

      formatter.${system} = pkgs.nixpkgs-fmt;
    };
}
