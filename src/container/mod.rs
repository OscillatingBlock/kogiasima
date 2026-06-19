use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio, exit};

use nix::libc::_exit;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::{sys::wait::waitpid, unistd::*};

use anyhow::Context;

use tokio::runtime::Runtime;

pub mod config;
use config::*;

pub mod network;
use network::*;

pub mod process;
use process::*;

pub struct Container {
    container_command: String,
    args: Vec<String>,
    chroot_path: String,
    cgroup_config: CgroupConfig,

    env_vars: Vec<String>,
    cwd: String,
}

pub struct CgroupConfig {
    max_pid: String,
    max_memory: String,
}

impl Container {
    pub fn build_from_bundle(bundle_path: &Path) -> anyhow::Result<Container, String> {
        // Look for config.json inside the OCI bundle directory
        let config_path = bundle_path.join("config.json");
        let file =
            File::open(&config_path).map_err(|e| format!("Failed to open config.json: {e}"))?;
        let reader = BufReader::new(file);

        // Parse using our new structures
        let oci_spec: OciConfig = serde_json::from_reader(reader)
            .map_err(|e| format!("Parsing OCI config failed: {e}"))?;

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

        Ok(Container {
            container_command: oci_spec.process.args[0].clone(),
            args: oci_spec.process.args[1..].to_vec(),
            chroot_path: absolute_chroot_path.to_string_lossy().to_string(),
            cgroup_config: CgroupConfig {
                max_pid: pids_limit,
                max_memory: mem_limit,
            },
            env_vars: oci_spec.process.env,
            cwd: oci_spec.process.cwd,
        })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        self.fork_and_run()?;
        Ok(())
    }

    pub fn fork_and_run(&self) -> anyhow::Result<()> {
        let clone_flags = CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWNET;

        let (reader, writer) = nix::unistd::pipe().unwrap();

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child, .. }) => {
                println!(
                    "Continuing execution in parent process, new child has pid: {}",
                    child
                );
                let child_pid_string = child.to_string();

                // Make sure this is running!
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

                let rt = Runtime::new().context("Failed to create tokio runtime")?;
                let result = rt.block_on(async {
                    init_network_isolation(child_pid_u32)
                        .await
                        .with_context(|| "failed to init network isolation ".to_string())
                });
                if let Err(result) = result {
                    eprintln!("[host network] Failed to setup network isolation: {result}");
                    return Err(result);
                }

                setup_nftables().context("Failed to setup nftables")?;

                // 3. OCI compliance: Write 1 byte to the pipe to wake up the child process
                nix::unistd::write(writer, &[1u8]).context("Failed to write to pipe")?;

                waitpid(child, None).context("Failed to wait for child process")?;

                cleanup_cgroup(&child_pid_string).context("Failed to cleanup cgroup")?;
                Ok(())
            }

            //first child process to unshare namespaces and fork again to create the final child process
            //that will run the command
            Ok(ForkResult::Child) => {
                unshare(clone_flags).context("Failed to unshare namespaces")?;

                //  OCI compliance: Tell the host we are ready, then block and wait
                // We try to read 1 byte from the pipe. Since the host hasn't written anything,
                // the kernel suspends this child process safely.
                let mut sync_buf = [0u8; 1];
                nix::unistd::read(reader, &mut sync_buf).context("failed to read from pipe")?;

                println!("dumping first child fds before setting up grandchild...");
                for fd in std::fs::read_dir("/proc/self/fd")? {
                    println!("{:?}", fd?.path());
                }

                //make root filesystem private
                //// CRITICAL: Prevent systemd shared mount propagation from leaking to/from the host.
                // Must happen immediately after `unshare` and before the sync pipe/second fork
                // to avoid a race condition that triggers an `EBUSY` error during `pivot_root`.
                mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                )
                .context("Failed to remount root filesystem as private")?;

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
                            setup_child_process(&self.chroot_path)
                                .context("Failed to setup child process")?;

                            let mut cmd = Command::new(&self.container_command);
                            if !self.args.is_empty() {
                                cmd.args(&self.args);
                            }

                            // 1. OCI COMPLIANCE: Inject the parsed environment variables array natively
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
                                .expect("Failed to execute command")
                                .wait()
                                .context("Failed to wait on command")?;

                            unmount_setup().context("Failed to complete unmount setup ")?;
                            _exit(0);
                        }
                        Err(why) => {
                            eprintln!("Fork failed: {why}");
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
