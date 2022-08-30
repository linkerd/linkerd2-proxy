{ pkgs ? import <nixpkgs> { } }:
with pkgs;
mkShell {
  buildInputs = [
    cacert
    cmake # for rust-analyzer
    docker
    git
    just
    libiconv
    nodePackages.markdownlint-cli2
    rustup
    shellcheck
  ] ++ lib.optionals stdenv.isDarwin [
    darwin.apple_sdk.frameworks.Security
  ];
}
