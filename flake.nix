{
  description = "A very basic flake";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
  let
    system = "x86_64-linux";
    pkgs = import nixpkgs {
      inherit system;
    };
  in
  {
    devShells.${system}.default = let
    in pkgs.mkShell rec {
      LIBCLANG_PATH = "${pkgs.libclang.lib}/lib/libclang.so";
      packages = with pkgs; [
        pkg-config
        pipewire.dev
        libinput.dev
      ];
    };
  };
}
