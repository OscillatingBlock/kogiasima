use std::env::current_dir;
use std::fs::{create_dir_all, remove_dir, remove_dir_all};
use std::path::Path;
use std::process::{Command, Stdio, exit};
use std::thread::sleep;
use std::time::Duration;

use nix::libc::_exit;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::{sys::wait::waitpid, unistd::*};

use std::fs::File;
use std::io::BufReader;

pub mod config;
use crate::config::*;

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
    pub fn build_from_bundle(bundle_path: &Path) -> Result<Container, String> {
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

    pub fn run(&self) {
        self.fork_and_run();
    }

    pub fn fork_and_run(&self) {
        let clone_flags =
            CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;

        let (reader, writer) = nix::unistd::pipe().unwrap();

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child, .. }) => {
                println!(
                    "Continuing execution in parent process, new child has pid: {}",
                    child
                );
                let child_pid_string = child.to_string();

                if let Err(why) = setup_cgroup(
                    &self.cgroup_config.max_pid,
                    &self.cgroup_config.max_memory,
                    &child_pid_string,
                ) {
                    eprintln!("failed to setup cgroup for child process: {why}");
                }

                // 3. OCI compliance: Write 1 byte to the pipe to wake up the child process
                nix::unistd::write(writer, &[1u8]).unwrap();

                waitpid(child, None).unwrap();

                cleanup_cgroup(&child_pid_string);
            }

            //first child process to unshare namespaces and fork again to create the final child process
            //that will run the command
            Ok(ForkResult::Child) => {
                unshare(clone_flags).expect("Failed to unshare namespaces");

                //  OCI compliance: Tell the host we are ready, then block and wait
                // We try to read 1 byte from the pipe. Since the host hasn't written anything,
                // the kernel suspends this child process safely.
                let mut sync_buf = [0u8; 1];
                nix::unistd::read(reader, &mut sync_buf).unwrap();

                if let Err(why) = mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                ) {
                    eprintln!("Failed to remount root filesystem as private: {why}");
                }

                unsafe {
                    // Linux requires a second fork after unshare(CLONE_NEWPID) for the new
                    // PID namespace limits to actually apply to a process.
                    match fork() {
                        Ok(ForkResult::Parent { child, .. }) => {
                            waitpid(child, None).unwrap();

                            _exit(0);
                        }
                        //grand child process that will run the command in the new namespaces
                        Ok(ForkResult::Child) => {
                            setup_child_process(&self.chroot_path);

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
                                .expect("Failed to wait on command");

                            unmount_setup();
                            _exit(0);
                        }
                        Err(why) => {
                            eprintln!("Fork failed: {why}");
                            exit(1);
                        }
                    }
                }
            }
            Err(why) => eprintln!("Fork failed: {why}"),
        }
    }
}

pub fn setup_child_process(chroot_path: &String) {
    //1. setup pivot root to chroot path
    pivot_root_setup(chroot_path);

    //2. set hostname to "mini-docker"
    sethostname("mini-docker").expect("Failed to set hostname");

    //3 mount filesystem
    println!("Mounting proc, sys, and dev filesystems...");
    if let Err(why) = mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        eprintln!("Failed to mount proc filesystem: {why}");
    }

    if let Err(why) = mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        eprintln!("Failed to mount sys filesystem: {why}");
    }

    if let Err(why) = mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        eprintln!("Failed to mount dev filesystem: {why}");
    }
}

fn unmount_setup() {
    println!("Unmounting proc, sys, and dev filesystems...");
    if let Err(why) = nix::mount::umount("/proc") {
        eprintln!("Failed to unmount proc filesystem: {why}");
    }

    if let Err(why) = nix::mount::umount("/sys") {
        eprintln!("Failed to unmount sys filesystem: {why}");
    }

    if let Err(why) = nix::mount::umount("/dev") {
        eprintln!("Failed to unmount dev filesystem: {why}");
    }
    println!("Unmounted proc, sys, and dev filesystems");
}

