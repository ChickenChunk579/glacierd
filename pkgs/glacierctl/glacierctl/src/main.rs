use clap::{Parser, Subcommand};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use std::os::unix::process::CommandExt;
use zbus::{proxy, Connection};

#[proxy(
    default_service = "com.chickenchunk.Glacier",
    default_path = "/com/chickenchunk/Glacier",
    interface = "com.chickenchunk.Glacier"
)]
trait Glacier {
    #[zbus(signal)]
    fn stdout_line(&self, line: String) -> zbus::Result<()>;
    async fn upload_config(&self, src_path: String) -> zbus::Result<String>;
    async fn switch(&self, system_name: String) -> zbus::Result<String>;
    async fn cancel(&self) -> zbus::Result<String>;
}

#[derive(Parser)]
#[command(name = "glacierctl", about = "Glacier Daemon Controller", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Switch { name: String },
    Init,
    Install,
}

struct DrvState {
    pkg: String,
    spinner: ProgressBar,
}

struct LogProcessor {
    mp: MultiProgress,
    main_pb: ProgressBar,
    active: HashMap<String, DrvState>,
    prefix_to_stem: HashMap<String, String>,
    re_total: Regex,
    re_drv_building: Regex,
    re_store_path: Regex,
    re_err_build: Regex,
    re_log_prefix: Regex,
}

impl LogProcessor {
    fn new() -> Self {
        let mp = MultiProgress::new();
        let main_pb = mp.add(ProgressBar::new_spinner());
        main_pb.enable_steady_tick(Duration::from_millis(100));
        main_pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        main_pb.set_message("Waiting for Nix...");

        Self {
            mp,
            main_pb,
            active: HashMap::new(),
            prefix_to_stem: HashMap::new(),
            re_total: Regex::new(r"these (\d+) derivations? will be built").unwrap(),
            re_drv_building: Regex::new(
                r"^building '/nix/store/[a-z0-9]+-(.+?)\.drv'\.\.\.$",
            )
            .unwrap(),
            re_store_path: Regex::new(r"^\s*/nix/store/[a-z0-9]{32}").unwrap(),
            re_err_build: Regex::new(
                r"error: build of '/nix/store/[a-z0-9]+-(.+?)\.drv' failed",
            )
            .unwrap(),
            re_log_prefix: Regex::new(r"^([\w.+\-]+)>(?:\s(.*))?$").unwrap(),
        }
    }

    fn short_prefix(stem: &str) -> String {
        let bytes = stem.as_bytes();
        for i in 1..bytes.len() {
            if bytes[i - 1] == b'-' && bytes[i].is_ascii_digit() {
                return stem[..i - 1].to_string();
            }
        }
        stem.to_string()
    }

    fn finalize(&mut self, stem: &str, success: bool) {
        if let Some(state) = self.active.remove(stem) {
            self.prefix_to_stem.retain(|_, v| v != stem);
            state
                .spinner
                .set_style(ProgressStyle::with_template("  {msg}").unwrap());
            if success {
                state.spinner.finish_with_message(format!(
                    "{} {}",
                    "✔".green(),
                    state.pkg.dimmed()
                ));
            } else {
                state.spinner.finish_with_message(format!(
                    "{} {} {}",
                    "✘".red().bold(),
                    "Failed:".red(),
                    state.pkg.red().bold()
                ));
            }
        }
    }

    fn finalize_all(&mut self, success: bool) {
        let keys: Vec<String> = self.active.keys().cloned().collect();
        for key in keys {
            self.finalize(&key, success);
        }
    }

