{ pkgs, lib, ... }:
pkgs.rustPlatform.buildRustPackage {
  pname = "glacierctl";
  version = "0.0.1";

  src = builtins.path {
    path = ./glacierctl;
    filter = path: type: !(type == "directory" && builtins.baseNameOf path == "target");
  };

  cargoLock.lockFile = ./glacierctl/Cargo.lock;

  nativeBuildInputs = [ pkgs.mold ];
  env.RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
  doCheck = false;

  meta = {
    description = "Glacier D-Bus controller";
    license = lib.licenses.gpl3;
    platforms = lib.platforms.linux;
  };
}
