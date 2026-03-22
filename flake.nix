{
  description = "Glacier Daemon";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs, ... }:
  let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
    glacierd = pkgs.callPackage ../pkgs/glacierd/default.nix { };
    glacierctl = pkgs.callPackage ../pkgs/glacierctl/default.nix { };   
  in {
    nixosModules.glacierd = {
      imports = [ ./modules/glacierd.nix ];
      _module.args = {
        glacierd-pkg = self.packages.${system}.glacierd;
        glacierctl-pkg = self.packages.${system}.glacierctl;
      };
    };
    nixosModules.default = self.nixosModules.glacierd;

    packages.${system} = {
      glacierd = glacierd;
      glacierctl = glacierctl;
    };
  };
}
