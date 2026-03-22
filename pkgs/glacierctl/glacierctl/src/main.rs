use clap::{Parser, Subcommand};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::time::Duration;
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
            ).unwrap(),
            re_store_path: Regex::new(r"^\s*/nix/store/[a-z0-9]{32}").unwrap(),
            re_err_build: Regex::new(
                r"error: build of '/nix/store/[a-z0-9]+-(.+?)\.drv' failed",
            ).unwrap(),
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
            state.spinner.set_style(ProgressStyle::with_template("  {msg}").unwrap());
            if success {
                state.spinner.finish_with_message(format!(
                    "{} {}", "✔".green(), state.pkg.dimmed()
                ));
            } else {
                state.spinner.finish_with_message(format!(
                    "{} {} {}", "✘".red().bold(), "Failed:".red(), state.pkg.red().bold()
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
        if trimmed.is_empty() { return; }
        if self.re_store_path.is_match(trimmed) { return; }

        if let Some(caps) = self.re_total.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                if let Ok(total) = m.as_str().parse::<u64>() {
                    self.main_pb.set_length(total);
                    self.main_pb.set_style(
                        ProgressStyle::default_bar()
                            .template(" {spinner:.cyan} 🧊 [{bar:30.blue/cyan}] {pos}/{len} {msg}")
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
            if self.active.contains_key(&key) { return; }

            self.main_pb.inc(1);
            self.main_pb.set_message(format!("Forging {}...", stem.bright_blue()));

            let pb = self.mp.insert_before(&self.main_pb, ProgressBar::new_spinner());
            pb.enable_steady_tick(Duration::from_millis(120));
            pb.set_style(ProgressStyle::with_template("  {spinner:.white} {msg}").unwrap());
            pb.set_message(format!("{}", stem.cyan()));

            let prefix = Self::short_prefix(stem);
            self.prefix_to_stem.insert(prefix, key.clone());
            self.active.insert(key, DrvState { pkg: stem.to_string(), spinner: pb });
            return;
        }

        if let Some(caps) = self.re_err_build.captures(trimmed) {
            let stem = caps.get(1).map(|m| m.as_str()).unwrap_or("unknown").to_string();
            self.finalize(&stem, false);
            return;
        }

        if let Some(caps) = self.re_log_prefix.captures(trimmed) {
            let prefix = caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            let msg = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("").trim();
            if msg.is_empty() { return; }

            if let Some(stem) = self.prefix_to_stem.get(&prefix).cloned() {
                if let Some(state) = self.active.get(&stem) {
                    state.spinner.set_message(format!(
                        "{}: {}", state.pkg.cyan(), msg.dimmed()
                    ));
                    if looks_like_error(msg) {
                        let _ = self.mp.println(format!(
                            "  {} {} {}", state.pkg.red(), "│".dimmed(), msg.red().dimmed()
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
        modules = baseModules ++ [./hosts/desktop.nix];
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    if let Commands::Init = cli.command {
        let mut flake_path = env::current_dir()?;
        flake_path.push("flake.nix");

        if flake_path.exists() {
            eprintln!("{}", "flake.nix already exists in this directory.".red().bold());
            std::process::exit(1);
        }

        let mut configuration_path = env::current_dir()?;
        configuration_path.push("configuration.nix");

        if configuration_path.exists() {
            eprintln!("{}", "configuration.nix already exists in this directory.".red().bold());
            std::process::exit(1);
        }

        let mut home_path = env::current_dir()?;
        home_path.push("home.nix");

        if home_path.exists() {
            eprintln!("{}", "home.nix already exists in this directory.".red().bold());
            std::process::exit(1);
        }

        println!("🏔️  {}\n", "Setting up your Glacier configuration".bold().bright_white());

        let system_name = ask("System hostname", "desktop");
        let user     = ask("User",              "john");
        let fullName = ask("Full Name",         "John Doe");
        let email    = ask("Email Address",     "john@doe.com");
        let wm       = pick("Window manager",   &["hyprland"]);
        let terminal = pick("Terminal",         &["kitty"]);
        let dm       = pick("Display manager",  &["sddm"]);
        let launcher = pick("Launcher",         &["wofi"]);
        let browser  = pick("Browser",          &["firefox"]);
        let shell    = pick("Shell",            &["exoshell", "noctalia", "dms"]);
        let editor   = pick("Editor",           &["micro"]);
        let wallpaper= pick("Wallpaper",        &["nixos", "mountain", "strips", "gradient"]);
        let theme    = pick("Theme",            &["everforest", "nord", "dracula", "catppuccinFrappe", "iceberg", "icebergDark", "kawagama", "rosePine", "tokyoNight", "oxocarbon", "monokaiPro", "synthwave", "atom"]);

        let flake = FLAKE_TEMPLATE
            .replace("{systemName}", &system_name)
            .replace("{wm}",        &wm)
            .replace("{terminal}",  &terminal)
            .replace("{dm}",        &dm)
            .replace("{launcher}",  &launcher)
            .replace("{browser}",   &browser)
            .replace("{shell}",     &shell)
            .replace("{editor}",    &editor);

        let configuration = CONFIGURATION_TEMPLATE
            .replace("{systemName}", &system_name)
            .replace("{user}",       &user)
            .replace("{email}",      &email)
            .replace("{colors}",     &theme)
            .replace("{fullName}",   &fullName)
            .replace("{wm}",         &wm)
            .replace("{terminal}",   &terminal)
            .replace("{dm}",         &dm)
            .replace("{launcher}",   &launcher)
            .replace("{browser}",    &browser)
            .replace("{shell}",      &shell)
            .replace("{editor}",     &editor);

        let home = HOME_TEMPLATE
        	.replace("{wallpaper}", &wallpaper);

        fs::write(&flake_path, flake)?;
        fs::write(&configuration_path, configuration)?;
        fs::write(&home_path, home)?;

        println!("\n✨ {}", "Successfully created a Glacier flake.".bold().bright_white());
        print!("   Use ");
        print!("{}", format!("sudo glacierctl switch {}", system_name).cyan().bold());
        println!(" to switch to your new config.");

        return Ok(());
    }

    let conn = Connection::system().await?;
    let proxy = GlacierProxy::new(&conn).await?;

    match cli.command {
        Commands::Init => unreachable!(),

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
