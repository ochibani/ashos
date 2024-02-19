use crate::{check_mutability, chr_delete, chroot_exec, get_current_snapshot, get_tmp,
            immutability_disable, immutability_enable, is_system_pkg, is_system_locked,
            post_transactions, prepare, remove_dir_content, snapshot_config_get, sync_time};

use configparser::ini::{Ini, WriteOptions};
use rustix::path::Arg;
use std::fs::{DirBuilder, File, metadata, OpenOptions, read_dir};
use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus};
use tempfile::TempDir;
use users::get_user_by_name;
use users::os::unix::UserExt;
use walkdir::WalkDir;

// Noninteractive update
pub fn auto_upgrade(snapshot: &str) -> Result<(), Error> {
    // Make sure snapshot exists
    if !Path::new(&format!("/.snapshots/rootfs/snapshot-{}", snapshot)).try_exists()? {
        eprintln!("Cannot upgrade as snapshot {} doesn't exist.", snapshot);

    } else {
        // Required in virtualbox, otherwise error in package db update
        sync_time()?;
        prepare(snapshot)?;

        // Use apt
        let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
                                           .args(["apt-get", "update", "-y"]).status()?;
        if excode.success() {
            post_transactions(snapshot)?;
            let mut file = OpenOptions::new().write(true)
                                             .create(true)
                                             .truncate(true)
                                             .open("/.snapshots/ash/upstate")?;
            file.write_all("0 ".as_bytes())?;
            let mut file = OpenOptions::new().append(true)
                                             .open("/.snapshots/ash/upstate")?;
            let date = Command::new("date").output()?;
            file.write_all(format!("\n{}", &date.stdout.to_string_lossy().as_str()?).as_bytes())?;
        } else {
            chr_delete(snapshot)?;
            let mut file = OpenOptions::new().write(true)
                                             .create(true)
                                             .truncate(true)
                                             .open("/.snapshots/ash/upstate")?;
            file.write_all("1 ".as_bytes())?;
            let mut file = OpenOptions::new().append(true)
                                             .open("/.snapshots/ash/upstate")?;
            let date = Command::new("date").output()?;
            file.write_all(format!("\n{}", &date.stdout.to_string_lossy().as_str()?).as_bytes())?;
        }
    }
    Ok(())
}

// Copy cache of downloaded packages to shared
pub fn cache_copy(snapshot: &str, prepare: bool) -> Result<(), Error> {
    let tmp = get_tmp();
    if prepare {
        Command::new("cp").args(["-n", "-r", "--reflink=auto"])
                          .arg(format!("/.snapshots/rootfs/snapshot-{}/var/cache/apt", snapshot))
                          .arg(format!("/.snapshots/rootfs/snapshot-chr{}/var/cache/apt", tmp))
                          .output().unwrap();
    } else {
        Command::new("cp").args(["-n", "-r", "--reflink=auto"])
                          .arg(format!("/.snapshots/rootfs/snapshot-chr{}/var/cache/apt", snapshot))
                          .arg(format!("/.snapshots/rootfs/snapshot-{}/var/cache/apt", tmp))
                          .output().unwrap();
    }
    Ok(())
}

// Clean apt cache
pub fn clean_cache(snapshot: &str) -> Result<(), Error> {
    if Path::new(&format!("/.snapshots/rootfs/snapshot-chr{}/var/cache/apt", snapshot)).try_exists().unwrap() {
        remove_dir_content(&format!("/.snapshots/rootfs/snapshot-chr{}/var/cache/apt", snapshot))?;
    }
    Ok(())
}

// Uninstall all packages in snapshot
pub fn clean_chroot(snapshot: &str, profconf: &Ini) -> Result<(), Error> {
    // Read commands section in configuration file
    if profconf.sections().contains(&"uninstall-commands".to_string()) {
        for cmd in profconf.get_map().unwrap().get("uninstall-commands").unwrap().keys() {
            chroot_exec(&format!("/.snapshots/rootfs/snapshot-chr{}", snapshot), cmd)?;
        }
    }
    let print_arg = "'{print $1}'";
    let excode = Command::new("chroot")
        .arg(format!("/.snapshots/rootfs/snapshot-chr{} apt-get remove --purge $(dpkg --get-selections | awk {}) --allow-remove-essential -y",
                     snapshot,print_arg)).status()?;

    if !excode.success() {
        return Err(Error::new(ErrorKind::Other,
                              format!("Failed remove packages from snapshot {} chroot.", snapshot)));
    }
    Ok(())
}

