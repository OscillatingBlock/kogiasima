use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio, exit};

use nix::libc::_exit;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::{sys::wait::waitpid, unistd::*};

use tracing::{debug, error, info, instrument, warn};

use anyhow::Context;

use tokio::runtime::Runtime;

use uuid::Uuid;

pub mod config;
use config::*;

pub mod network;
use network::*;

pub mod process;
use process::*;

pub mod cli;

pub struct Container {
    container_command: String,
    args: Vec<String>,
    chroot_path: String,
    cgroup_config: CgroupConfig,
    hostname: String,
    namespaces: Vec<String>,

    env_vars: Vec<String>,
    cwd: String,

    id: Uuid,
}

pub struct CgroupConfig {
    max_pid: String,
    max_memory: String,
}

impl Container {
    pub fn build_from_bundle(bundle_path: &Path) -> anyhow::Result<Container, anyhow::Error> {
        // Look for config.json inside the OCI bundle directory
        let file = File::open(bundle_path)?;
        let reader = BufReader::new(file);

        // Parse using our new structures
        let oci_spec: OciConfig = serde_json::from_reader(reader)?;

        // Safely extract values out of Option wraps with smart fallbacks
        let pids_limit = oci_spec
            .linux
            .as_ref()
            .and_then(|l| l.resources.as_ref())
            .and_then(|r| r.pids.as_ref())
            .map(|p| p.limit.to_string())
            .unwrap_or_else(|| "max".to_string());

        let mem_limit = oci_spec
            .linux
            .as_ref()
            .and_then(|l| l.resources.as_ref())
            .and_then(|r| r.memory.as_ref())
            .and_then(|m| m.limit)
            .map(|m| m.to_string())
            .unwrap_or_else(|| "max".to_string());

        let absolute_chroot_path = if oci_spec.root.path.is_relative() {
            bundle_path.join(&oci_spec.root.path)
        } else {
            oci_spec.root.path.clone()
        };

        let hostname = oci_spec
            .process
            .hostname
            .context("failed to get hostname from config file")?;

        let namespaces: Vec<String> = oci_spec
            .linux
            .as_ref()
            .unwrap()
            .namespaces
            .iter()
            .map(|ns| ns.ns_type.to_string())
            .collect();

        Ok(Container {
            container_command: oci_spec.process.args[0].clone(),
            args: oci_spec.process.args[1..].to_vec(),
            chroot_path: absolute_chroot_path.to_string_lossy().to_string(),
            cgroup_config: CgroupConfig {
                max_pid: pids_limit,
                max_memory: mem_limit,
            },
            namespaces: namespaces,
            hostname: hostname,
            env_vars: oci_spec.process.env,
            cwd: oci_spec.process.cwd,
            id: Uuid::new_v4(),
        })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        self.fork_and_run()?;
        Ok(())
    }

    pub fn clone_flags(&self) -> CloneFlags {
        let mut flags = CloneFlags::empty();

        for ns in &self.namespaces {
            let flag = match ns.to_lowercase().as_str() {
                "uts" => CloneFlags::CLONE_NEWUTS,
                "pid" => CloneFlags::CLONE_NEWPID,
                "mount" => CloneFlags::CLONE_NEWNS,
                "network" => CloneFlags::CLONE_NEWNET,
                "ipc" => CloneFlags::CLONE_NEWIPC,
                "user" => CloneFlags::CLONE_NEWUSER,
                "cgroup" => CloneFlags::CLONE_NEWCGROUP,
                other => {
                    eprintln!("warning: unknown namespace '{other}', skipping");
                    continue;
                }
            };
            flags |= flag;
        }

        flags
    }

