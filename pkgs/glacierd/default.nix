{ pkgs, lib, ... }:
pkgs.rustPlatform.buildRustPackage {
  pname = "glacierd";
  version = "0.0.1";

  src = builtins.path {
    path = ./glacierd;
    filter = path: type: !(type == "directory" && builtins.baseNameOf path == "target");
  };

  cargoLock.lockFile = ./glacierd/Cargo.lock;

  nativeBuildInputs = [ pkgs.mold ];
  env.RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
  doCheck = false;

  meta = {
    description = "Glacier D-Bus daemon";
    license = lib.licenses.gpl3;
    platforms = lib.platforms.linux;
  };
}
