{pkgs ? import <nixpkgs> {}}:
pkgs.rustPlatform.buildRustPackage {
  pname = "glacierctl";
  version = "0.0.1";

  src = builtins.path {
    path = ./glacierctl;
    filter = path: type: !(type == "directory" && builtins.baseNameOf path == "target");
  };

  cargoLock = {
    lockFile = ./glacierctl/Cargo.lock;
  };

  nativeBuildInputs = [pkgs.mold];

  env.RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

  # avoid running cargo tests during build
  doCheck = false;

  meta = with pkgs.lib; {
    description = "Glacier D-Bus daemon controller";
    license = licenses.gpl3;
    platforms = platforms.linux;
  };
}
