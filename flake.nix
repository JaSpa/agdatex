{
  description = "Utility to extract LaTeX macros from Agda code";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
      naersk,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = (import nixpkgs) {
          inherit system;
        };

        inherit (pkgs) lib;

        rust = pkgs.callPackage naersk { };
      in
      {
        packages = {
          default = rust.buildPackage { src = lib.cleanSource ./.; };
        };
      }
    );
}
