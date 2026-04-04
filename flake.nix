{
  description = "Bria";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
  };
  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem
    (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };
      rustVersion = pkgs.pkgsBuildHost.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustToolchain = rustVersion.override {
        extensions = ["rust-analyzer" "rust-src"];
      };
      nativeBuildInputs = with pkgs;
        [
          rustToolchain
          protobuf
        ];
      batsLatest = pkgs.bats.overrideAttrs (_old: {
        version = "1.13.0";
        src = pkgs.fetchFromGitHub {
          owner = "bats-core";
          repo = "bats-core";
          rev = "v1.13.0";
          sha256 = "145s0ca5vy3bs50hvkk1qkbi8hdiyvc7jp2rmnyvnjihdsdq2p1n";
        };
      });
      devEnvVars = rec {
        PGDATABASE = "pg";
        PGUSER = "user";
        PGPASSWORD = "password";
        PGHOST = "127.0.0.1";
        DATABASE_URL = "postgres://${PGUSER}:${PGPASSWORD}@${PGHOST}:5432/pg";
        PG_CON = "${DATABASE_URL}";
      };
    in
      with pkgs; {
        devShells.default = mkShell (devEnvVars
          // {
            inherit nativeBuildInputs;
            packages = [
              alejandra
              sqlx-cli
              bacon
              cargo-nextest
              cargo-audit
              cargo-watch
              postgresql
              docker-compose
              batsLatest
              jq
            ];
            shellHook = ''
              # Workaround for nixpkgs xcrun warnings on Darwin
              # See: https://github.com/NixOS/nixpkgs/issues/376958
              unset DEVELOPER_DIR
            '';
          });

        formatter = alejandra;
      });
}
