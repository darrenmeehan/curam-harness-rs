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
      # Linux is the machine this is developed on and the one CI builds;
      # aarch64-darwin is the other one with a nixpkgs binary cache behind it.
      #
      # Not listed: x86_64-darwin. The nixpkgs input above (nixpkgs-unstable,
      # i.e. 26.11) dropped Intel Macs — `import nixpkgs { system =
      # "x86_64-darwin"; }` throws, and rust-overlay's toolchain no longer
      # declares it either. Forcing it takes two `allowUnsupportedSystem`
      # escapes and still leaves a system with no cache and no patches, which
      # is worse than an honest absence. Nothing in the Darwin branch below is
      # arm-specific, so an Intel Mac becomes a one-line addition here the day
      # `inputs.nixpkgs` moves to a branch that supports it
      # (nixpkgs-26.05-darwin is the last one that does).
      systems = [ "x86_64-linux" "aarch64-darwin" ];

      # One flat output set merged from every system, so `devShells.<system>`
      # and friends land where Nix looks for them.
      forAllSystems = f:
        nixpkgs.lib.foldl' nixpkgs.lib.recursiveUpdate { }
          (map (system: f system (import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          })) systems);

      # The version is in rust-toolchain.toml, never here: change that channel
      # and nothing else to move this repo to a different Rust. Each repo gets
      # its own copy of this file, so different projects can sit on different
      # versions at the same time.
      #
      # The pinned rust-overlay rev ships stable 1.29.0+ and nightly back to
      # 2025-01-01 as vendored manifests — picking any of those is offline and
      # needs no coordination. A version outside that range needs
      # `nix flake update rust-overlay` in this repo only (own flake.lock).
      perSystem = system: pkgs:
        let
          inherit (pkgs) lib;
          darwin = pkgs.stdenv.hostPlatform.isDarwin;

          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # What the window front end links against. Shared by the devShell and
          # the package so the two cannot drift apart.
          guiBuildInputs = if darwin then [
            # macOS gets text from CoreText: zed-font-kit's freetype-sys and
            # yeslogic-fontconfig-sys deps are behind
            # `cfg(not(any(windows, macos, ios)))`, so neither is compiled
            # here. The window server, Metal and the C system libraries come
            # from the frameworks in nixpkgs' Apple SDK — there is no
            # X11/Wayland/Vulkan on a Mac, and libxkbcommon/libxcb/wayland in
            # particular do not build there at all.
            #
            # openssl is not linked today either: ureq pulls native-tls, which
            # goes through Security.framework on Apple and only builds
            # openssl-sys on Linux. It stays because it is in the binary cache
            # and a future dependency with a bare openssl-sys link would
            # otherwise fail only on somebody else's Mac.
            pkgs.openssl
          ] else [
            # Linux: fontconfig/freetype for text, xkbcommon + xcb + X11 for
            # input/windows, wayland for the Wayland backend, vulkan-loader for
            # the renderer. Missing any of them fails at link time, not
            # configure time.
            pkgs.openssl
            pkgs.fontconfig
            pkgs.freetype
            pkgs.libxkbcommon
            pkgs.libxcb
            pkgs.vulkan-loader
            pkgs.wayland
            pkgs.libX11
            pkgs.libXcursor
            pkgs.libXrandr
            pkgs.libXi
          ];

          # ghostty-vt-sys has no prebuilt Linux library, so Linux compiles
          # libghostty-vt from source and needs Zig. The Apple targets link the
          # release's XCFramework instead, which needs no Zig and no
          # GHOSTTY_SOURCE_DIR / GHOSTTY_ZIG_SYSTEM_DIR.
          ghosttyNativeInputs = [ pkgs.pkg-config ] ++ lib.optional (!darwin) pkgs.zig;

          # ghostty-vt-sys normally downloads libghostty-vt and Zig-builds it
          # during `cargo build`. The Nix sandbox has no network, so the two
          # tarballs are fetched here and handed over by env var instead. The
          # hashes are the ones in the crate's own GHOSTTY.lock — bump them
          # together.
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

          # The Linux source build: the two tarballs from GHOSTTY.lock.
          ghosttyLinux = {
            GHOSTTY_SOURCE_DIR = ghosttyTarball {
              name = "src";
              url = "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz";
              hash = "sha256-Mz386XZB75tTa7ngiQethquqraUOj3QXtw2ShK3vN6o=";
            };
            GHOSTTY_ZIG_SYSTEM_DIR = ghosttyTarball {
              name = "deps";
              url = "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1-deps.tar.gz";
              hash = "sha256-kaThCWClK/W/SxWqKizdr/ionWzZA9MFW9pP0X3jNW0=";
            };

            # nixpkgs ships a Zig setup hook that replaces configurePhase /
            # buildPhase / checkPhase / installPhase with `zig build` at the
            # derivation root whenever those phases are unset. There is no
            # build.zig here — Cargo drives Zig inside ghostty-vt-sys's build
            # script — so the hook has to stand down. Its guards are these
            # four. Only needed where Zig is in nativeBuildInputs, which is
            # Linux alone.
            dontUseZigConfigure = true;
            dontUseZigBuild = true;
            dontUseZigCheck = true;
            dontUseZigInstall = true;

            # The sandbox has no HOME, so Zig cannot create its default
            # $HOME/.cache/zig and aborts with "unable to open global cache
            # directory /homeless-shelter/.cache/zig". Point it at the build
            # dir.
            preBuild = ''
              export HOME=$TMPDIR
              export ZIG_GLOBAL_CACHE_DIR=$TMPDIR/zig-global
              export ZIG_LOCAL_CACHE_DIR=$TMPDIR/zig-local
            '';
          };

          # The Darwin link: the release's XCFramework, read from disk. The
          # build script fetches it from the release URL by default and that
          # cannot work in the sandbox, so GHOSTTY_RELEASE_DIR hands it over —
          # it looks the file up by the name in GHOSTTY.lock. Both Apple
          # targets link the same `macos-arm64_x86_64` slice of the universal
          # archive, so one zip serves aarch64 and x86_64 alike.
          ghosttyDarwin = {
            GHOSTTY_RELEASE_DIR = pkgs.runCommand "libghostty-vt-release" { } ''
              mkdir -p $out
              cp ${pkgs.fetchurl {
                url = "https://release.files.ghostty.org/libghostty-vt/1.0.1/ghostty-vt-1.0.1.xcframework.zip";
                hash = "sha256-wPljEnf3JhVMj1IZVNF83EGqxGJvwN+M+SWhnuWOgps=";
              }} $out/ghostty-vt-1.0.1.xcframework.zip
            '';
          };

          curamPkg = pkgs.rustPlatform.buildRustPackage ({
            pname = "curam";
            # Read, not hardcoded: the crate version has one home.
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = ghosttyNativeInputs;
            # rusqlite's `bundled` feature does not engage under buildRustPackage:
            # libsqlite3-sys emits a bare `-lsqlite3` with no search path, and the
            # link dies with "cannot find -lsqlite3". Hand it Nix's sqlite instead
            # of fighting the vendored copy — nixpkgs' build is the patched one
            # anyway. (nix-config pins a whole nixpkgs rev for curam-membrane to
            # avoid this; a buildInput is the cheaper fix here.)
            #
            # Kept on Darwin as well: the same build script runs there, sqlite
            # builds and is cached for both Apple targets, and an unused input
            # costs nothing next to a link failure only a Mac user would hit.
            buildInputs = guiBuildInputs ++ [ pkgs.sqlite ];

            # The unit tests resolve the bwrap cage binary `curam-run`, which lives
            # in the sibling curam-security repo. Its own dev shell runs them.
            doCheck = false;

            meta = {
              description = "zero-trust coding agent harness";
              license = pkgs.lib.licenses.mit;
              mainProgram = "curam";
              platforms = systems;
            };
          } // (if darwin then ghosttyDarwin else ghosttyLinux));

          # The cage: every shell/run tool spawns `curam-run`, a binary from the
          # sibling curam-security repo. src/cage.rs resolves it as CURAM_RUN_BIN,
          # then the [cage] binary config value, then `curam-run` on PATH — and
          # a package that ships only `curam` leaves that last one empty, so a
          # fresh `nix profile install` fails `curam doctor` with "cage
          # curam-run not found on PATH". Ship both binaries from one package
          # instead of resurrecting a $HOME dev-checkout fallback.
          #
          # Pinned by rev, not by branch: the build must be reproducible, and
          # that repo's own lockfile pins its dependency graph.
          curamSecurity = pkgs.fetchFromGitHub {
            owner = "darrenmeehan";
            repo = "curam-security";
            rev = "c95181fe8e4b4700ae4f0c869f697b03ce60d47c";
            hash = "sha256-IMd1ExdgosAaqqtoy1cPk1XmkzP5XbJ/Werbd8UmOzQ=";
          };

          # One crate out of that workspace; cargoLock covers the whole graph.
          curamRun = pkgs.rustPlatform.buildRustPackage {
            pname = "curam-run";
            version = "0.1.0";
            src = curamSecurity;
            cargoLock.lockFile = "${curamSecurity}/Cargo.lock";
            cargoBuildFlags = [ "-p" "curam-run" ];
            # That crate's tests want a real bwrap and a writable sandbox; they
            # run in its own repo's dev shell, not in this derivation.
            doCheck = false;
            meta = {
              description = "curam cage binary — sandboxed exec over bwrap";
              license = pkgs.lib.licenses.mit;
              mainProgram = "curam-run";
              platforms = [ "x86_64-linux" "aarch64-linux" ];
            };
          };
        in
        {
          devShells.${system}.default = pkgs.mkShell {
            # `rustc`, `cargo`, `rust-analyzer`, `cargo clippy` all come from
            # here — nothing in the global profile provides them.
            nativeBuildInputs = [ toolchain ] ++ ghosttyNativeInputs;
            buildInputs = guiBuildInputs;

            env = {
              RUST_BACKTRACE = "1";
              # rust-analyzer runs `cargo check`/`clippy` in the background and
              # uses this for "go to definition" into the standard library
              # (rust-src).
              RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
            };
          };

          # `nix build` / `nix profile install .#` / `nix run`. Without this a
          # consumer can only enter a dev shell, never install the binary.
          #
          # Linux gets both binaries: curam-run (the cage) is a separate binary
          # and src/cage.rs looks it up on PATH, so a curam-only package fails
          # `curam doctor`. Darwin stays curam-only — curam-run drives
          # bubblewrap, which does not exist on macOS, so shipping it there
          # would install a binary that can never work.
          packages.${system}.default =
            if darwin then curamPkg else
            pkgs.symlinkJoin {
              name = "curam-${curamPkg.version}";
              paths = [ curamPkg curamRun ];
              # Keep mainProgram so `nix run` picks curam, not curam-run.
              meta = curamPkg.meta // { mainProgram = "curam"; };
            };

          # `nix run` on its own. Without this it guesses from packages, which
          # works today but breaks the moment a second package is added.
          apps.${system}.default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/curam";
            meta.description = "zero-trust coding agent harness";
          };

          formatter.${system} = pkgs.nixpkgs-fmt;
        };
    in
    forAllSystems perSystem;
}
