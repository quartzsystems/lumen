//! Build the ordered install plan from an InstallConfig.
//!
//! The plan is data (commands + file contents), not behavior: it can be
//! printed with --print-plan and unit-tested without touching hardware.

use super::{Action, Step};
use crate::config::{BuildPins, InstallConfig, NetworkConfig};

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

/// The management bridge. `br0` carries the address; the chosen NIC is a port
/// of it. See docs/networking.md — the appliance is bridged from the first
/// boot so the first virtual machine does not require moving the management
/// address, which is the one change that costs a trip to the rack.
pub const MGMT_BRIDGE: &str = "br0";

/// NetworkManager keyfiles for the management connection: the bridge that
/// holds the address, and the port that attaches the NIC to it.
///
/// `mac` is the NIC's hardware address and is pinned onto the bridge. Without
/// the pin a Linux bridge takes the lowest MAC among its ports, so the day a
/// second NIC is added the management MAC changes underneath the DHCP
/// reservation and any switch-side port security — silently, at boot.
pub fn nm_keyfiles(nic: &str, mac: &str, net: &NetworkConfig) -> (String, String) {
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
    // mac-address is omitted rather than written empty when the live
    // environment could not read one: an empty value is a parse error for
    // NetworkManager, and an unpinned bridge still works.
    let mac_line = if mac.is_empty() {
        String::new()
    } else {
        format!("mac-address={mac}\n")
    };

    let bridge = format!(
        "# Written by the Lumen installer.\n\
         [connection]\n\
         id=management\n\
         type=bridge\n\
         interface-name={MGMT_BRIDGE}\n\
         autoconnect=true\n\n\
         [bridge]\n\
         stp=false\n\
         forward-delay=0\n\
         {mac_line}\n\
         {ipv4}\n\
         [ipv6]\n\
         method=disabled\n"
    );
    // controller=/port-type=, not the deprecated master=/slave-type=: EL10's
    // NetworkManager is well past 1.46. One spelling everywhere — the control
    // plane writes the same one over D-Bus.
    let port = format!(
        "# Written by the Lumen installer.\n\
         [connection]\n\
         id=management-port\n\
         type=ethernet\n\
         interface-name={nic}\n\
         controller={MGMT_BRIDGE}\n\
         port-type=bridge\n\
         autoconnect=true\n"
    );
    (bridge, port)
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
    let (mgmt_bridge_keyfile, mgmt_port_keyfile) =
        nm_keyfiles(&cfg.nic, &cfg.nic_mac, &cfg.network);

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
        actions: vec![
            // kernel-core %posttrans runs kernel-install, which derives its
            // entry token from /etc/machine-id and quietly writes no BLS
            // entry when the id is missing — and dnf --installroot never
            // creates one. Seed it first so the kernel package can register
            // /boot/loader/entries/<machine-id>-<kver>.conf.
            sh(format!(
                "mkdir -p {TARGET}/etc && systemd-machine-id-setup --root={TARGET}"
            )),
            sh(format!(
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
             policycoreutils selinux-policy-targeted \
             lumen-release lumen-networking lumen-storage lumen-compute \
             lumen-controlplane"
            )),
        ],
    });

    steps.push(Step {
        title: "Configure system".into(),
        actions: vec![
            // hostid + zpool.cache coherence: dracut's zfs module needs both
            // to import rpool without force at first boot.
            sh(format!("cp /etc/hostid {TARGET}/etc/hostid")),
            // The root pool's installation media library, made here so a fresh
            // node can be given an image from the console without anyone
            // touching the box first. Created now rather than on demand
            // because the control plane cannot see a mount that appears after
            // its own namespace was set up — the dataset has to exist before
            // it starts, and at install time it always can.
            sh("zfs create -p -o mountpoint=/var/lib/lumen/iso/rpool rpool/lumen/iso"),
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
                contents: mgmt_bridge_keyfile.clone(),
                mode: 0o600,
            },
            Action::WriteFile {
                path: format!(
                    "{TARGET}/etc/NetworkManager/system-connections/management-port.nmconnection"
                ),
                contents: mgmt_port_keyfile.clone(),
                mode: 0o600,
            },
            // Same NIC names on the installed system as in the live env:
            // run from the live env (which sees /sys) against the target.
            sh(format!("/usr/sbin/lumen-nicnames --root {TARGET}")),
            sh(format!(
                "systemctl --root={TARGET} enable NetworkManager chronyd sshd firewalld \
                 lumen-controlplane lumen-console-banner"
            )),
            // Open the management console port (8443). firewalld isn't
            // running in the target, so edit its permanent config offline;
            // the service definition ships in lumen-controlplane.
            sh(format!(
                "chroot {TARGET} firewall-offline-cmd --add-service=lumen-controlplane"
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
            // kernel-install ran during the package step in a degraded
            // environment (no /proc in the target), so its output cannot be
            // trusted: entry paths come out /boot/-prefixed (blscfg resolves
            // them against the /boot fs root), and on some hosts it declines
            // to write an entry — or copy the kernel image — at all, leaving
            // an unbootable target. Write the BLS entry ourselves when it is
            // missing, then normalize the paths either way.
            sh(format!(
                "kver=$(chroot {TARGET} rpm -q --qf '%{{VERSION}}-%{{RELEASE}}.%{{ARCH}}\\n' kernel-core | head -n1) && \
                 mkdir -p {TARGET}/boot/loader/entries && \
                 if ! ls {TARGET}/boot/loader/entries/*.conf >/dev/null 2>&1; then \
                     [ -e {TARGET}/boot/vmlinuz-\"$kver\" ] || \
                         cp {TARGET}/usr/lib/modules/\"$kver\"/vmlinuz {TARGET}/boot/vmlinuz-\"$kver\"; \
                     token=$(cat {TARGET}/etc/machine-id 2>/dev/null); \
                     [ -n \"$token\" ] || token=lumen; \
                     {{ printf 'title Lumen (%s)\\n' \"$kver\"; \
                        printf 'version %s\\n' \"$kver\"; \
                        printf 'linux /vmlinuz-%s\\n' \"$kver\"; \
                        printf 'initrd /initramfs-%s.img\\n' \"$kver\"; \
                        printf 'options {root_arg}\\n'; \
                        printf 'grub_users $grub_users\\n'; \
                        printf 'grub_arg --unrestricted\\n'; \
                        printf 'grub_class kernel\\n'; \
                     }} > {TARGET}/boot/loader/entries/\"$token\"-\"$kver\".conf; \
                 fi && \
                 sed -i -e 's|^linux /boot/|linux /|' -e 's|^initrd /boot/|initrd /|' \
                     {TARGET}/boot/loader/entries/*.conf"
            )),
            // Same branded menu as the install media (see build-live-iso.sh):
            // gfxmenu/png are compiled into the signed EL GRUB, the font is
            // not, so theme + font are staged on /boot where $root points.
            // lumen-release ships the theme; unicode.pf2 comes from
            // grub2-common — both are already installed in the target.
            sh(format!(
                "mkdir -p {TARGET}/boot/grub2/fonts {TARGET}/boot/grub2/themes/lumen && \
                 cp {TARGET}/usr/share/grub/unicode.pf2 {TARGET}/boot/grub2/fonts/unicode.pf2 && \
                 cp {TARGET}/usr/share/lumen-release/grub/theme.txt \
                    {TARGET}/usr/share/lumen-release/grub/lumen-grub-bg.png \
                    {TARGET}/boot/grub2/themes/lumen/"
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
                        'if loadfont /grub2/fonts/unicode.pf2; then' \
                        '    set gfxmode=1024x768,auto' \
                        '    terminal_output gfxterm' \
                        '    set color_normal=light-gray/black' \
                        '    set color_highlight=black/green' \
                        '    set theme=/grub2/themes/lumen/theme.txt' \
                        'fi' \
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
        ],
    });

    steps.push(Step {
        title: "Label filesystem for SELinux".into(),
        actions: vec![
            // The live env runs selinux=0, so the target boots with a
            // completely unlabeled root — and in enforcing mode an
            // unlabeled /usr/lib/systemd/systemd leaves init in a domain
            // that is denied everything ("Failed to allocate manager
            // object"), long before a .autorelabel pass could run. Label
            // at install time instead: with no policy loaded in the live
            // kernel, setfiles must validate against the target's binary
            // policy (-c) and the labels land as raw security.selinux
            // xattrs (zfs xattr=sa stores them inline). The EL10 policy
            // has fs_use_xattr for zfs, so the labels are honored at boot.
            // /proc, /sys, /dev are live bind mounts; the ESP is vfat and
            // cannot hold xattrs.
            sh(format!(
                "chroot {TARGET} sh -c 'pol=$(ls /etc/selinux/targeted/policy/policy.* | tail -n1) && \
                 setfiles -F -e /proc -e /sys -e /dev -e /boot/efi \
                     -c \"$pol\" /etc/selinux/targeted/contexts/files/file_contexts /'"
            )),
            // Hard gate: an unlabeled init binary is exactly the
            // unbootable case, so fail the install if labels did not stick.
            sh(format!(
                "chroot {TARGET} ls -Z /usr/lib/systemd/systemd | grep -q init_exec_t"
            )),
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
    use crate::config::PoolTopology;

    fn test_cfg() -> InstallConfig {
        InstallConfig {
            root_password_hash: "$6$s$h".into(),
            timezone: "America/New_York".into(),
            keymap: "us".into(),
            hostname: "lumen01.example.lan".into(),
            nic: "nic0".into(),
            nic_mac: "52:54:00:aa:bb:00".into(),
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
        let tail: Vec<&str> = argv
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(String::as_str)
            .collect();
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
        assert!(mkfs
            .iter()
            .all(|argv| argv.last().unwrap().starts_with("/dev/sda")));
    }

    #[test]
    fn raidz1_pool_uses_raidz_keyword() {
        let mut cfg = test_cfg();
        cfg.disks = vec!["/dev/sda".into(), "/dev/sdb".into(), "/dev/sdc".into()];
        cfg.topology = PoolTopology::Raidz1;
        let argv = find_zpool_create(&build_plan(&cfg, &BuildPins::default()));
        let tail: Vec<&str> = argv
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(String::as_str)
            .collect();
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
        let script = plan
            .iter()
            .flat_map(|s| &s.actions)
            .find_map(|a| match a {
                Action::Shell(s) if s.contains("dnf -y --installroot") => Some(s),
                _ => None,
            })
            .expect("plan must contain the dnf install action");
        assert!(script.contains("kernel-6.12.0-211.7.3.el10_2"));
        assert!(script.contains("kmod-zfs"));
        assert!(script.contains("--disablerepo='*'"));
    }

    #[test]
    fn machine_id_is_seeded_before_package_install() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let seed = shells
            .iter()
            .position(|s| s.contains("systemd-machine-id-setup"))
            .expect("plan must seed the target machine-id");
        let dnf = shells
            .iter()
            .position(|s| s.contains("dnf -y --installroot"))
            .expect("plan must contain the dnf install action");
        assert!(seed < dnf, "machine-id must be seeded before dnf runs");
    }

    #[test]
    fn bls_entry_is_written_when_kernel_install_skipped_it() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let script = plan
            .iter()
            .flat_map(|s| &s.actions)
            .find_map(|a| match a {
                Action::Shell(s) if s.contains("/boot/loader/entries") => Some(s),
                _ => None,
            })
            .expect("plan must handle BLS entries");
        // Fallback entry generation, not just a hard existence gate.
        assert!(script.contains("if ! ls"));
        assert!(script.contains("grub_arg --unrestricted"));
        assert!(script.contains("root=zfs:rpool/ROOT/lumen"));
        // Kernel image installed if kernel-install skipped that too.
        assert!(script.contains("/usr/lib/modules/"));
    }

    #[test]
    fn target_is_labeled_at_install_time_not_first_boot() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let setfiles = shells
            .iter()
            .find(|s| s.contains("setfiles"))
            .expect("plan must label the target with setfiles");
        // No policy is loaded in the live kernel (selinux=0): validation
        // must run against the target's binary policy.
        assert!(setfiles.contains("-c"));
        assert!(setfiles.contains("file_contexts"));
        // vfat cannot hold security xattrs.
        assert!(setfiles.contains("-e /boot/efi"));
        // The broken first-boot relabel path must be gone: enforcing boot
        // on an unlabeled root dies before .autorelabel can run.
        assert!(!shells.iter().any(|s| s.contains(".autorelabel")));
        // The label result is verified before the pool is exported.
        assert!(shells.iter().any(|s| s.contains("init_exec_t")));
        // Labeling runs after all target writes: last step before Finalize.
        assert_eq!(plan[plan.len() - 2].title, "Label filesystem for SELinux");
    }

    #[test]
    fn installed_grub_menu_is_branded() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let grub_cfg = shells
            .iter()
            .find(|s| s.contains("> /mnt/sysroot/boot/grub2/grub.cfg"))
            .expect("plan must write the static grub.cfg");
        assert!(grub_cfg.contains("loadfont /grub2/fonts/unicode.pf2"));
        assert!(grub_cfg.contains("theme=/grub2/themes/lumen/theme.txt"));
        assert!(grub_cfg.contains("terminal_output gfxterm"));
        let staged = shells
            .iter()
            .find(|s| s.contains("unicode.pf2") && s.contains("themes/lumen"))
            .expect("plan must stage the theme and font on /boot");
        assert!(staged.contains("lumen-grub-bg.png"));
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
    fn controlplane_is_installed_enabled_and_reachable() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        // In the offline package set…
        assert!(shells
            .iter()
            .any(|s| s.contains("dnf -y --installroot") && s.contains("lumen-controlplane")));
        // …enabled at boot, together with the console banner that surfaces
        // the console's address on the pre-login screen…
        assert!(shells.iter().any(|s| s.contains("systemctl --root")
            && s.contains("lumen-controlplane")
            && s.contains("lumen-console-banner")));
        // …and reachable: the console port must be opened in firewalld's
        // permanent config, after the package that ships the service
        // definition is installed.
        let dnf = shells
            .iter()
            .position(|s| s.contains("dnf -y --installroot"))
            .expect("plan must install packages");
        let fw = shells
            .iter()
            .position(|s| s.contains("firewall-offline-cmd --add-service=lumen-controlplane"))
            .expect("plan must open the console port");
        assert!(dnf < fw, "firewall edit needs the installed service file");
    }

    /// The management daemon speaks to the hypervisor and to the pool tooling
    /// through packages that are not its own dependencies — they are appliance
    /// policy, and they carry the presets that start those services. Leaving
    /// them out of the install set is invisible until a fresh node opens the
    /// Virtual Machines page and finds no hypervisor socket to connect to.
    #[test]
    fn integration_packages_are_installed() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let dnf = plan
            .iter()
            .flat_map(|s| &s.actions)
            .find_map(|a| match a {
                Action::Shell(s) if s.contains("dnf -y --installroot") => Some(s),
                _ => None,
            })
            .expect("plan must contain the dnf install action");
        for package in ["lumen-networking", "lumen-storage", "lumen-compute"] {
            assert!(dnf.contains(package), "install set must contain {package}");
        }
    }

    /// The media library has to exist before the control plane's first start:
    /// a mount made later is not visible inside its namespace, so a node
    /// installed without one cannot be given an image from the console until
    /// somebody restarts the daemon.
    #[test]
    fn the_media_library_is_made_at_install_time() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let shells: Vec<&str> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::Shell(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let iso = shells
            .iter()
            .position(|s| s.contains("rpool/lumen/iso"))
            .expect("plan must create the media library");
        assert!(
            shells[iso].contains("mountpoint=/var/lib/lumen/iso/rpool"),
            "it must mount where the unit's ReadWritePaths names: {}",
            shells[iso]
        );
    }

    #[test]
    fn nm_keyfile_dhcp() {
        let (bridge, port) = nm_keyfiles("nic0", "52:54:00:aa:bb:00", &NetworkConfig::Dhcp);
        // The address lives on the bridge, not on the NIC.
        assert!(bridge.contains("type=bridge"));
        assert!(bridge.contains("interface-name=br0"));
        assert!(bridge.contains("method=auto"));
        assert!(!bridge.contains("address1"));
        assert!(!bridge.contains("interface-name=nic0"));
        // …and the NIC is a port of it, with no addressing of its own.
        assert!(port.contains("type=ethernet"));
        assert!(port.contains("interface-name=nic0"));
        assert!(port.contains("controller=br0"));
        assert!(port.contains("port-type=bridge"));
        assert!(!port.contains("[ipv4]"));
    }

    #[test]
    fn nm_keyfile_static() {
        let net = NetworkConfig::Static {
            cidr: "10.0.0.5/24".into(),
            gateway: "10.0.0.1".into(),
            dns: vec!["9.9.9.9".into(), "1.1.1.1".into()],
        };
        let (bridge, port) = nm_keyfiles("nic2", "52:54:00:aa:bb:02", &net);
        assert!(bridge.contains("interface-name=br0"));
        assert!(bridge.contains("method=manual"));
        assert!(bridge.contains("address1=10.0.0.5/24,10.0.0.1"));
        assert!(bridge.contains("dns=9.9.9.9;1.1.1.1;"));
        assert!(bridge.contains("[ipv6]\nmethod=disabled"));
        assert!(port.contains("interface-name=nic2"));
    }

    #[test]
    fn management_bridge_pins_the_nic_mac() {
        // Unpinned, a bridge inherits the lowest MAC among its ports, so
        // adding a second NIC later would move the management MAC and break
        // the DHCP reservation.
        let (bridge, _) = nm_keyfiles("nic0", "52:54:00:aa:bb:00", &NetworkConfig::Dhcp);
        assert!(bridge.contains("[bridge]"));
        assert!(bridge.contains("mac-address=52:54:00:aa:bb:00"));
        assert!(bridge.contains("stp=false"));
        assert!(bridge.contains("forward-delay=0"));

        // No MAC to pin is not a reason to write an empty value NM rejects.
        let (bridge, _) = nm_keyfiles("nic0", "", &NetworkConfig::Dhcp);
        assert!(!bridge.contains("mac-address"));
    }

    #[test]
    fn the_deprecated_port_property_spelling_is_not_written() {
        // One spelling across the appliance: the control plane writes
        // controller=/port-type= over D-Bus, so the installer must too.
        let (_, port) = nm_keyfiles("nic0", "52:54:00:aa:bb:00", &NetworkConfig::Dhcp);
        assert!(!port.contains("master="));
        assert!(!port.contains("slave-type="));
    }

    #[test]
    fn both_management_keyfiles_reach_the_target_root_only() {
        let plan = build_plan(&test_cfg(), &BuildPins::default());
        let keyfiles: Vec<(&str, &str)> = plan
            .iter()
            .flat_map(|s| &s.actions)
            .filter_map(|a| match a {
                Action::WriteFile {
                    path,
                    contents,
                    mode,
                } if path.ends_with(".nmconnection") => {
                    assert_eq!(*mode, 0o600, "nmconnection must be written with mode 0600");
                    Some((path.as_str(), contents.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(keyfiles.len(), 2, "bridge plus port: {keyfiles:?}");
        let bridge = keyfiles
            .iter()
            .find(|(p, _)| p.ends_with("/management.nmconnection"))
            .expect("plan must write the management bridge");
        assert!(bridge.1.contains("type=bridge"));
        let port = keyfiles
            .iter()
            .find(|(p, _)| p.ends_with("/management-port.nmconnection"))
            .expect("plan must write the management port");
        assert!(port.1.contains("controller=br0"));
        assert!(port.1.contains("interface-name=nic0"));
    }
}
