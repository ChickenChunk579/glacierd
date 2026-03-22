{ config, lib, pkgs, glacierd-pkg, glacierctl-pkg, ... }:
{
  options.services.glacierd.enable = lib.mkEnableOption "Enable the Glacier Daemon";
  config = lib.mkIf config.services.glacierd.enable {
    systemd.services.glacierd = {
	    description = "Glacier Configuration Management Daemon";
	    
	    # Ensure the service starts after the network and D-Bus are ready
	    after = [ "network.target" "dbus.socket" ];
	    wantedBy = [ "multi-user.target" ];

	    path = [
	    	pkgs.nixos-rebuild
	    	pkgs.nix
	    	pkgs.git
	    	pkgs.coreutils
	    ];

	    serviceConfig = {
	      # Path to your binary (assuming it's in your system packages or flake)
	      ExecStart = "${glacierd}/bin/glacierd";
	      
	      # Run as root to allow system-level nixos-rebuild
	      User = "root";
	      Group = "root";

	      # Restart automatically if it crashes
	      Restart = "always";
	      RestartSec = "5s";

	      StateDirectory = "glacier";
	      WorkingDirectory = "/var/lib/glacier";
	    };
	  };
	  
	  services.dbus.packages = [
	    (pkgs.writeTextFile {
	      name = "glacier-dbus-policy";
	      destination = "/share/dbus-1/system.d/com.chickenchunk.Glacier.conf";
	      text = ''
	        <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
	         "http://www.freedesktop.org">
	        <busconfig>
	          <!-- Allow root to own the name -->
	          <policy user="root">
	            <allow own="com.chickenchunk.Glacier"/>
	          </policy>

	          <!-- Allow anyone to call methods on the service -->
	          <policy context="default">
	            <allow send_destination="com.chickenchunk.Glacier"/>
	            <allow receive_sender="com.chickenchunk.Glacier"/>
	          </policy>
	        </busconfig>
	      '';
	    })
	  ];

	  environment.systemPackages = [glacierctl];  	
  };
}
