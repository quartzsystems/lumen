//! Build the ordered install plan from an InstallConfig.
//!
//! The plan is data (commands + file contents), not behavior: it can be
//! printed with --print-plan and unit-tested without touching hardware.

use super::{Action, Step};
use crate::config::{BuildPins, InstallConfig, NetworkConfig, PoolTopology};

/// Target root while installing.
pub const TARGET: &str = "/mnt/sysroot";
/// Install media mount (dmsquash-live convention).
pub const MEDIA: &str = "/run/initramfs/live";

/// Partition device path: /dev/sda -> /dev/sda2, /dev/nvme0n1 -> /dev/nvme0n1p2.
pub fn part_dev(disk: &str, n: u32) -> String {
    if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

fn cmd(argv: &[&str]) -> Action {
    Action::Cmd(argv.iter().map(|s| s.to_string()).collect())
}

fn sh(script: impl Into<String>) -> Action {
    Action::Shell(script.into())
}

/// NetworkManager keyfile for the management connection.
pub fn nm_keyfile(nic: &str, net: &NetworkConfig) -> String {
    let ipv4 = match net {
        NetworkConfig::Dhcp => "[ipv4]\nmethod=auto\n".to_string(),
        NetworkConfig::Static { cidr, gateway, dns } => {
            let dns_line = if dns.is_empty() {
                String::new()
            } else {
                format!("dns={};\n", dns.join(";"))
            };
            format!("[ipv4]\nmethod=manual\naddress1={cidr},{gateway}\n{dns_line}")
        }
    };
    format!(
        "# Written by the Lumen installer.\n\
         [connection]\n\
         id=management\n\
         type=ethernet\n\
         interface-name={nic}\n\
         autoconnect=true\n\n\
         {ipv4}\n\
         [ipv6]\n\
         method=disabled\n"
    )
}

pub fn build_plan(cfg: &InstallConfig, pins: &BuildPins) -> Vec<Step> {
    // Every disk gets the same ESP/boot/rpool layout so any of them can be
    // promoted to the boot disk later, but only the first disk's ESP and
    // /boot are formatted and used.
    let boot_disk = cfg.disks[0].as_str();
    let esp = part_dev(boot_disk, 1);
    let boot = part_dev(boot_disk, 2);
    let kernel_pkg = pins.kernel_nevr.clone().unwrap_or_else(|| "kernel".into());
    let root_arg = "root=zfs:rpool/ROOT/lumen";

    let mut steps: Vec<Step> = Vec::new();

    steps.push(Step {
        title: "Preflight".into(),
        actions: vec![
            // Pool hostid must match the initramfs of the installed system;
            // generate ours first and copy it into the target later.
            cmd(&["zgenhostid", "-f"]),
            cmd(&["udevadm", "settle"]),
        ],
    });

    let mut partition_actions: Vec<Action> = Vec::new();
    for disk in &cfg.disks {
        partition_actions.push(sh(format!("wipefs -a {disk} || true")));
        partition_actions.push(cmd(&["sgdisk", "--zap-all", disk]));
        partition_actions.push(cmd(&[
            "sgdisk",
            "-n1:1M:+1G",
            "-t1:EF00",
            "-c1:EFI",
            "-n2:0:+2G",
            "-t2:8300",
            "-c2:boot",
            "-n3:0:0",
            "-t3:BF00",
            "-c3:rpool",
            disk,
        ]));
        partition_actions.push(cmd(&["partprobe", disk]));
    }
    partition_actions.push(cmd(&["udevadm", "settle"]));
    steps.push(Step {
        title: if cfg.disks.len() == 1 {
            "Partition target disk".into()
        } else {
            format!("Partition {} target disks", cfg.disks.len())
        },
        actions: partition_actions,
    });

    let mut zpool_create: Vec<String> = [
        "zpool",
        "create",
        "-f",
        "-o",
        "ashift=12",
        "-o",
        "autotrim=on",
        "-O",
        "compression=lz4",
        "-O",
        "acltype=posixacl",
        "-O",
        "xattr=sa",
        "-O",
        "dnodesize=auto",
        "-O",
        "relatime=on",
        "-O",
        "canmount=off",
        "-O",
        "mountpoint=none",
        "-R",
        TARGET,
        "rpool",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(keyword) = cfg.topology.vdev_keyword() {
        zpool_create.push(keyword.into());
    }
    zpool_create.extend(cfg.disks.iter().map(|d| part_dev(d, 3)));

    steps.push(Step {
        title: "Create filesystems and pool".into(),
        actions: vec![
            cmd(&["mkfs.vfat", "-F32", "-n", "EFI", &esp]),
            cmd(&["mkfs.ext4", "-F", "-L", "boot", &boot]),
            Action::Cmd(zpool_create),
            cmd(&[
                "zfs",
                "create",
                "-o",
                "canmount=off",
                "-o",
                "mountpoint=none",
                "rpool/ROOT",
            ]),
            cmd(&[
                "zfs",
                "create",
                "-o",
                "canmount=noauto",
                "-o",
                "mountpoint=/",
                "rpool/ROOT/lumen",
            ]),
            cmd(&["zpool", "set", "bootfs=rpool/ROOT/lumen", "rpool"]),
            cmd(&["zfs", "mount", "rpool/ROOT/lumen"]),
            sh(format!(
                "mkdir -p {TARGET}/boot && mount {boot} {TARGET}/boot"
            )),
            sh(format!(
                "mkdir -p {TARGET}/boot/efi && mount {esp} {TARGET}/boot/efi"
            )),
        ],
    });

    steps.push(Step {
        title: "Install packages (offline, from media)".into(),
        actions: vec![sh(format!(
            "dnf -y --installroot={TARGET} --releasever=10 \
             --disablerepo='*' \
             --repofrompath=media,{MEDIA}/Minimal \
             --repofrompath=lumen,{MEDIA}/lumen \
             --setopt=media.gpgcheck=0 --setopt=lumen.gpgcheck=0 \
             --setopt=install_weak_deps=False \
             install @core {kernel_pkg} \
             zfs kmod-zfs zfs-dracut \
             grub2-efi-x64 shim-x64 grub2-tools grubby efibootmgr \
             e2fsprogs dosfstools \
             NetworkManager chrony firewalld openssh-server \
             lumen-release lumen-networking"
        ))],
    });

    steps.push(Step {
        title: "Configure system".into(),
        actions: vec![
            // hostid + zpool.cache coherence: dracut's zfs module needs both
            // to import rpool without force at first boot.
            sh(format!("cp /etc/hostid {TARGET}/etc/hostid")),
            sh(format!(
                "mkdir -p {TARGET}/etc/zfs && \
                 cp /etc/zfs/zpool.cache {TARGET}/etc/zfs/zpool.cache 2>/dev/null || true"
            )),
            sh(format!(
                "{{ printf 'UUID=%s /boot ext4 defaults 0 2\\n' \
                       \"$(blkid -s UUID -o value {boot})\"; \
                    printf 'UUID=%s /boot/efi vfat umask=0077,shortname=winnt 0 2\\n' \
                       \"$(blkid -s UUID -o value {esp})\"; \
                 }} > {TARGET}/etc/fstab"
            )),
            sh(format!(
                "echo 'root:{}' | chroot {TARGET} chpasswd -e",
                cfg.root_password_hash
            )),
            cmd(&[
                "ln",
                "-sfn",
                &format!("../usr/share/zoneinfo/{}", cfg.timezone),
                &format!("{TARGET}/etc/localtime"),
            ]),
            Action::WriteFile {
                path: format!("{TARGET}/etc/hostname"),
                contents: format!("{}\n", cfg.hostname),
                mode: 0o644,
            },
            Action::WriteFile {
                path: format!("{TARGET}/etc/vconsole.conf"),
                contents: format!("KEYMAP={}\n", cfg.keymap),
                mode: 0o644,
            },
            Action::WriteFile {
                path: format!(
                    "{TARGET}/etc/NetworkManager/system-connections/management.nmconnection"
                ),
                contents: nm_keyfile(&cfg.nic, &cfg.network),
                mode: 0o600,
            },
            // Same NIC names on the installed system as in the live env:
            // run from the live env (which sees /sys) against the target.
            sh(format!("/usr/sbin/lumen-nicnames --root {TARGET}")),
            sh(format!(
                "systemctl --root={TARGET} enable NetworkManager chronyd sshd firewalld"
            )),
        ],
    });

    steps.push(Step {
        title: "Boot configuration".into(),
        actions: vec![
            Action::WriteFile {
                path: format!("{TARGET}/etc/dracut.conf.d/zfs.conf"),
                // Force the zfs dracut module for every future kernel update,
                // not just the initramfs we build here.
                contents: "add_dracutmodules+=\" zfs \"\n".into(),
                mode: 0o644,
            },
            Action::WriteFile {
                path: format!("{TARGET}/etc/kernel/cmdline"),
                contents: format!("{root_arg}\n"),
                mode: 0o644,
            },
            sh(format!(
                "for d in dev proc sys; do mount --bind /$d {TARGET}/$d; done && \
                 mount --bind /sys/firmware/efi/efivars {TARGET}/sys/firmware/efi/efivars"
            )),
            // /lib/modules holds a second directory for the kernel the kABI
            // kmod was built against, so the running kernel version must
            // come from the kernel-core RPM, not a directory listing. The
            // explicit in-chroot depmod indexes the weak-updates symlinks
            // (absolute paths — only resolvable inside the chroot) before
            // dracut needs the zfs module.
            sh(format!(
                "kver=$(chroot {TARGET} rpm -q --qf '%{{VERSION}}-%{{RELEASE}}.%{{ARCH}}\\n' kernel-core | head -n1) && \
                 chroot {TARGET} depmod -a \"$kver\" && \
                 chroot {TARGET} dracut --force --add zfs /boot/initramfs-\"$kver\".img \"$kver\""
            )),
            // grub2-mkconfig cannot run here: the EL10 GRUB userland has no
            // ZFS support, so grub2-probe dies on the ZFS root and mkconfig
            // writes nothing — GRUB then boots to an empty menu (only the
            // firmware-settings entry). The menu is BLS-driven anyway, so
            // write a static config that loads the BLS entries from /boot.
            //
            // kernel-install ran during the package step with no /proc in
            // the target, so it could not see that /boot is its own
            // filesystem: entry paths come out /boot/-prefixed, but blscfg
            // resolves them against the /boot fs root. Normalize them (the
            // ls doubles as a gate that entries exist at all).
            sh(format!(
                "ls {TARGET}/boot/loader/entries/*.conf >/dev/null && \
                 sed -i -e 's|^linux /boot/|linux /|' -e 's|^initrd /boot/|initrd /|' \
                     {TARGET}/boot/loader/entries/*.conf"
            )),
            sh(format!(
                "boot_uuid=$(blkid -s UUID -o value {boot}) && \
                 {{ printf '%s\\n' \
                        'set timeout=5' \
                        'set default=0' \
                        'function load_video {{' \
                        '    insmod efi_gop' \
                        '    insmod efi_uga' \
                        '    insmod all_video' \
                        '}}' \
                        'insmod part_gpt' \
                        'insmod ext2'; \
                    printf 'search --no-floppy --fs-uuid --set=root %s\\n' \"$boot_uuid\"; \
                    printf '%s\\n' \
                        'set boot=$root' \
                        'insmod blscfg' \
                        'blscfg' \
                        \"menuentry 'UEFI Firmware Settings' --id uefi-firmware {{\" \
                        '    fwsetup' \
                        '}}'; \
                 }} > {TARGET}/boot/grub2/grub.cfg"
            )),
            // The signed grubx64.efi has prefix /EFI/almalinux baked in and
            // loads $prefix/grub.cfg from the ESP. Anaconda normally writes
            // that stub; dnf --installroot does not — without it GRUB drops
            // to the shell. The stub chains to the real config on /boot.
            sh(format!(
                "boot_uuid=$(blkid -s UUID -o value {boot}) && \
                 {{ printf 'search --no-floppy --fs-uuid --set=dev %s\\n' \"$boot_uuid\"; \
                    printf 'set prefix=($dev)/grub2\\n'; \
                    printf 'export $prefix\\n'; \
                    printf 'configfile $prefix/grub.cfg\\n'; \
                 }} > {TARGET}/boot/efi/EFI/almalinux/grub.cfg"
            )),
            sh(format!(
                "chroot {TARGET} grubby --update-kernel=ALL --args='{root_arg}'"
            )),
            sh(format!(
                "efibootmgr -c -d {boot_disk} -p 1 -L Lumen -l '\\EFI\\almalinux\\shimx64.efi'"
            )),
            // Live env runs selinux=0, so nothing is labeled: relabel on
            // first boot (zfs xattr=sa carries security.selinux fine).
            sh(format!("touch {TARGET}/.autorelabel")),
        ],
    });

    steps.push(Step {
        title: "Finalize".into(),
        actions: vec![
            sh(format!(
                "umount {TARGET}/sys/firmware/efi/efivars {TARGET}/dev {TARGET}/proc {TARGET}/sys"
            )),
            sh(format!("umount {TARGET}/boot/efi {TARGET}/boot")),
            // A cleanly exported pool imports without force in the target's
            // initramfs; skipping this forces zpool import -f forever after.
            cmd(&["zpool", "export", "rpool"]),
        ],
    });

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> InstallConfig {
        InstallConfig {
            root_password_hash: "$6$s$h".into(),
            timezone: "America/New_York".into(),
            keymap: "us".into(),
            hostname: "lumen01.example.lan".into(),
            nic: "nic0".into(),
            network: NetworkConfig::Dhcp,
            disks: vec!["/dev/sda".into()],
            topology: PoolTopology::Single,
        }
    }

    fn find_zpool_create(plan: &[Step]) -> Vec<String> {
        plan.iter()
            .flat_map(|s| &s.actions)
            .find_map(|a| match a {
                Action::Cmd(argv) if argv.starts_with(&["zpool".into(), "create".into()]) => {
                    Some(argv.clone())
                }
                _ => None,
            })
            .expect("plan must contain zpool create")
    }

    #[test]
    fn hostname_and_keymap_reach_the_target() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let files: Vec<(&str, &str)> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::WriteFile { path, contents, .. } => {
                    Some((path.as_str(), contents.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(files
            .iter()
            .any(|(p, c)| p.ends_with("/etc/hostname") && c.trim() == "lumen01.example.lan"));
        assert!(files
            .iter()
            .any(|(p, c)| p.ends_with("/etc/vconsole.conf") && c.trim() == "KEYMAP=us"));
    }

    #[test]
    fn single_disk_pool_has_plain_vdev() {
        let argv = find_zpool_create(&build_plan(&test_cfg(), &BuildPins::default()));
        assert_eq!(argv.last().unwrap(), "/dev/sda3");
        assert!(!argv.iter().any(|a| a == "mirror" || a.starts_with("raidz")));
    }

    #[test]
    fn mirror_pool_lists_keyword_then_partitions() {
        let mut cfg = test_cfg();
        cfg.disks = vec!["/dev/sda".into(), "/dev/nvme0n1".into()];
        cfg.topology = PoolTopology::Mirror;
        let plan = build_plan(&cfg, &BuildPins::default());
        let argv = find_zpool_create(&plan);
        let tail: Vec<&str> = argv.iter().rev().take(3).rev().map(String::as_str).collect();
        assert_eq!(tail, ["mirror", "/dev/sda3", "/dev/nvme0n1p3"]);
        // Both disks are partitioned; boot filesystems only on the first.
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(shells.iter().any(|s| s.contains("wipefs -a /dev/nvme0n1")));
        let mkfs: Vec<&Vec<String>> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Cmd(argv) if argv[0].starts_with("mkfs.") => Some(argv),
                _ => None,
            })
            .collect();
        assert_eq!(mkfs.len(), 2);
        assert!(mkfs.iter().all(|argv| argv.last().unwrap().starts_with("/dev/sda")));
    }

    #[test]
    fn raidz1_pool_uses_raidz_keyword() {
        let mut cfg = test_cfg();
        cfg.disks = vec!["/dev/sda".into(), "/dev/sdb".into(), "/dev/sdc".into()];
        cfg.topology = PoolTopology::Raidz1;
        let argv = find_zpool_create(&build_plan(&cfg, &BuildPins::default()));
        let tail: Vec<&str> = argv.iter().rev().take(4).rev().map(String::as_str).collect();
        assert_eq!(tail, ["raidz1", "/dev/sda3", "/dev/sdb3", "/dev/sdc3"]);
    }

    #[test]
    fn partition_suffix_rules() {
        assert_eq!(part_dev("/dev/sda", 3), "/dev/sda3");
        assert_eq!(part_dev("/dev/nvme0n1", 1), "/dev/nvme0n1p1");
        assert_eq!(part_dev("/dev/vda", 2), "/dev/vda2");
        assert_eq!(part_dev("/dev/mmcblk0", 3), "/dev/mmcblk0p3");
    }

    #[test]
    fn plan_ends_with_pool_export() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let last = plan.last().unwrap();
        match last.actions.last().unwrap() {
            Action::Cmd(argv) => assert_eq!(argv[..2], ["zpool".to_string(), "export".to_string()]),
            other => panic!("expected zpool export last, got {other:?}"),
        }
    }

    #[test]
    fn plan_pins_kernel_when_available() {
        let pins = BuildPins {
            kernel_nevr: Some("kernel-6.12.0-211.7.3.el10_2".into()),
            lumen_version: "0.1.0".into(),
        };
        let plan = build_plan(&test_cfg(), &pins);
        let dnf_step = &plan[3];
        let Action::Shell(script) = &dnf_step.actions[0] else {
            panic!("expected shell dnf action");
        };
        assert!(script.contains("kernel-6.12.0-211.7.3.el10_2"));
        assert!(script.contains("kmod-zfs"));
        assert!(script.contains("--disablerepo='*'"));
    }

    #[test]
    fn esp_stub_chains_to_boot_grub2() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let found = plan.iter().flat_map(|s| &s.actions).any(|a| {
            matches!(
                a,
                Action::Shell(s)
                    if s.contains("/boot/efi/EFI/almalinux/grub.cfg")
                    && s.contains("configfile")
            )
        });
        assert!(found, "plan must write the ESP grub.cfg stub");
    }

    #[test]
    fn nm_keyfile_dhcp() {
        let text = nm_keyfile("nic0", &NetworkConfig::Dhcp);
        assert!(text.contains("interface-name=nic0"));
        assert!(text.contains("method=auto"));
        assert!(!text.contains("address1"));
    }

    #[test]
    fn nm_keyfile_static() {
        let net = NetworkConfig::Static {
            cidr: "10.0.0.5/24".into(),
            gateway: "10.0.0.1".into(),
            dns: vec!["9.9.9.9".into(), "1.1.1.1".into()],
        };
        let text = nm_keyfile("nic2", &net);
        assert!(text.contains("interface-name=nic2"));
        assert!(text.contains("method=manual"));
        assert!(text.contains("address1=10.0.0.5/24,10.0.0.1"));
        assert!(text.contains("dns=9.9.9.9;1.1.1.1;"));
    }

    #[test]
    fn nmconnection_is_root_only() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let found = plan.iter().flat_map(|s| &s.actions).any(|a| {
            matches!(
                a,
                Action::WriteFile { path, mode, .. }
                    if path.ends_with(".nmconnection") && *mode == 0o600
            )
        });
        assert!(found, "nmconnection must be written with mode 0600");
    }
}
