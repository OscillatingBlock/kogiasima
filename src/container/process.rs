use std::fs;
use std::fs::{create_dir_all, remove_dir, remove_dir_all};
use std::path::Path;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use tracing::{debug, info};

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::*;

use anyhow::Context;

use tokio::runtime::Runtime;

use crate::container::Container;

use super::network::*;

//used by child process itself, to pivot root, set hostname, mount important file sysetms and config
//container network
pub fn setup_child_process(chroot_path: &String, container: &Container) -> anyhow::Result<()> {
    //dont use tracing in child process functions
    pivot_root_setup(chroot_path).context("Failed to setup pivot root")?;

    println!("[child] isolating system hostname");
    sethostname(container.hostname.as_str()).context("Failed to set hostname")?;

    // Mount essential virtual filesystems after pivot root
    // This ensures `/proc` and `/sys` are available for any network/system tools below.
    println!("[child] Mounting virtual filesystems into new rootfs");
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

    // Network config using Tokio must happen AFTER pivot_root.
    // Creating a multi-threaded Tokio runtime spins up background OS worker threads.
    // Linux strictly forbids `pivot_root` if other threads in the process pool are
    // actively pinning/using the old root filesystem, which triggers a fatal `EBUSY` error.
    println!("starting container network state machine ...");
    let rt = Runtime::new()?;
    let result = rt.block_on(async {
        config_container_network()
            .await
            .context("Failed to config container network: ")
    });

    if let Err(err) = result {
        eprintln!("[child] Critical failure setting up internal networking: {err}");
        return Err(err);
    }

    Ok(())
}

fn pivot_root_setup(pivot_root_path_string: &String) -> anyhow::Result<()> {
    println!("[child] Pivot root targeted to: {pivot_root_path_string}");

    // bind mount pivot root directory, and chdir to it
    let pivot_root_path = Path::new(pivot_root_path_string);
    bind_mount(pivot_root_path_string);
    chdir(pivot_root_path).context("Failed to change directory to pivot root path")?;

    // remove old root directory if it already exists and create new
    let old_root_temp_path = pivot_root_path.join("old_root");
    if old_root_temp_path.exists() {
        std::fs::remove_dir_all(&old_root_temp_path)
            .context("Failed to remove already existing old root directory")?;
    }
    create_dir_all(&old_root_temp_path).with_context(|| {
        format!(
            "Failed to create old root directory at {}",
            old_root_temp_path.to_string_lossy()
        )
    })?;

    nix::unistd::pivot_root(".", "old_root").context("Failed to pivot root")?;
    chdir("/").context("Failed to change directory to new root")?;

    umount2("/old_root", MntFlags::MNT_DETACH).context("Failed to unmount old root")?;
    remove_dir_all("/old_root").context("failed to remove old root directory after unmounting")?;

    chroot("/").context("Failed to chroot ")?;
    chdir("/").context("Failed to change directory to new root")?;

    println!("[child] Pivot root swap complete");
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

pub fn unmount_setup() -> anyhow::Result<()> {
    println!("[child] Unmounting proc, sys, and dev filesystems...");

    nix::mount::umount("/proc").context("Failed to unmount proc filesystem")?;
    nix::mount::umount("/sys").context("Failed to unmount sys filesystem")?;
    nix::mount::umount("/dev").context("Failed to unmount dev filesystem")?;
    Ok(())
}

//called by parent process, can use tracing here
pub fn setup_cgroup(max_pid: &str, max_memory: &str, child_pid: &str) -> anyhow::Result<()> {
    info!(target_pid = %child_pid, "Initializing limits for child process");

    let cgroups_path = PathBuf::from(format!("/sys/fs/cgroup/minidocker-{}", child_pid));
    fs::create_dir_all(&cgroups_path).context("failed to create directories for cgroups")?;

    // Give the kernel pseudo-filesystem a split second to populate the files
    sleep(Duration::from_millis(50));

    debug!(limit = %max_pid, "Writing task thread limit boundaries");
    fs::write(cgroups_path.join("pids.max"), max_pid)
        .context("Error applying pids limit, failed to write to pids.max file ")?;

    debug!(limit = %max_memory, "Writing RAM allocation limitations");
    fs::write(cgroups_path.join("memory.max"), max_memory)
        .context("Error applying memory limit, failed to write to memory.max file ")?;

    info!(target_pid = %child_pid, "Attaching target process tree to cgroup");
    fs::write(cgroups_path.join("cgroup.procs"), child_pid)
        .context("Error inserting child process into cgroups, failed to write cgroup.procs file")?;

    Ok(())
}

pub fn cleanup_cgroup(child_pid: &String) -> anyhow::Result<()> {
    info!(target_pid = %child_pid, "Cleaning child cgroup");
    let cgroups_path = format!("/sys/fs/cgroup/minidocker-{}", child_pid);
    let path = Path::new(&cgroups_path);

    if Path::new(&cgroups_path).exists() {
        let kill_path = path.join("cgroup.kill");
        if kill_path.exists() {
            debug!("killing any remaining processes in cgroup");
            fs::write(kill_path, "1")
                .with_context(|| format!("Failed to kill processes in cgroup:"))?;

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