// Fix signature invalid error
pub fn fix_package_db(snapshot: &str) -> Result<(), Error> {
    // Make sure snapshot does exist
    if !Path::new(&format!("/.snapshots/rootfs/snapshot-{}", snapshot)).try_exists()? && !snapshot.is_empty() {
        return Err(Error::new(ErrorKind::NotFound,
                              format!("Cannot fix package man database as snapshot {} doesn't exist.", snapshot)));

        // Make sure snapshot is not in use
        } else if Path::new(&format!("/.snapshots/rootfs/snapshot-chr{}", snapshot)).try_exists()? {
        return Err(
            Error::new(ErrorKind::Unsupported,
                       format!("Snapshot {} appears to be in use. If you're certain it's not in use, clear lock with 'ash unlock -s {}'.",
                               snapshot,snapshot)));

    } else if snapshot.is_empty() && get_current_snapshot() == "0" {
        // Base snapshot unsupported
        return Err(Error::new(ErrorKind::Unsupported, format!("Snapshot 0 (base) should not be modified.")));

    } else if snapshot == "0" {
        // Base snapshot unsupported
        return Err(Error::new(ErrorKind::Unsupported, format!("Snapshot 0 (base) should not be modified.")));

    } else {
        let run_chroot: bool;
        // If snapshot is current running
        run_chroot = if snapshot.is_empty() {
            false
        } else {
            true
        };

        // Snapshot is mutable so do not make it immutable after fixdb is done
        let flip = if check_mutability(snapshot) {
            false
        } else {
            if immutability_disable(snapshot).is_ok() {
                println!("Snapshot {} successfully made mutable.", snapshot);
            }
            true
        };

        // Fix package database
        if run_chroot {
            prepare(snapshot)?;
        }
        if run_chroot {
            //TODO run in chroot
        } else {
            // Update database
            let update = Command::new("apt-get").arg("update").output()?;

            // Convert the output to a string
            let output = String::from_utf8_lossy(&update.stdout);

            // Extract the missing GPG keys and their corresponding repository names
            let missing_keys: Vec<(String, String)> = output
                .lines()
                .filter(|line| line.contains("NO_PUBKEY"))
                .map(|line| {
                    let parts: Vec<&str> = line.split(" ").collect();
                    let key = parts.last().unwrap().to_string();
                    let repo = parts.get(parts.len() - 2).unwrap_or(&"Unknown repository").to_string();
                    (key, repo)
                }).collect();
            // Import the missing GPG keys
            for (key, repo) in missing_keys {
                let gpg_import = Command::new("gpg")
                    .args(&["--keyserver", "hkp://keyserver.ubuntu.com:80", "--recv-keys", &key])
                    .status()?;
                if !gpg_import.success() {
                    return Err(Error::new(ErrorKind::Other,
                                          "Failed to import gpg keys."));
                }

                let gpg_export = Command::new("sh").arg("-c")
                                                   .arg(&format!("gpg --export {} | tee /usr/share/keyrings/{}.gpg", &key,&repo))
                                                   .status()?;
                if !gpg_export.success() {
                    return Err(Error::new(ErrorKind::Other,
                                          "Failed to export gpg keys."));
                }
                // TODO change deb $repo to use /usr/share/keyrings
            }
        }

        if snapshot.is_empty() {
            let snapshot = get_current_snapshot();
            prepare(&snapshot)?;
            refresh_helper(&snapshot).expect("Refresh failed.");
        }

        // Return snapshot to immutable after fixdb is done if snapshot was immutable
        if flip {
            if immutability_enable(snapshot).is_ok() {
                println!("Snapshot {} successfully made immutable.", snapshot);
            }
        }
    }
    Ok(())
}

// Install atomic-operation
pub fn install_package_helper(snapshot:&str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    prepare(snapshot)?;
    //Profile configurations
    let cfile = format!("/.snapshots/rootfs/snapshot-chr{}/etc/ash/profile", snapshot);
    let mut profconf = Ini::new();
    profconf.set_comment_symbols(&['#']);
    profconf.set_multiline(true);
    let mut write_options = WriteOptions::default();
    write_options.blank_lines_between_sections = 1;
    // Load profile
    profconf.load(&cfile).unwrap();

    for pkg in pkgs {
        let mut pkgs_list: Vec<String> = Vec::new();
        for pkg in profconf.get_map().unwrap().get("profile-packages").unwrap().keys() {
            pkgs_list.push(pkg.to_string());
        }
        // Nocomfirm flag
        let install_args = if noconfirm {
            format!("apt-get install {} -y", pkg)
        } else {
            format!("apt-get install {}", pkg)
        };
        // Install packages using apt
        let excode = Command::new("chroot")
            .arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
            .arg(&install_args)
            .status()?;
        if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to install {}.", pkg)));
        // Add to profile-packages if not system package
        } else if !pkgs_list.contains(pkg) && !is_system_pkg(&profconf, pkg.to_string()) {
            pkgs_list.push(pkg.to_string());
            pkgs_list.sort();
            for key in pkgs_list {
                profconf.remove_key("profile-packages", &key);
                profconf.set("profile-packages", &key, None);
            }
            profconf.pretty_write(&cfile, &write_options)?;
        }
    }
    Ok(())
}

