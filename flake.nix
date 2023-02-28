{
  description = "A very basic flake";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/22.11";

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
        pkgconfig
        pipewire.dev
        xorg.libX11.dev
        xorg.libXi.dev
        xorg.libXtst
      ];
    };
  };
}