fn pivot_root_setup(pivot_root_path_string: &String) {
    println!("Setting up pivot root to: {pivot_root_path_string}");

    //1. change to pivot root path
    let pivot_root_path = Path::new(pivot_root_path_string);
    chdir(pivot_root_path).expect("Failed to change directory to pivot root path");

    //2. bind mount pivot root path to itself to make it a mount point
    bind_mount(pivot_root_path_string);

    //3. remove old root directory if it already exists and create new
    let cwd = current_dir().expect("Failed to get current working directory");
    // let old_root_temp_path = cwd.join("old_root/");
    let old_root_temp_path = pivot_root_path.join("old_root");

    println!("cwd = {}", cwd.display());
    println!("old_root = {}", old_root_temp_path.display());

    if old_root_temp_path.exists() {
        println!("removing old_root directory");
        std::fs::remove_dir_all(&old_root_temp_path).unwrap();
    }
    create_dir_all(&old_root_temp_path).expect("Failed to create old root directory");

    //4. pivot root to pivot root path, putting old root in old_root directory
    //Instead of using current_dir(), map old_root strictly INSIDE the target rootfs path
    pivot_root(".", "old_root").expect("Failed to pivot root");
    chdir("/").expect("Failed to change directory to new root");

    //5. unmount old root and remove old root directory
    if let Err(why) = umount2("/old_root", MntFlags::MNT_DETACH) {
        eprintln!("Failed to unmount old root directory: {why}");
    }
    if let Err(why) = remove_dir_all("/old_root") {
        eprintln!("Failed to remove old root directory: {why}");
    }

    //6. chroot into new root
    chroot("/").expect("Failed to chroot");
    chdir("/").expect("Failed to change directory to new root");

    println!("Pivot root setup complete");
}

fn bind_mount(pivot_root_path: &String) {
    mount(
        Some(pivot_root_path.as_str()),
        pivot_root_path.as_str(),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .expect("CRITICAL: Failed to bind mount pivot root path");
}

//set pid limit for the child process using cgroups,
//called by chilld process before pivot root and chroot
//
use std::fs;
use std::path::PathBuf;

pub fn setup_cgroup(max_pid: &str, max_memory: &str, child_pid: &str) -> std::io::Result<()> {
    println!("[cgroup] Initializing limits for child process: {child_pid}");

    let cgroups_path = PathBuf::from(format!("/sys/fs/cgroup/minidocker-{}", child_pid));

    println!("[cgroup] Creating directory at: {:?}", cgroups_path);
    fs::create_dir_all(&cgroups_path)?;

    // Give the kernel pseudo-filesystem a split second to populate the files
    sleep(Duration::from_millis(50));

    // 1. APPLY PID LIMIT
    println!("[cgroup] Configuring pids.max -> {max_pid}");
    fs::write(cgroups_path.join("pids.max"), max_pid)?;

    // 2. APPLY MEMORY LIMIT
    println!("[cgroup] Configuring memory.max -> {max_memory}");
    fs::write(cgroups_path.join("memory.max"), max_memory)?;

    // 3. ATTACH PROCESS TO CGROUP
    println!("[cgroup] Attaching PID {child_pid} to cgroup.procs");
    fs::write(cgroups_path.join("cgroup.procs"), child_pid)?;

    println!("[cgroup] Resource boundaries successfully active.");
    Ok(())
}

fn cleanup_cgroup(child_pid: &String) {
    println!("cleaning up cgroup for child process {child_pid}");
    let cgroups_path = format!("/sys/fs/cgroup/minidocker-{}", child_pid);
    let path = Path::new(&cgroups_path);

    if Path::new(&cgroups_path).exists() {
        let kill_path = path.join("cgroup.kill");
        if kill_path.exists() {
            println!("killing any remaining processes in cgroup");

            if let Err(why) = fs::write(kill_path, "1") {
                eprintln!("Failed to kill processes in cgroup: {why}");
            }

            // Give the kernel a moment to cleanly deregister the dead processes
            sleep(Duration::from_millis(50));
        }
        println!("removing cgroup directory at: {cgroups_path}");

        // MUST use remove_dir, NOT remove_dir_all.
        // Files inside cgroups are virtual kernel APIs, not real files on disk.
        // remove_dir_all will try to delete them individually, triggering EPERM (os error 1).
        if let Err(why) = remove_dir(&cgroups_path) {
            eprintln!("Failed to remove cgroup directory: {why}");
        }
    }
}