    fn process(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.re_store_path.is_match(trimmed) {
            return;
        }

        if let Some(caps) = self.re_total.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                if let Ok(total) = m.as_str().parse::<u64>() {
                    self.main_pb.set_length(total);
                    self.main_pb.set_style(
                        ProgressStyle::default_bar()
                            .template(
                                " {spinner:.cyan} 🧊 [{bar:30.blue/cyan}] {pos}/{len} {msg}",
                            )
                            .unwrap()
                            .progress_chars("❄·"),
                    );
                }
            }
            return;
        }

        if let Some(caps) = self.re_drv_building.captures(trimmed) {
            let stem = caps.get(1).map(|m| m.as_str()).unwrap_or("unknown");
            let key = stem.to_string();
            if self.active.contains_key(&key) {
                return;
            }

            self.main_pb.inc(1);
            self.main_pb
                .set_message(format!("Forging {}...", stem.bright_blue()));

            let pb = self
                .mp
                .insert_before(&self.main_pb, ProgressBar::new_spinner());
            pb.enable_steady_tick(Duration::from_millis(120));
            pb.set_style(
                ProgressStyle::with_template("  {spinner:.white} {msg}").unwrap(),
            );
            pb.set_message(format!("{}", stem.cyan()));

            let prefix = Self::short_prefix(stem);
            self.prefix_to_stem.insert(prefix, key.clone());
            self.active.insert(
                key,
                DrvState {
                    pkg: stem.to_string(),
                    spinner: pb,
                },
            );
            return;
        }

        if let Some(caps) = self.re_err_build.captures(trimmed) {
            let stem = caps
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            self.finalize(&stem, false);
            return;
        }

        if let Some(caps) = self.re_log_prefix.captures(trimmed) {
            let prefix = caps
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let msg = caps
                .get(2)
                .map(|m| m.as_str().trim())
                .unwrap_or("")
                .trim();
            if msg.is_empty() {
                return;
            }

            if let Some(stem) = self.prefix_to_stem.get(&prefix).cloned() {
                if let Some(state) = self.active.get(&stem) {
                    state.spinner.set_message(format!(
                        "{}: {}",
                        state.pkg.cyan(),
                        msg.dimmed()
                    ));
                    if looks_like_error(msg) {
                        let _ = self.mp.println(format!(
                            "  {} {} {}",
                            state.pkg.red(),
                            "│".dimmed(),
                            msg.red().dimmed()
                        ));
                    }
                }
            }
            return;
        }

        let _ = self.mp.println(format!("  {}", trimmed.dimmed()));
    }

    fn finish(&mut self) {
        self.finalize_all(true);
        self.main_pb.finish_and_clear();
        println!("✨ {}", "System solidified.".bold().bright_white());
    }

    fn abort(&mut self) {
        self.finalize_all(false);
        self.main_pb.finish_and_clear();
    }
}

fn looks_like_error(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.starts_with("error")
        || lower.starts_with("fatal error")
        || lower.starts_with("ld: ")
        || lower.contains(": error:")
        || lower.contains(": fatal error:")
        || lower.contains("error[e")
        || lower.starts_with("make[")
        || lower.starts_with("cmake error")
        || lower.starts_with("traceback")
        || lower.starts_with("panicked at")
}

const FLAKE_TEMPLATE: &str = r#"{
  description = "My GlacierOS";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    glacier.url = "github:ChickenChunk579/glacier";
  };
  outputs = {
    self,
    nixpkgs,
    glacier,
    ...
  }: let
    baseModules = [
      ./configuration.nix
      glacier.nixosModules.base
      glacier.glacierModules.{wm}
      glacier.glacierModules.{terminal}
      glacier.glacierModules.{dm}
      glacier.glacierModules.{launcher}
      glacier.glacierModules.{browser}
      glacier.glacierModules.{shell}
      glacier.glacierModules.{editor}
    ];
  in {
    nixosConfigurations = {
      {systemName} = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {inherit glacier;};
        modules = baseModules ++ [./hosts/{systemName}.nix];
      };
      vm = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {inherit glacier;};
        modules =
          baseModules
          ++ [
            (
              {pkgs, ...}: {
                virtualisation.vmVariant = {
                  virtualisation.memorySize = 2048;
                  virtualisation.cores = 2;
                  virtualisation.qemu.options = [
                    "-device virtio-vga-gl"
                    "-display gtk,gl=on"
                  ];
                };
              }
            )
          ];
      };
    };
  };
}
"#;

