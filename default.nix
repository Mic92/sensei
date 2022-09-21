 with import <nixpkgs> {};
 mkShell {
   nativeBuildInputs = [
     bashInteractive
     cargo
     rustc
     rustfmt
     protobuf
     nodejs
     bitcoind
     hivemind
   ];
   # this package bundles static protoc binaries
   # This line requires: programs.nix-ld.enable = true; in your nixos configuration
   NIX_LD = lib.fileContents "${stdenv.cc}/nix-support/dynamic-linker";
 }