    #[instrument(skip(self), fields(command = %self.container_command, chroot = %self.chroot_path))]
    pub fn fork_and_run(&self) -> anyhow::Result<()> {
        let clone_flags = self.clone_flags();
        info!(?clone_flags, "Preparing namespace flags for isolation");

        let (reader, writer) = nix::unistd::pipe().unwrap();

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child, .. }) => {
                info!(
                    child_pid = %child,
                    "Fork seccessful. Monitoring child from parent context",
                );
                let child_pid_string = child.to_string();

                debug!("Ensuring host ipv4 forwarding is enabled");
                enable_host_ip_forwarding().context("Failed to toggle IP forwarding")?;

                setup_cgroup(
                    &self.cgroup_config.max_pid,
                    &self.cgroup_config.max_memory,
                    &child_pid_string,
                )
                .context("Failed to setup cgroup")?;

                let child_pid_string = child.to_string();
                let child_pid_u32 = child_pid_string
                    .parse::<u32>()
                    .context("Failed to parse child pid to u32")?;

                debug!("Starting tokio for host network isolation");
                let rt = Runtime::new().context("Failed to create tokio runtime")?;
                let result = rt.block_on(async {
                    init_network_isolation(child_pid_u32)
                        .await
                        .with_context(|| "failed to init network isolation ".to_string())
                });
                if let Err(err) = result {
                    error!(error= ?err, "[host network] Failed to setup network isolation");
                    return Err(err);
                }

                debug!("Initializing nftables for network isolation");
                setup_nftables(&self.id).context("Failed to setup nftables")?;

                // 3. OCI compliance: Write 1 byte to the pipe to wake up the child process
                debug!("waking up first child process through sync pipe");
                nix::unistd::write(writer, &[1u8]).context("Failed to write to pipe")?;

                debug!("Awaiting first child process termination");
                waitpid(child, None).context("Failed to wait for child process")?;

                info!(child_pid = %child, "Child exited cleanly. Initiating runtime resource cleanup");
                cleanup_cgroup(&child_pid_string).context("Failed to cleanup cgroup")?;

                info!("Removing network isolation rules");
                remove_firewall_rules(&self.id).context("Failed to remove firewall rules")?;
                Ok(())
            }

            //first child process to unshare namespaces and fork again to create the final child process
            //that will run the command
            Ok(ForkResult::Child) => {
                unshare(clone_flags).context("[child] Failed to unshare namespaces")?;

                // OCI compliance: Tell the host we are ready, then block and wait
                // We try to read 1 byte from the pipe. Since the host hasn't written anything,
                // the kernel suspends this child process safely.
                let mut sync_buf = [0u8; 1];
                nix::unistd::read(reader, &mut sync_buf)
                    .context("[child] Failed to read from pipe")?;

                //change root filesystem from shared to private
                //since pivot root requires type of the parent mount of new_root and the
                //parent mount of the current root directory to not be
                //MS_SHARED;
                mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                )
                .context("[child] Failed to remount root filesystem as private")?;

                unsafe {
                    // Linux requires a second fork after unshare(CLONE_NEWPID) for the new
                    // PID namespace limits to actually apply to a process.
                    match fork() {
                        Ok(ForkResult::Parent { child, .. }) => {
                            waitpid(child, None).unwrap();

                            Ok(())
                        }
                        //grand child process that will run the command in the new namespaces
                        Ok(ForkResult::Child) => {
                            setup_child_process(&self.chroot_path, &self)
                                .context("[grand child] Failed to setup grand child process")?;

                            let mut cmd = Command::new(&self.container_command);
                            if !self.args.is_empty() {
                                cmd.args(&self.args);
                            }

                            for env_var in &self.env_vars {
                                if let Some((key, val)) = env_var.split_once('=') {
                                    cmd.env(key, val);
                                }
                            }

                            // 2. OCI COMPLIANCE: Set the internal execution directory (cwd)
                            // We use the root-relative path inside the container
                            cmd.current_dir(&self.cwd);

                            // 3. Bind standard streams and spawn
                            cmd.stdin(Stdio::inherit())
                                .stderr(Stdio::inherit())
                                .stdout(Stdio::inherit())
                                .spawn()
                                .expect("[grand child] Failed to execute command")
                                .wait()
                                .context("[grand child] Failed to wait on command")?;

                            unmount_setup()
                                .context("[grand child] Failed to complete unmount setup ")?;
                            _exit(0);
                        }
                        Err(why) => {
                            eprintln!("Fork failed: {why}");
                            eprintln!("[child] Secondary PID engine fork failed: {why}");

                            exit(1);
                        }
                    }
                }
            }
            Err(why) => {
                eprintln!("Fork failed: {why}");
                Ok(())
            }
        }
    }
}