const CONFIGURATION_TEMPLATE: &str = r#"{
  pkgs,
  config,
  lib,
  glacier,
  ...
}: {
  glacier.enable = true;
  glacier.homeModules = [
    glacier.glacierHomeModules.{wm}
    glacier.glacierHomeModules.{terminal}
    glacier.glacierHomeModules.{shell}
    glacier.glacierHomeModules.{launcher}
    glacier.glacierHomeModules.{browser}
    glacier.glacierHomeModules.{editor}
    glacier.glacierThemeModules.{colors}
  ];
  glacier.{wm}.enable = true;
  glacier.{terminal}.enable = true;
  glacier.{dm}.enable = true;
  glacier.{launcher}.enable = true;
  glacier.{browser}.enable = true;
  glacier.{shell}.enable = true;

  glacier.users.{user} = {
    fullName = "{fullName}";
    emailAddress = "{email}";
    home = ./home.nix;
    wallpapers = [];
  };

  environment.systemPackages = with pkgs; [
    micro
    gnumake
    nautilus
    fzf
  ];

  networking.networkmanager.enable = true;

  nix.settings = {
    max-jobs = "auto";
    cores = 0;
  };

  nix.optimise.automatic = true;

  system.stateVersion = "26.05";
}
"#;

const HOME_TEMPLATE: &str = r#"{
  pkgs,
  config,
  lib,
  glacier,
  ...
}: {
  home.stateVersion = "25.11";
  glacier.wallpaper = config.glacier.defaultWallpapers.{wallpaper};
}
"#;

/// Ask a Select question, returning the chosen value string.
fn pick(prompt: &str, options: &[&str]) -> String {
    let idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(options)
        .default(0)
        .interact()
        .unwrap_or(0);
    options[idx].to_string()
}

/// Ask a free-text question with a default value.
fn ask(prompt: &str, default: &str) -> String {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()
        .unwrap_or_else(|_| default.to_string())
}

/// Run a shell command, printing a spinner while it runs.
/// Returns Ok(()) on success or Err with stderr on failure.
fn run_cmd_spinner(msg: &str, program: &str, args: &[&str]) -> Result<(), String> {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(msg.to_string());

    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {}: {}", program, e))?;

    pb.finish_and_clear();

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Enumerate block devices via lsblk, returning a list of (display_label, device_path).
fn list_disks() -> Vec<(String, String)> {
    let output = Command::new("lsblk")
        .args(["-dpno", "NAME,SIZE,MODEL", "--exclude", "7"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.splitn(3, ' ').collect();
                    if parts.is_empty() || parts[0].is_empty() {
                        return None;
                    }
                    let dev = parts[0].to_string();
                    let label = line.trim().to_string();
                    Some((label, dev))
                })
                .collect()
        }
        _ => vec![],
    }
}

/// Run nixos-install with full log streaming, reusing LogProcessor.
fn run_nixos_install(flake_target: &str, glacier_dir: &str) -> Result<bool, Box<dyn Error>> {
    println!(
        "\n❄️  {} {}...",
        "Installing NixOS for".dimmed(),
        flake_target.cyan().bold()
    );

    let mut child = Command::new("nixos-install")
        .args([
            "--flake",
            flake_target,
            "--verbose",
            "--show-trace",
        ])
        .current_dir(glacier_dir)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut logger = LogProcessor::new();
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // We process both streams in lockstep (simple approach: drain stdout then stderr per line).
    // For a real async impl you'd use threads; here we use two threads to merge into a channel.
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<String>();
    let tx2 = tx.clone();

    std::thread::spawn(move || {
        for line in stdout_reader.lines().flatten() {
            let _ = tx.send(line);
        }
    });
    std::thread::spawn(move || {
        for line in stderr_reader.lines().flatten() {
            let _ = tx2.send(line);
        }
    });

    let mut success = false;
    loop {
        match rx.recv() {
            Ok(line) => {
                if line.contains("--- Switch Complete ---")
                    || line.contains("installation finished")
                {
                    success = true;
                    logger.finish();
                    break;
                }
                if line.contains("--- Switch Failed") || line.contains("error:") {
                    logger.process(&line);
                }
                logger.process(&line);
            }
            Err(_) => {
                // channel closed — all output consumed
                break;
            }
        }
    }

    let status = child.wait()?;
    if status.success() {
        if !success {
            logger.finish();
        }
        Ok(true)
    } else {
        logger.abort();
        Ok(false)
    }
}

