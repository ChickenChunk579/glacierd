use std::error::Error;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use zbus::{connection, interface, fdo, object_server::SignalEmitter};
use fs_extra::dir::CopyOptions;

struct Glacier {
    runtime: Handle,
    /// PID of the currently running nixos-rebuild child, if any.
    active_pid: Arc<Mutex<Option<u32>>>,
}

#[interface(name = "com.chickenchunk.Glacier")]
impl Glacier {
    #[zbus(signal)]
    async fn stdout_line(signal_ctxt: &SignalEmitter<'_>, line: String) -> zbus::Result<()>;

    async fn upload_config(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        src_path: String,
    ) -> fdo::Result<String> {
        let dbus_proxy = fdo::DBusProxy::new(conn).await?;
        let sender = header.sender().ok_or_else(|| fdo::Error::Failed("No sender".into()))?;
        let uid = dbus_proxy.get_connection_unix_user(sender.to_owned().into()).await?;
        if uid != 0 {
            return Err(fdo::Error::Failed("Root only".into()));
        }

        let dest = "/var/lib/glacier";
        std::fs::create_dir_all(dest).map_err(|e| fdo::Error::Failed(e.to_string()))?;

        let mut items = Vec::new();
        for entry in std::fs::read_dir(&src_path).map_err(|e| fdo::Error::Failed(e.to_string()))? {
            let entry = entry.map_err(|e| fdo::Error::Failed(e.to_string()))?;
            items.push(entry.path());
        }

        fs_extra::copy_items(&items, dest, &CopyOptions::new().overwrite(true))
            .map_err(|e| fdo::Error::Failed(format!("Copy failed: {}", e)))?;

        Ok(format!("Uploaded contents of {} to {}", src_path, dest))
    }

    /// Kill the currently running nixos-rebuild process, if any.
    async fn cancel(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<String> {
        let dbus_proxy = fdo::DBusProxy::new(conn).await?;
        let sender = header.sender().ok_or_else(|| fdo::Error::Failed("No sender".into()))?;
        let uid = dbus_proxy.get_connection_unix_user(sender.to_owned().into()).await?;
        if uid != 0 {
            return Err(fdo::Error::Failed("Root only".into()));
        }

        let mut guard = self.active_pid.lock().await;
        if let Some(pid) = guard.take() {
            // SIGTERM the process group so child Nix builders are also killed
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
            Ok(format!("Sent SIGTERM to process group {}", pid))
        } else {
            Ok("No active build to cancel".into())
        }
    }

    async fn switch(
        &self,
        #[zbus(signal_context)] signal_ctxt: SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        system_name: String,
    ) -> fdo::Result<String> {
        let dbus_proxy = fdo::DBusProxy::new(conn).await?;
        let sender = header.sender().ok_or_else(|| fdo::Error::Failed("No sender".into()))?;
        let uid = dbus_proxy.get_connection_unix_user(sender.to_owned().into()).await?;
        if uid != 0 {
            return Err(fdo::Error::Failed("Root only".into()));
        }

        let emitter = signal_ctxt.to_owned();
        let name_for_task = system_name.clone();
        let active_pid = Arc::clone(&self.active_pid);

        self.runtime.spawn(async move {
            let flake_arg = format!(".#{}", name_for_task);
            let mut child = match tokio::process::Command::new("nixos-rebuild")
                .args([
                    "switch",
                    "--flake", &flake_arg,
                    "--log-format", "bar-with-logs",
                    "-L",
                    "--show-trace",
                ])
                .current_dir("/var/lib/glacier")
                // Spawn in its own process group so we can SIGTERM the whole tree
                .process_group(0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = Self::stdout_line(&emitter, format!("Error: {}", e)).await;
                    return;
                }
            };

            // Store the PID so cancel() can reach it
            if let Some(pid) = child.id() {
                *active_pid.lock().await = Some(pid);
            }

            let mut out_reader = BufReader::new(child.stdout.take().unwrap()).lines();
            let mut err_reader = BufReader::new(child.stderr.take().unwrap()).lines();

            loop {
                tokio::select! {
                    res = out_reader.next_line() => {
                        match res {
                            Ok(Some(line)) => { let _ = Self::stdout_line(&emitter, line).await; }
                            _ => break,
                        }
                    }
                    res = err_reader.next_line() => {
                        if let Ok(Some(line)) = res {
                            let _ = Self::stdout_line(&emitter, line).await;
                        }
                    }
                }
            }

            // Clear the stored PID before reporting outcome
            active_pid.lock().await.take();

            match child.wait().await {
                Ok(s) if s.success() => {
                    let _ = Self::stdout_line(&emitter, "--- Switch Complete ---".to_string()).await;
                }
                Ok(s) => {
                    let _ = Self::stdout_line(
                        &emitter,
                        format!("--- Switch Failed with code {} ---", s.code().unwrap_or(1)),
                    )
                    .await;
                }
                Err(e) => {
                    let _ = Self::stdout_line(&emitter, format!("--- Switch Error: {} ---", e)).await;
                }
            }
        });

        Ok(format!("Switching to {}...", system_name))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let glacier = Glacier {
        runtime: Handle::current(),
        active_pid: Arc::new(Mutex::new(None)),
    };

    let _conn = connection::Builder::system()?
        .name("com.chickenchunk.Glacier")?
        .serve_at("/com/chickenchunk/Glacier", glacier)?
        .build()
        .await?;

    println!("Glacier running on System Bus...");
    std::future::pending::<()>().await;
    Ok(())
}
