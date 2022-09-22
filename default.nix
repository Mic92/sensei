with import <nixpkgs> {};
let
  version = "1.4.0";
  pname = "polar";
  src = fetchurl {
    url = "https://github.com/jamaljsr/polar/releases/download/v${version}/polar-linux-x86_64-v${version}.AppImage";
    sha256 = "sha256-m6WR/PUaoC0pArvIxwdJZNuwo0oNwEofReXG+CS92YQ=";
  };
  appimageContents = appimageTools.extractType2 { inherit pname src version; };

  polar = appimageTools.wrapType2 {
    inherit pname src version;
    extraInstallCommands = ''
      mv $out/bin/polar-${version} $out/bin/polar
      . ${makeWrapper}/nix-support/setup-hook
      wrapProgram "$out/bin/polar" --prefix PATH : ${docker-compose}/bin
    '';
  };
in
 mkShell {
   nativeBuildInputs = [
     bashInteractive
     cargo
     rustc
     rustfmt
     protobuf
     nodejs
     #bitcoind
     #bitcoin
     hivemind
     polar
   ];
   # this package bundles static protoc binaries
   # This line requires: programs.nix-ld.enable = true; in your nixos configuration
   NIX_LD = lib.fileContents "${stdenv.cc}/nix-support/dynamic-linker";
 }