/// Append GRUB/EFI boot loader config to an existing hardware nix file.
fn append_bootloader(hw_nix_path: &str, is_uefi: bool) -> Result<(), Box<dyn Error>> {
    let mut content = fs::read_to_string(hw_nix_path)?;

    let boot_block = if is_uefi {
        r#"  boot.loader = {
    efi = {
      canTouchEfiVariables = true;
      efiSysMountPoint = "/boot";
    };
    grub = {
      enable = true;
      device = "nodev";
      efiSupport = true;
    };
  };"#
    } else {
        r#"  boot.loader = {
    grub = {
      enable = true;
      device = "{disk}";
      efiSupport = false;
    };
  };"#
    };

    // Insert before the closing `}` of the top-level attrset.
    if let Some(pos) = content.rfind('}') {
        content.insert_str(pos, &format!("\n{}\n", boot_block));
    } else {
        content.push_str(&format!("\n{}\n}}\n", boot_block));
    }

    fs::write(hw_nix_path, content)?;
    Ok(())
}

/// The interactive installer flow.
fn run_install() -> Result<(), Box<dyn Error>> {
    println!(
        "🏔️  {}\n",
        "Glacier OS Installer".bold().bright_white()
    );

    // ── 1. Firmware type ────────────────────────────────────────────────────
    let firmware = pick("Firmware type", &["UEFI", "BIOS/Legacy"]);
    let is_uefi = firmware == "UEFI";

    // ── 2. Network ──────────────────────────────────────────────────────────
    println!("\n{}", "── Network ──────────────────────────────".dimmed());
    let net_type = pick("Network connection type", &["Ethernet", "Wi-Fi"]);

    match net_type.as_str() {
        "Ethernet" => {
            // List ethernet interfaces
            let iface_output = Command::new("nmcli")
                .args(["-t", "-f", "DEVICE,TYPE", "device"])
                .output()?;
            let ifaces: Vec<String> = String::from_utf8_lossy(&iface_output.stdout)
                .lines()
                .filter(|l| l.contains(":ethernet"))
                .filter_map(|l| l.split(':').next().map(str::to_string))
                .collect();

            let iface = if ifaces.is_empty() {
                ask("Ethernet interface (e.g. eth0)", "eth0")
            } else {
                let labels: Vec<&str> = ifaces.iter().map(String::as_str).collect();
                pick("Select ethernet interface", &labels)
            };

            run_cmd_spinner(
                &format!("Connecting via {}...", iface.cyan()),
                "nmcli",
                &["device", "connect", &iface],
            )
            .map_err(|e| format!("Network error: {}", e))?;
        }
        "Wi-Fi" => {
            // Scan and list SSIDs
            let scan_pb = ProgressBar::new_spinner();
            scan_pb.enable_steady_tick(Duration::from_millis(100));
            scan_pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.cyan} {msg}")
                    .unwrap(),
            );
            scan_pb.set_message("Scanning for Wi-Fi networks...");
            let _ = Command::new("nmcli").args(["device", "wifi", "rescan"]).output();
            std::thread::sleep(Duration::from_secs(2));
            scan_pb.finish_and_clear();

            let wifi_output = Command::new("nmcli")
                .args(["-t", "-f", "SSID", "device", "wifi", "list"])
                .output()?;
            let ssids: Vec<String> = String::from_utf8_lossy(&wifi_output.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty() && *l != "--")
                .map(str::to_string)
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            let ssid = if ssids.is_empty() {
                ask("Wi-Fi SSID", "")
            } else {
                let labels: Vec<&str> = ssids.iter().map(String::as_str).collect();
                pick("Select Wi-Fi network", &labels)
            };

            let password: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Wi-Fi password")
                .allow_empty(true)
                .interact_text()?;

            run_cmd_spinner(
                &format!("Connecting to {}...", ssid.cyan()),
                "nmcli",
                &["device", "wifi", "connect", &ssid, "password", &password],
            )
            .map_err(|e| format!("Wi-Fi error: {}", e))?;
        }
        _ => unreachable!(),
    }

    println!("  {} {}", "✔".green(), "Network connected.".dimmed());

    // ── 3. Disk selection ───────────────────────────────────────────────────
    println!("\n{}", "── Disk ─────────────────────────────────".dimmed());

    let disks = list_disks();
    let disk_device: String = if disks.is_empty() {
        ask("Target disk (e.g. /dev/sda)", "/dev/sda")
    } else {
        let labels: Vec<&str> = disks.iter().map(|(l, _)| l.as_str()).collect();
        let idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select target disk")
            .items(&labels)
            .default(0)
            .interact()
            .unwrap_or(0);
        disks[idx].1.clone()
    };

    // ── 4. Filesystem ───────────────────────────────────────────────────────
    let fs_type = pick("Root filesystem type", &["ext4", "btrfs", "xfs"]);

    // Confirm before wiping
    println!(
        "\n  {} {} {} {}",
        "⚠".yellow().bold(),
        "This will".yellow(),
        "ERASE ALL DATA".red().bold(),
        format!("on {}!", disk_device).yellow()
    );
    let confirmed = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Are you sure you want to continue?")
        .default(false)
        .interact()?;
    if !confirmed {
        println!("{}", "Installation aborted.".red().bold());
        return Ok(());
    }

    // ── 5. Partition & format ────────────────────────────────────────────────
    println!("\n{}", "── Partitioning ─────────────────────────".dimmed());

    if is_uefi {
        // GPT + EFI + root
        run_cmd_spinner(
            &format!("Writing GPT partition table on {}...", disk_device.cyan()),
            "sgdisk",
            &[
                "--zap-all",
                &disk_device,
            ],
        )
        .map_err(|e| format!("sgdisk zap failed: {}", e))?;

        run_cmd_spinner(
            "Creating EFI System Partition (512 MiB)...",
            "sgdisk",
            &[
                "-n", "1:0:+512M",
                "-t", "1:EF00",
                "-c", "1:EFI System",
                &disk_device,
            ],
        )
        .map_err(|e| format!("sgdisk EFI partition failed: {}", e))?;

        run_cmd_spinner(
            &format!("Creating Linux root partition ({})...", fs_type.cyan()),
            "sgdisk",
            &[
                "-n", "2:0:0",
                "-t", "2:8304",
                "-c", "2:Linux Root (x86-64)",
                &disk_device,
            ],
        )
        .map_err(|e| format!("sgdisk root partition failed: {}", e))?;

        // Re-read partition table
        let _ = Command::new("partprobe").arg(&disk_device).output();
        std::thread::sleep(Duration::from_millis(500));

        // Derive partition names (handles /dev/sda -> /dev/sda1, /dev/nvme0n1 -> /dev/nvme0n1p1)
        let (efi_part, root_part) = derive_partitions(&disk_device);

        run_cmd_spinner(
            "Formatting EFI partition as FAT32...",
            "mkfs.fat",
            &["-F", "32", "-n", "BOOT", &efi_part],
        )
        .map_err(|e| format!("mkfs.fat failed: {}", e))?;

        run_cmd_spinner(
            &format!("Formatting root partition as {}...", fs_type.cyan()),
            &format!("mkfs.{}", fs_type),
            &["-L", "nixos", &root_part],
        )
        .map_err(|e| format!("mkfs failed: {}", e))?;

        // Mount
        run_cmd_spinner(
            "Mounting root partition at /mnt...",
            "mount",
            &["-L", "nixos", "/mnt"],
        )
        .map_err(|e| format!("mount root failed: {}", e))?;

        run_cmd_spinner(
            "Creating /mnt/boot...",
            "mkdir",
            &["-p", "/mnt/boot"],
        )
        .map_err(|e| format!("mkdir boot failed: {}", e))?;

        run_cmd_spinner(
            "Mounting EFI partition at /mnt/boot...",
            "mount",
            &["-L", "BOOT", "/mnt/boot"],
        )
        .map_err(|e| format!("mount boot failed: {}", e))?;
    } else {
        // MBR + root only
        run_cmd_spinner(
            &format!("Writing MBR partition table on {}...", disk_device.cyan()),
            "sgdisk",
            &["--zap-all", &disk_device],
        )
        .map_err(|e| format!("sgdisk zap failed: {}", e))?;

        run_cmd_spinner(
            &format!("Creating Linux root partition ({})...", fs_type.cyan()),
            "sgdisk",
            &["-n", "1:0:0", "-t", "1:8300", "-c", "1:Linux Root", &disk_device],
        )
        .map_err(|e| format!("sgdisk root partition failed: {}", e))?;

        let _ = Command::new("partprobe").arg(&disk_device).output();
        std::thread::sleep(Duration::from_millis(500));

        let (root_part, _) = derive_partitions(&disk_device);

        run_cmd_spinner(
            &format!("Formatting root partition as {}...", fs_type.cyan()),
            &format!("mkfs.{}", fs_type),
            &["-L", "nixos", &root_part],
        )
        .map_err(|e| format!("mkfs failed: {}", e))?;

        run_cmd_spinner(
            "Mounting root partition at /mnt...",
            "mount",
            &["-L", "nixos", "/mnt"],
        )
        .map_err(|e| format!("mount root failed: {}", e))?;
    }

    println!("  {} {}", "✔".green(), "Disks formatted and mounted.".dimmed());

    // ── 6. Generate hardware config ──────────────────────────────────────────
    println!("\n{}", "── Hardware Config ──────────────────────".dimmed());
    run_cmd_spinner(
        "Generating hardware configuration...",
        "nixos-generate-config",
        &["--root", "/mnt"],
    )
    .map_err(|e| format!("nixos-generate-config failed: {}", e))?;
    println!("  {} {}", "✔".green(), "Hardware configuration generated.".dimmed());

    // ── 7. Glacier init in /mnt/glacier ──────────────────────────────────────
    println!("\n{}", "── Glacier Configuration ────────────────".dimmed());
    println!(
        "  {} {}",
        "→".cyan(),
        "Setting up your Glacier configuration...".dimmed()
    );

    let glacier_dir = "/mnt/glacier";
    fs::create_dir_all(glacier_dir)?;
    fs::create_dir_all(format!("{}/hosts", glacier_dir))?;

    let mut flake_path = std::path::PathBuf::from(glacier_dir);
    flake_path.push("flake.nix");
    let mut configuration_path = std::path::PathBuf::from(glacier_dir);
    configuration_path.push("configuration.nix");
    let mut home_path = std::path::PathBuf::from(glacier_dir);
    home_path.push("home.nix");

    let system_name = ask("System hostname", "desktop");
    let user = ask("User", "john");
    let full_name = ask("Full Name", "John Doe");
    let email = ask("Email Address", "john@doe.com");
    let wm = pick("Window manager", &["hyprland"]);
    let terminal = pick("Terminal", &["kitty"]);
    let dm = pick("Display manager", &["sddm"]);
    let launcher = pick("Launcher", &["wofi"]);
    let browser = pick("Browser", &["firefox"]);
    let shell = pick("Shell", &["exoshell", "noctalia", "dms"]);
    let editor = pick("Editor", &["micro"]);
    let wallpaper = pick(
        "Wallpaper",
        &["nixos", "mountain", "strips", "gradient"],
    );
    let theme = pick(
        "Theme",
        &[
            "everforest",
            "nord",
            "dracula",
            "catppuccinFrappe",
            "iceberg",
            "icebergDark",
            "kawagama",
            "rosePine",
            "tokyoNight",
            "oxocarbon",
            "monokaiPro",
            "synthwave",
            "atom",
        ],
    );

    let flake = FLAKE_TEMPLATE
        .replace("{systemName}", &system_name)
        .replace("{wm}", &wm)
        .replace("{terminal}", &terminal)
        .replace("{dm}", &dm)
        .replace("{launcher}", &launcher)
        .replace("{browser}", &browser)
        .replace("{shell}", &shell)
        .replace("{editor}", &editor);

    let configuration = CONFIGURATION_TEMPLATE
        .replace("{systemName}", &system_name)
        .replace("{user}", &user)
        .replace("{email}", &email)
        .replace("{colors}", &theme)
        .replace("{fullName}", &full_name)
        .replace("{wm}", &wm)
        .replace("{terminal}", &terminal)
        .replace("{dm}", &dm)
        .replace("{launcher}", &launcher)
        .replace("{browser}", &browser)
        .replace("{shell}", &shell)
        .replace("{editor}", &editor);

    let home = HOME_TEMPLATE.replace("{wallpaper}", &wallpaper);

    fs::write(&flake_path, flake)?;
    fs::write(&configuration_path, configuration)?;
    fs::write(&home_path, home)?;

    println!("  {} {}", "✔".green(), "Glacier config written.".dimmed());

    // ── 8. Move hardware-configuration.nix → hosts/{name}.nix ───────────────
    let hw_src = "/mnt/etc/nixos/hardware-configuration.nix";
    let hw_dst = format!("{}/hosts/{}.nix", glacier_dir, system_name);

    if Path::new(hw_src).exists() {
        fs::copy(hw_src, &hw_dst)?;
        fs::remove_file(hw_src).ok();
        println!(
            "  {} {} {}",
            "✔".green(),
            "Hardware config moved to".dimmed(),
            format!("hosts/{}.nix", system_name).cyan()
        );
    } else {
        eprintln!(
            "  {} {} {}",
            "⚠".yellow(),
            "hardware-configuration.nix not found at".yellow(),
            hw_src.yellow()
        );
    }

    // ── 9. Append bootloader config ──────────────────────────────────────────
    if Path::new(&hw_dst).exists() {
        append_bootloader(&hw_dst, is_uefi)?;
        println!(
            "  {} {}",
            "✔".green(),
            "Boot loader config appended.".dimmed()
        );
    }

    // ── 10. nixos-install ────────────────────────────────────────────────────
    println!("\n{}", "── Installing ───────────────────────────".dimmed());
    let flake_target = format!(".#{}", system_name);
    let install_ok = run_nixos_install(&flake_target, glacier_dir)?;

    if !install_ok {
        eprintln!(
            "\n{}",
            "❌ Installation failed. Check the output above for errors."
                .red()
                .bold()
        );
        std::process::exit(1);
    }

    println!(
        "\n✨ {} {}",
        "NixOS installed successfully!".bold().bright_white(),
        format!("({})", system_name).dimmed()
    );

    // ── 11. Post-install action ──────────────────────────────────────────────
    println!();
    let action = pick(
        "What would you like to do now?",
        &["Reboot", "Drop to shell", "Shutdown"],
    );

    match action.as_str() {
        "Reboot" => {
            println!("{}", "Rebooting...".dimmed());
            let _ = Command::new("reboot").status();
        }
        "Shutdown" => {
            println!("{}", "Shutting down...".dimmed());
            let _ = Command::new("shutdown").args(["-h", "now"]).status();
        }
        "Drop to shell" => {
            println!(
                "  {} {}",
                "→".cyan(),
                "Dropping to shell. Type `exit` when done.".dimmed()
            );
            let _ = Command::new("/bin/sh").status();
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Given a disk device path, return the first and second partition paths.
/// Handles both /dev/sdX (-> /dev/sdX1, /dev/sdX2) and
/// /dev/nvmeXnY (-> /dev/nvmeXnYp1, /dev/nvmeXnYp2) naming.
fn derive_partitions(disk: &str) -> (String, String) {
    let needs_p = disk
        .chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false);
    if needs_p {
        (format!("{}p1", disk), format!("{}p2", disk))
    } else {
        (format!("{}1", disk), format!("{}2", disk))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    // ── install (no daemon needed) ───────────────────────────────────────────
    if let Commands::Install = cli.command {
        return run_install();
    }

    // ── init (no daemon needed) ──────────────────────────────────────────────
    if let Commands::Init = cli.command {
        let mut flake_path = env::current_dir()?;
        flake_path.push("flake.nix");
        if flake_path.exists() {
            eprintln!(
                "{}",
                "flake.nix already exists in this directory.".red().bold()
            );
            std::process::exit(1);
        }

        let mut configuration_path = env::current_dir()?;
        configuration_path.push("configuration.nix");
        if configuration_path.exists() {
            eprintln!(
                "{}",
                "configuration.nix already exists in this directory."
                    .red()
                    .bold()
            );
            std::process::exit(1);
        }

        let mut home_path = env::current_dir()?;
        home_path.push("home.nix");
        if home_path.exists() {
            eprintln!(
                "{}",
                "home.nix already exists in this directory.".red().bold()
            );
            std::process::exit(1);
        }

        println!(
            "🏔️  {}\n",
            "Setting up your Glacier configuration".bold().bright_white()
        );

        let system_name = ask("System hostname", "desktop");
        let user = ask("User", "john");
        let full_name = ask("Full Name", "John Doe");
        let email = ask("Email Address", "john@doe.com");
        let wm = pick("Window manager", &["hyprland"]);
        let terminal = pick("Terminal", &["kitty"]);
        let dm = pick("Display manager", &["sddm"]);
        let launcher = pick("Launcher", &["wofi"]);
        let browser = pick("Browser", &["firefox"]);
        let shell = pick("Shell", &["exoshell", "noctalia", "dms"]);
        let editor = pick("Editor", &["micro"]);
        let wallpaper = pick(
            "Wallpaper",
            &["nixos", "mountain", "strips", "gradient"],
        );
        let theme = pick(
            "Theme",
            &[
                "everforest",
                "nord",
                "dracula",
                "catppuccinFrappe",
                "iceberg",
                "icebergDark",
                "kawagama",
                "rosePine",
                "tokyoNight",
                "oxocarbon",
                "monokaiPro",
                "synthwave",
                "atom",
            ],
        );

        let flake = FLAKE_TEMPLATE
            .replace("{systemName}", &system_name)
            .replace("{wm}", &wm)
            .replace("{terminal}", &terminal)
            .replace("{dm}", &dm)
            .replace("{launcher}", &launcher)
            .replace("{browser}", &browser)
            .replace("{shell}", &shell)
            .replace("{editor}", &editor);

        let configuration = CONFIGURATION_TEMPLATE
            .replace("{systemName}", &system_name)
            .replace("{user}", &user)
            .replace("{email}", &email)
            .replace("{colors}", &theme)
            .replace("{fullName}", &full_name)
            .replace("{wm}", &wm)
            .replace("{terminal}", &terminal)
            .replace("{dm}", &dm)
            .replace("{launcher}", &launcher)
            .replace("{browser}", &browser)
            .replace("{shell}", &shell)
            .replace("{editor}", &editor);

        let home = HOME_TEMPLATE.replace("{wallpaper}", &wallpaper);

        fs::write(&flake_path, flake)?;
        fs::write(&configuration_path, configuration)?;
        fs::write(&home_path, home)?;

        println!(
            "\n✨ {}",
            "Successfully created a Glacier flake.".bold().bright_white()
        );
        print!("   Use ");
        print!(
            "{}",
            format!("sudo glacierctl switch {}", system_name)
                .cyan()
                .bold()
        );
        println!(" to switch to your new config.");

        return Ok(());
    }

    // ── daemon-backed commands ───────────────────────────────────────────────
    let conn = Connection::system().await?;
    let proxy = GlacierProxy::new(&conn).await?;

    match cli.command {
        Commands::Init | Commands::Install => unreachable!(),

        Commands::Switch { name } => {
            let current_dir = env::current_dir()?.to_string_lossy().to_string();

            let up_pb = ProgressBar::new_spinner();
            up_pb.enable_steady_tick(Duration::from_millis(100));
            up_pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.white} {msg}")
                    .unwrap(),
            );
            up_pb.set_message(format!(
                "🏔️  {}",
                "Uploading glacier configuration".bold().white()
            ));

            proxy.upload_config(current_dir).await?;
            up_pb.finish_and_clear();

            println!("⚙️  {} {}...", "Rebuilding".dimmed(), name.cyan());

            let mut log_stream = proxy.receive_stdout_line().await?;
            let mut logger = LogProcessor::new();

            proxy.switch(name).await?;

            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        logger.abort();
                        eprint!("\n{} ", "Cancelling build...".yellow().bold());
                        match proxy.cancel().await {
                            Ok(msg) => eprintln!("{}", msg.dimmed()),
                            Err(e)  => eprintln!("{}", format!("(cancel failed: {e})").red()),
                        }
                        break;
                    }

                    signal = log_stream.next() => {
                        match signal {
                            Some(sig) => {
                                if let Ok(args) = sig.args() {
                                    let line = args.line;
                                    if line.contains("--- Switch Complete ---") {
                                        logger.finish();
                                        break;
                                    }
                                    if line.contains("--- Switch Failed") {
                                        logger.abort();
                                        eprintln!("\n{}", "❌ Build failed. Check the logs above.".red().bold());
                                        break;
                                    }
                                    logger.process(&line);
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