// Install atomic-operation
pub fn install_package_helper_chroot(snapshot:&str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    for pkg in pkgs {
        let install_args = if noconfirm {
            format!("apt-get install {} -y", pkg)
        } else {
            format!("apt-get install {}", pkg)
        };

        let excode = Command::new("sh").arg("-c")
                                       .arg(format!("chroot /.snapshots/rootfs/snapshot-chr{} {}", snapshot,install_args))
                                       .status()?;
        if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to install {}.", pkg)));
        }
    }
    Ok(())
}

// Install atomic-operation in live snapshot
pub fn install_package_helper_live(snapshot: &str, tmp: &str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    for pkg in pkgs {
        let install_args = if noconfirm {
            format!("apt-get install {} -y", pkg)
        } else {
            format!("apt-get install {}", pkg)
        };

        let excode = Command::new("sh")
                .arg("-c")
                .arg(format!("chroot /.snapshots/rootfs/snapshot-{} {}", tmp,install_args))
                .status()?;

        if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to install {}.", pkg)));
        }
    }
    Ok(())
}

// Check if service enabled
pub fn is_service_enabled(snapshot: &str, service: &str) -> bool {
    if Path::new("/var/lib/systemd/").try_exists().unwrap() {
        let excode = Command::new("sh").arg("-c")
                                       .arg(format!("chroot /.snapshots/rootfs/snapshot-chr{} systemctl is-enabled {}", snapshot,service))
                                       .output().unwrap();
        let stdout = String::from_utf8_lossy(&excode.stdout).trim().to_string();
        if stdout == "enabled" {
            return true;
        } else {
            return false;
        }
    } else {
        // TODO add OpenRC
        return false;
    }
}

// Prevent system packages from being automatically removed
pub fn lockpkg(snapshot:&str, profconf: &Ini) -> Result<(), Error> {
    Ok(())
}

// Get list of installed packages and exclude packages installed as dependencies
pub fn no_dep_pkg_list(snapshot: &str, chr: &str) /*-> Vec<String>*/ {
}

// Reinstall base packages in snapshot
pub fn bootstrap(snapshot: &str) -> Result<(), Error> {
   Ok(())
}

// Get list of packages installed in a snapshot
pub fn pkg_list(snapshot: &str, chr: &str) /*-> Vec<String>*/ {
}

// APT query
pub fn pkg_query(pkg: &str) /*-> Result<ExitStatus, Error>*/ {
}

// Refresh snapshot atomic-operation
pub fn refresh_helper(snapshot: &str) -> Result<(), Error> {
    let refresh = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
                                        .args(["apt-get", "update", "-y"])
                                        .status()?;
    if !refresh.success() {
        return Err(Error::new(ErrorKind::Other,
                              "Refresh failed."));
    }
    Ok(())
}

// Disable service(s) (Systemd, OpenRC, etc.)
pub fn service_disable(snapshot: &str, services: &Vec<String>, chr: &str) -> Result<(), Error> {
    for service in services {
        // Systemd
        if Path::new("/var/lib/systemd/").try_exists()? {
            let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-{}{}", chr,snapshot))
                                               .arg("systemctl")
                                               .arg("disable")
                                               .arg(&service).status()?;
            if !excode.success() {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Failed to disable {}.", service)));
            }
        } //TODO add OpenRC
    }
    Ok(())
}

// Enable service(s) (Systemd, OpenRC, etc.)
pub fn service_enable(snapshot: &str, services: &Vec<String>, chr: &str) -> Result<(), Error> {
    for service in services {
        // Systemd
        if Path::new("/var/lib/systemd/").try_exists()? {
            let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-{}{}", chr,snapshot))
                                               .arg("systemctl")
                                               .arg("enable")
                                               .arg(&service).status()?;
            if !excode.success() {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Failed to enable {}.", service)));
            }
        } //TODO add OspenRC
    }
    Ok(())
}

