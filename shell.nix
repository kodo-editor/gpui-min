{
  pkgs ? import <nixpkgs> {},
  include-rust ? false,
}: let
  stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.llvmPackages_18.stdenv;
  toolchain =
    if pkgs ? fenix
    then pkgs.fenix.complete #.stable
    else throw "todo: vanilla nixpkgs";
in
  if pkgs.stdenv.isDarwin
  then
    # See https://github.com/NixOS/nixpkgs/issues/320084
    throw "zed: nix dev-shell isn't supported on darwin yet."
  else
    (pkgs.mkShell.override {inherit stdenv;}) rec {
      nativeBuildInputs = with pkgs; [
        copyDesktopItems
        curl
        perl
        pkg-config
        protobuf
        rustPlatform.bindgenHook
        (toolchain.withComponents ["rustc" "cargo" "rust-src" "rustfmt" "clippy"])
        rust-analyzer-nightly
      ];

      buildInputs = with pkgs; [
        curl
        fontconfig
        libgit2
        openssl
        sqlite
        zlib
        zstd

        alsa-lib
        libxkbcommon
        wayland
        xorg.libxcb
      ];

      env = {
        LD_LIBRARY_PATH = with pkgs;
          lib.makeLibraryPath (
            buildInputs
            ++ [
              stdenv.cc.cc.lib
              vulkan-loader
            ]
          );
        ZSTD_SYS_USE_PKG_CONFIG = true;
        FONTCONFIG_FILE = pkgs.makeFontsConf {
          fontDirectories = [
            "assets/fonts/zed-mono"
            "assets/fonts/zed-sans"
          ];
        };
      };
    }
