{
  description = "Nobz — a portable native desktop GUI for Usenet (Rust + egui)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (localSystem:
      let
        pkgs = import nixpkgs {
          inherit localSystem;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Toolchain pinned via rust-overlay; falls back to the latest stable
        # toolchain if the file is missing.
        toolchain =
          if builtins.pathExists ./rust-toolchain.toml then
            pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
          else
            pkgs.rust-bin.stable.latest.default.override {
              extensions = [ "rust-src" "clippy" "rustfmt" ];
            };

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # Common args for `cargo build`/`cargo clippy`/`cargo test`. These
        # native deps cover egui's GL / X11 / Wayland needs on Linux.
        commonArgs = {
          src = craneLib.path ./.;
          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ];

          buildInputs = with pkgs; [
            # GL / windowing for egui's glow backend.
            libGL
            libxkbcommon
            wayland
            xorg.libX11
            xorg.libXcursor
            xorg.libXi
            xorg.libXrandr
            xorg.libXrender
            # Font discovery at runtime.
            fontconfig
            freetype
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            # sqlite3 is bundled by sqlx-sqlite via libsqlite3-sys; this is
            # only needed if we later switch to system sqlite.
          ];

          # CARGO_BUILD_RUSTFLAGS lets us inject target-specific flags without
          # touching .cargo/config.toml. Keep -C force-unwind-tables so that
          # panics in release still produce backtraces for bug reports.
          CARGO_BUILD_RUSTFLAGS =
            if pkgs.stdenv.isLinux then
              "-C link-arg=-Wl,--as-needed -C force-unwind-tables=yes"
            else
              "-C force-unwind-tables=yes";
        };

        # Workspace-deps only derivation, so that source changes in our own
        # crates don't bust the cargo-deps cache.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        nobz-gui = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          # The single binary the app ships.
          cargoExtraArgs = "--bin nobz";
          # Desktop entry + icon at runtime via a wrapProgram so `nix run`
          # behaves like a normal app on Linux.
          postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            wrapProgram $out/bin/nobz \
              --prefix XDG_DATA_DIRS : "${pkgs.fontconfig}/share" \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath commonArgs.buildInputs}"
          '';
          meta = with pkgs.lib; {
            description = "Portable native desktop GUI for Usenet";
            homepage = "https://github.com/nobz/nobz";
            license = with licenses; [ mit asl20 ];
            mainProgram = "nobz";
            platforms = platforms.linux ++ platforms.darwin;
          };
        });
      in
      {
        packages = {
          default = nobz-gui;
          nobz = nobz-gui;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = nobz-gui;
          name = "nobz";
        };

        # `nix develop` shell: same native deps + toolchain + a few extras
        # for day-to-day work (rustfmt/clippy come from the overlay). No
        # `cargoArtifacts` here, so the first `cargo build`/`cargo test` in the
        # shell compiles deps lazily (smaller initial fetch, slower first build).
        devShells.default = craneLib.devShell (commonArgs // {
          packages = with pkgs; [
            cargo-nextest
            cargo-watch
            cargo-edit
            nixpkgs-fmt
            # File dialog fallback for rfd (native file picker).
            zenity
          ];

          # Expose the runtime libraries (GL/X11/Wayland/fontconfig) so
          # `cargo run` inside `nix develop` can open a window on Linux
          # without a wrapper.
          shellHook = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath commonArgs.buildInputs}:$LD_LIBRARY_PATH"
          '';
        });

        # Static checks that don't need to build the whole workspace.
        checks = {
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
          fmt = craneLib.cargoFmt commonArgs;
          test = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });
        };
      });
}