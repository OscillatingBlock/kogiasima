use std::env::current_dir;
use std::fs::{create_dir_all, remove_dir, remove_dir_all};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::*;

use anyhow::Context;

use tokio::runtime::Runtime;

use super::network::*;

pub fn setup_child_process(chroot_path: &String) -> anyhow::Result<()> {
    println!("dumping child fds ...");
    for fd in std::fs::read_dir("/proc/self/fd")? {
        println!("{:?}", fd?.path());
    }
    pivot_root_setup(chroot_path).context("Failed to setup pivot root")?;

    // 2. Set hostname to "mini-docker"
    sethostname("mini-docker").context("Failed to set hostname")?;

    // 3. Mount essential virtual filesystems IMMEDIATELY after pivot root
    // This ensures `/proc` and `/sys` are available for any network/system tools below.
    println!("Mounting proc, sys, and dev filesystems...");
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .context("Failed to mount proc filesystem")?;

    mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .context("Failed to mount sys filesystem")?;

    mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .context("Failed to mount dev filesystem")?;

    // 5. Container side network configurations (safe to spawn Tokio threads now)
    // CRITICAL: Network config using Tokio must happen AFTER pivot_root.
    // Creating a multi-threaded Tokio runtime spins up background OS worker threads.
    // Linux strictly forbids `pivot_root` if other threads in the process pool are
    // actively pinning/using the old root filesystem, which triggers a fatal `EBUSY` error.
    let rt = Runtime::new()?;
    let result = rt.block_on(async {
        config_container_network()
            .await
            .context("Failed to config container network: ")
    });

    if let Err(err) = result {
        eprintln!("[container network] Failed to setup network isolation: {err}");
        return Err(err);
    }

    Ok(())
}

pub fn unmount_setup() -> anyhow::Result<()> {
    println!("Unmounting proc, sys, and dev filesystems...");

    nix::mount::umount("/proc").context("Failed to unmount proc filesystem")?;
    nix::mount::umount("/sys").context("Failed to unmount sys filesystem")?;
    nix::mount::umount("/dev").context("Failed to unmount dev filesystem")?;

    println!("Unmounted proc, sys, and dev filesystems");
    Ok(())
}

fn pivot_root_setup(pivot_root_path_string: &String) -> anyhow::Result<()> {
    println!("Setting up pivot root to: {pivot_root_path_string}");

    //1. change to pivot root path
    let pivot_root_path = Path::new(pivot_root_path_string);

    //2. bind mount pivot root path to itself to make it a mount point
    bind_mount(pivot_root_path_string);
    chdir(pivot_root_path).context("Failed to change directory to pivot root path")?;

    //3. remove old root directory if it already exists and create new
    let cwd = current_dir().expect("Failed to get current working directory");
    // let old_root_temp_path = cwd.join("old_root/");
    let old_root_temp_path = pivot_root_path.join("old_root");

    println!("cwd = {}", cwd.display());
    println!("old_root = {}", old_root_temp_path.display());

    if old_root_temp_path.exists() {
        println!("removing old_root directory");
        std::fs::remove_dir_all(&old_root_temp_path)
            .context("Failed to remove already existing old root directory")?;
    }
    create_dir_all(&old_root_temp_path).with_context(|| {
        format!(
            "Failed to create old root directory at {}",
            old_root_temp_path.to_string_lossy()
        )
    })?;

    //4. pivot root to pivot root path, putting old root in old_root directory
    //Instead of using current_dir(), map old_root strictly INSIDE the target rootfs path
    pivot_root(".", "old_root").context("Failed to pivot root")?;
    chdir("/").context("Failed to change directory to new root")?;

    //5. unmount old root and remove old root directory
    umount2("/old_root", MntFlags::MNT_DETACH).context("Failed to unmount old root")?;
    remove_dir_all("/old_root").context("failed to remove old root directory after unmounting")?;

    //6. chroot into new root
    chroot("/").context("Failed to chroot ")?;
    chdir("/").context("Failed to change directory to new root")?;

    println!("Pivot root setup complete");
    Ok(())
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

pub fn setup_cgroup(max_pid: &str, max_memory: &str, child_pid: &str) -> anyhow::Result<()> {
    println!("[cgroup] Initializing limits for child process: {child_pid}");

    let cgroups_path = PathBuf::from(format!("/sys/fs/cgroup/minidocker-{}", child_pid));

    println!("[cgroup] Creating directory at: {:?}", cgroups_path);
    //use contexxt when we have to return plain strings
    fs::create_dir_all(&cgroups_path).context("failed to create directories for cgroups")?;

    // Give the kernel pseudo-filesystem a split second to populate the files
    sleep(Duration::from_millis(50));

    // 1. APPLY PID LIMIT
    println!("[cgroup] Configuring pids.max -> {max_pid}");
    fs::write(cgroups_path.join("pids.max"), max_pid)
        .context("Error applying pids limit, failed to write to pids.max file ")?;

    // 2. APPLY MEMORY LIMIT
    println!("[cgroup] Configuring memory.max -> {max_memory}");
    fs::write(cgroups_path.join("memory.max"), max_memory)
        .context("Error applying memory limit, failed to write to memory.max file ")?;

    // 3. ATTACH PROCESS TO CGROUP
    println!("[cgroup] Attaching PID {child_pid} to cgroup.procs");
    fs::write(cgroups_path.join("cgroup.procs"), child_pid)
        .context("Error inserting child process into cgroups, failed to write cgroup.procs file")?;

    println!("[cgroup] Resource boundaries successfully active.");
    Ok(())
}

pub fn cleanup_cgroup(child_pid: &String) -> anyhow::Result<()> {
    println!("cleaning up cgroup for child process {child_pid}");
    let cgroups_path = format!("/sys/fs/cgroup/minidocker-{}", child_pid);
    let path = Path::new(&cgroups_path);

    if Path::new(&cgroups_path).exists() {
        let kill_path = path.join("cgroup.kill");
        if kill_path.exists() {
            println!("killing any remaining processes in cgroup");

            fs::write(kill_path, "1")
                .with_context(|| format!("Failed to kill processes in cgroup:"))?;

            // Give the kernel a moment to cleanly deregister the dead processes
            sleep(Duration::from_millis(50));
        }
        println!("removing cgroup directory at: {cgroups_path}");

        // MUST use remove_dir, NOT remove_dir_all.
        // Files inside cgroups are virtual kernel APIs, not real files on disk.
        // remove_dir_all will try to delete them individually, triggering EPERM (os error 1).
        remove_dir(&cgroups_path).context("Failed to remove cgroup directory: ")?;
    }
    Ok(())
}