// Show diff of packages between 2 snapshots
pub fn snapshot_diff(snapshot1: &str, snapshot2: &str) -> Result<(), Error> {
    Ok(())
}

// Copy system configurations to new snapshot
pub fn system_config(snapshot: &str, profconf: &Ini) -> Result<(), Error> {
   Ok(())
}

// Sync tree helper function
pub fn tree_sync_helper(s_f: &str, s_t: &str, chr: &str) -> Result<(), Error>  {
   Ok(())
}

// Uninstall package(s) atomic-operation
pub fn uninstall_package_helper(snapshot: &str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    // Profile configurations
    let cfile = format!("/.snapshots/rootfs/snapshot-chr{}/etc/ash/profile", snapshot);
    let mut profconf = Ini::new();
    profconf.set_comment_symbols(&['#']);
    profconf.set_multiline(true);
    let mut write_options = WriteOptions::default();
    write_options.blank_lines_between_sections = 1;
    // Load profile
    profconf.load(&cfile).unwrap();

    for pkg in pkgs {
        let mut pkgs_list: Vec<String> = Vec::new();
        for pkg in profconf.get_map().unwrap().get("profile-packages").unwrap().keys() {
            pkgs_list.push(pkg.to_string());
        }
        let uninstall_args = if noconfirm {
            ["apt-get", "remove", "-y"]
        } else {
            ["apt-get", "remove", ""]
        };

        if !is_system_locked() || !is_system_pkg(&profconf, pkg.to_string()) {
            let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
                                               .args(uninstall_args)
                                               .arg(format!("{}", pkg)).status()?;

            if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to uninstall {}.", pkg)));
            } else if pkgs_list.contains(pkg) {
                profconf.remove_key("profile-packages", &pkg);
                profconf.pretty_write(&cfile, &write_options)?;
            } else if is_system_pkg(&profconf, pkg.to_string()) {
                profconf.remove_key("system-packages", &pkg);
                profconf.pretty_write(&cfile, &write_options)?;
            }
        } else if is_system_locked() && is_system_pkg(&profconf, pkg.to_string()){
            return Err(Error::new(ErrorKind::Unsupported,
                                  "Remove system package(s) is not allowed."));
        }
    }
    Ok(())
}

// Uninstall package(s) atomic-operation
pub fn uninstall_package_helper_chroot(snapshot: &str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    for pkg in pkgs {
        let uninstall_args = if noconfirm {
            ["apt-get", "remove", "-y"]
        } else {
            ["apt-get", "remove", ""]
        };

        let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
                                           .args(uninstall_args)
                                           .arg(format!("{}", pkg)).status()?;

        if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to uninstall {}.", pkg)));
        }
    }
    Ok(())
}

// Uninstall package(s) atomic-operation live snapshot
pub fn uninstall_package_helper_live(tmp: &str, pkgs: &Vec<String>, noconfirm: bool) -> Result<(), Error> {
    for pkg in pkgs {
        let uninstall_args = if noconfirm {
            ["apt-get", "remove", "-y"]
        } else {
            ["apt-get", "remove", ""]
        };

        let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-{}", tmp))
                                           .args(uninstall_args)
                                           .arg(format!("{}", pkg)).status()?;

        if !excode.success() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Failed to uninstall {}.", pkg)));
        }
    }
    Ok(())
}

// Upgrade snapshot atomic-operation
pub fn upgrade_helper(snapshot: &str, noconfirm: bool) -> Result<(), Error> {
    // Prepare snapshot
    prepare(snapshot).unwrap();

    let upgrade_args = if noconfirm {
        ["pacman", "--noconfirm", "-Syyu"]
    } else {
        ["pacman", "--confirm", "-Syyu"]
    };

    let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-chr{}", snapshot))
                                       .args(upgrade_args)
                                       .status().unwrap();
    if !excode.success() {
        return Err(Error::new(ErrorKind::Other,
                              format!("Failed to upgrade snapshot {}.", snapshot)));
    }
    Ok(())
}

// Live upgrade snapshot atomic-operation
pub fn upgrade_helper_live(tmp: &str, noconfirm: bool) -> Result<(), Error> {
    let upgrade_args = if noconfirm {
        ["apt-get", "upgrade", "-y"]
    } else {
        ["apt-get", "upgrade", "-y"]
    };

    let excode = Command::new("chroot").arg(format!("/.snapshots/rootfs/snapshot-{}", tmp))
                                       .args(upgrade_args)
                                       .status().unwrap();
    if !excode.success() {
        return Err(Error::new(ErrorKind::Other,
                              "Failed to upgrade current/live snapshot."));
    }
    Ok(())
}
