use std::env;
use std::env::current_dir;
use std::fs::{create_dir_all, remove_dir, remove_dir_all};
use std::path::Path;
use std::process;
use std::process::{Command, Stdio, exit};
use std::thread::sleep;
use std::time::Duration;

use nix::libc::_exit;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::{sys::wait::waitpid, unistd::*};

pub struct Container {
    container_command: String,
    args: Vec<String>,
    chroot_path: String,
    cgroup_config: CgroupConfig,
}

pub struct CgroupConfig {
    max_pid: String,
    max_memory: String,
}

impl Container {
    pub fn build(args: Vec<String>) -> Result<Container, String> {
        if args.len() < 1 {
            return Err("You must provide a command to run in the container".to_string());
        }

        let chroot_path = env::var("MINI_DOCKER_CHROOT").unwrap_or_else(|_| {
            eprintln!("error: MINI_DOCKER_CHROOT environment variable is not set");
            process::exit(1);
        });

        let max_pid = env::var("MAX_PID").unwrap_or_else(|_| "max".to_string());
        let max_memory = env::var("MAX_MEMORY").unwrap_or_else(|_| "max".to_string());

        Ok(Container {
            container_command: args[0].clone(),
            args: args[1..].to_vec(),
            chroot_path: chroot_path,
            cgroup_config: CgroupConfig {
                max_pid,
                max_memory,
            },
        })
    }

    pub fn run(&self) {
        self.fork_and_run();
    }

    pub fn fork_and_run(&self) {
        let clone_flags =
            CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;

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

                waitpid(child, None).unwrap();

                cleanup_cgroup(&child_pid_string);
            }

            //first child process to unshare namespaces and fork again to create the final child process
            //that will run the command
            Ok(ForkResult::Child) => {
                unshare(clone_flags).expect("Failed to unshare namespaces");
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

                            //execute the command provided in args
                            if self.args.len() > 1 {
                                Command::new(&self.container_command)
                                    .args(&self.args)
                                    .stdin(Stdio::inherit())
                                    .stderr(Stdio::inherit())
                                    .stdout(Stdio::inherit())
                                    .spawn()
                                    .expect("Failed to execute command")
                                    .wait()
                                    .expect("Failed to wait on command");
                            } else {
                                Command::new(&self.container_command)
                                    .stdin(Stdio::inherit())
                                    .stderr(Stdio::inherit())
                                    .stdout(Stdio::inherit())
                                    .spawn()
                                    .expect("Failed to execute command")
                                    .wait()
                                    .expect("Failed to wait on command");
                            }

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
    let old_root_temp_path = cwd.join("old_root/");

    println!("cwd = {}", cwd.display());
    println!("old_root = {}", old_root_temp_path.display());

    if old_root_temp_path.exists() {
        println!("removing old_root directory");
        std::fs::remove_dir_all(&old_root_temp_path).unwrap();
    }
    create_dir_all(&old_root_temp_path).expect("Failed to create old root directory");

    //4. pivot root to pivot root path, putting old root in old_root directory
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
    if let Err(why) = mount(
        Some(pivot_root_path.as_str()),
        pivot_root_path.as_str(),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    ) {
        eprintln!("Failed to bind mount pivot root path: {why}");
    }
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
