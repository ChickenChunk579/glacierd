{pkgs ? import <nixpkgs> {}}:
pkgs.rustPlatform.buildRustPackage {
  pname = "glacierd";
  version = "0.0.1";

  src = builtins.path {
    path = ./glacierd;
    filter = path: type: !(type == "directory" && builtins.baseNameOf path == "target");
  };

  cargoLock = {
    lockFile = ./glacierd/Cargo.lock;
  };

  nativeBuildInputs = [pkgs.mold];

  env.RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

  # avoid running cargo tests during build
  doCheck = false;

  meta = with pkgs.lib; {
    description = "Glacier D-Bus daemon";
    license = licenses.gpl3;
    platforms = platforms.linux;
  };
}
