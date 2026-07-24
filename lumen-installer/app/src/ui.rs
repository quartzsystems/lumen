//! GTK4 wizard: welcome -> root password -> time zone -> network -> disk
//! -> summary -> progress -> done. Linear flow over a GtkStack; state
//! collected into `Draft`, converted to an InstallConfig on confirm.

use crate::config::{BuildPins, InstallConfig, NetworkConfig};
use crate::engine;
use crate::sysinfo;

use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::str::FromStr;

const PAGES: [&str; 8] = [
    "welcome", "password", "timezone", "network", "disk", "summary", "progress", "done",
];

#[derive(Default)]
struct Draft {
    password: String,
    timezone: String,
    nic: String,
    dhcp: bool,
    cidr: String,
    gateway: String,
    dns: Vec<String>,
    disk: String,
    disk_label: String,
}

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("theme.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn build(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Lumen Installer")
        .default_width(1024)
        .default_height(700)
        .build();

    let draft = Rc::new(RefCell::new(Draft {
        dhcp: true,
        timezone: "UTC".into(),
        ..Default::default()
    }));

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    stack.add_named(&page_welcome(&stack), Some(PAGES[0]));
    stack.add_named(&page_password(&stack, &draft), Some(PAGES[1]));
    stack.add_named(&page_timezone(&stack, &draft), Some(PAGES[2]));
    stack.add_named(&page_network(&stack, &draft), Some(PAGES[3]));
    stack.add_named(&page_disk(&stack, &draft), Some(PAGES[4]));
    // summary/progress/done need the window for dialogs.
    stack.add_named(&page_summary(&stack, &draft, &window), Some(PAGES[5]));

    let progress = ProgressPage::new();
    stack.add_named(&progress.root, Some(PAGES[6]));
    stack.add_named(&page_done(), Some(PAGES[7]));

    PROGRESS.with(|slot| *slot.borrow_mut() = Some(progress));

    window.set_child(Some(&stack));
    window.fullscreen();
    window.present();
}

thread_local! {
    // The progress page is created once and driven from the summary page's
    // confirm handler; a thread-local slot avoids threading it through
    // every page constructor.
    static PROGRESS: RefCell<Option<ProgressPage>> = const { RefCell::new(None) };
}

// --- layout helpers ---------------------------------------------------------

fn shell(step: &str, title: &str, subtitle: &str) -> (gtk::Box, gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_top(48);
    outer.set_margin_bottom(32);
    outer.set_margin_start(96);
    outer.set_margin_end(96);

    let step_label = gtk::Label::new(Some(step));
    step_label.add_css_class("qz-step");
    step_label.set_halign(gtk::Align::Start);

    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("qz-title");
    title_label.set_halign(gtk::Align::Start);

    let subtitle_label = gtk::Label::new(Some(subtitle));
    subtitle_label.add_css_class("qz-subtitle");
    subtitle_label.set_halign(gtk::Align::Start);
    subtitle_label.set_wrap(true);

    let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
    header.append(&step_label);
    header.append(&title_label);
    header.append(&subtitle_label);
    header.set_margin_bottom(24);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_vexpand(true);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.set_halign(gtk::Align::End);
    footer.set_margin_top(24);

    outer.append(&header);
    outer.append(&content);
    outer.append(&footer);
    (outer, content, footer)
}

fn nav_button(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn go(stack: &gtk::Stack, page: &str) {
    stack.set_visible_child_name(page);
}

fn list_row(title: &str, subtitle: &str) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title_label = gtk::Label::new(Some(title));
    title_label.set_halign(gtk::Align::Start);
    let subtitle_label = gtk::Label::new(Some(subtitle));
    subtitle_label.set_halign(gtk::Align::Start);
    subtitle_label.add_css_class("qz-hint");
    content.append(&title_label);
    content.append(&subtitle_label);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row
}

// --- pages -------------------------------------------------------------------

fn page_welcome(stack: &gtk::Stack) -> gtk::Box {
    let (outer, content, footer) = shell(
        "QUARTZ SYSTEMS",
        "Lumen",
        &format!(
            "{} — hypervisor appliance installer",
            sysinfo::lumen_version()
        ),
    );

    let mut blocker: Option<&str> = None;
    if !sysinfo::is_uefi() {
        blocker = Some("This machine did not boot via UEFI. Lumen requires UEFI firmware.");
    } else if sysinfo::secure_boot_enabled() {
        blocker = Some(
            "Secure Boot is enabled. The ZFS kernel module is not signed for \
             Secure Boot — disable it in firmware setup and boot this media again.",
        );
    }
    if let Some(message) = blocker {
        let error = gtk::Label::new(Some(message));
        error.add_css_class("qz-error");
        error.set_wrap(true);
        error.set_halign(gtk::Align::Start);
        content.append(&error);
    } else {
        let blurb = gtk::Label::new(Some(
            "This wizard installs Lumen onto a single disk: root password, \
             time zone, management network, and the target drive. The target \
             drive is formatted with ZFS (rpool) and completely erased.",
        ));
        blurb.set_wrap(true);
        blurb.set_halign(gtk::Align::Start);
        content.append(&blurb);
    }

    let reboot = gtk::Button::with_label("Reboot");
    reboot.connect_clicked(|_| {
        let _ = std::process::Command::new("systemctl")
            .arg("reboot")
            .spawn();
    });
    let install = nav_button("Install ▸", true);
    install.set_sensitive(blocker.is_none());
    let stack = stack.clone();
    install.connect_clicked(move |_| go(&stack, "password"));
    footer.append(&reboot);
    footer.append(&install);
    outer
}

fn page_password(stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) -> gtk::Box {
    let (outer, content, footer) = shell(
        "STEP 1 OF 4",
        "Root password",
        "Console and SSH login for the appliance. Minimum 8 characters.",
    );

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    let confirm = gtk::PasswordEntry::new();
    confirm.set_show_peek_icon(true);
    let hint = gtk::Label::new(None);
    hint.add_css_class("qz-hint");
    hint.set_halign(gtk::Align::Start);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.add_css_class("qz-card");
    form.set_halign(gtk::Align::Start);
    form.set_size_request(480, -1);
    let l1 = gtk::Label::new(Some("Password"));
    l1.set_halign(gtk::Align::Start);
    let l2 = gtk::Label::new(Some("Confirm password"));
    l2.set_halign(gtk::Align::Start);
    form.append(&l1);
    form.append(&entry);
    form.append(&l2);
    form.append(&confirm);
    form.append(&hint);
    content.append(&form);

    let back = gtk::Button::with_label("◂ Back");
    let next = nav_button("Next ▸", true);
    next.set_sensitive(false);

    let validate = {
        let entry = entry.clone();
        let confirm = confirm.clone();
        let hint = hint.clone();
        let next = next.clone();
        move || {
            let a = entry.text().to_string();
            let b = confirm.text().to_string();
            let ok = if a.len() < 8 {
                hint.set_text("Too short (minimum 8 characters).");
                false
            } else if a != b {
                hint.set_text("Passwords do not match.");
                false
            } else {
                hint.set_text("OK.");
                true
            };
            next.set_sensitive(ok);
        }
    };
    {
        let v = validate.clone();
        entry.connect_changed(move |_| v());
    }
    confirm.connect_changed(move |_| validate());

    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "welcome"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let entry = entry.clone();
        next.connect_clicked(move |_| {
            draft.borrow_mut().password = entry.text().to_string();
            go(&stack, "timezone");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

fn page_timezone(stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) -> gtk::Box {
    let (outer, content, footer) = shell(
        "STEP 2 OF 4",
        "Time zone",
        "The appliance clock is NTP-synchronized (chrony); pick the local zone.",
    );

    let zones = Rc::new(sysinfo::timezones());
    let mut regions: Vec<String> = Vec::new();
    for (region, _) in zones.iter() {
        if !regions.contains(region) {
            regions.push(region.clone());
        }
    }
    let regions = Rc::new(regions);

    let region_strs: Vec<&str> = regions.iter().map(String::as_str).collect();
    let region_dd = gtk::DropDown::from_strings(&region_strs);
    let zone_dd = gtk::DropDown::from_strings(&["UTC"]);
    // Zone names shown for the currently selected region.
    let current: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(vec!["UTC".into()]));

    let refresh_zones = {
        let zones = zones.clone();
        let regions = regions.clone();
        let zone_dd = zone_dd.clone();
        let current = current.clone();
        move |region_index: u32| {
            let region = &regions[region_index as usize];
            let names: Vec<String> = zones
                .iter()
                .filter(|(r, _)| r == region)
                .map(|(_, tz)| tz.clone())
                .collect();
            let display: Vec<String> = names
                .iter()
                .map(|tz| {
                    tz.split_once('/')
                        .map_or(tz.clone(), |(_, city)| city.replace('_', " "))
                })
                .collect();
            let display_refs: Vec<&str> = display.iter().map(String::as_str).collect();
            zone_dd.set_model(Some(&gtk::StringList::new(&display_refs)));
            zone_dd.set_selected(0);
            *current.borrow_mut() = names;
        }
    };
    refresh_zones(0);
    {
        let refresh_zones = refresh_zones.clone();
        region_dd.connect_selected_notify(move |dd| refresh_zones(dd.selected()));
    }

    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.add_css_class("qz-card");
    form.set_halign(gtk::Align::Start);
    form.set_size_request(480, -1);
    let l1 = gtk::Label::new(Some("Region"));
    l1.set_halign(gtk::Align::Start);
    let l2 = gtk::Label::new(Some("Zone"));
    l2.set_halign(gtk::Align::Start);
    form.append(&l1);
    form.append(&region_dd);
    form.append(&l2);
    form.append(&zone_dd);
    content.append(&form);

    let back = gtk::Button::with_label("◂ Back");
    let next = nav_button("Next ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "password"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let zone_dd = zone_dd.clone();
        let current = current.clone();
        next.connect_clicked(move |_| {
            let names = current.borrow();
            let index = zone_dd.selected() as usize;
            draft.borrow_mut().timezone = names.get(index).cloned().unwrap_or_else(|| "UTC".into());
            go(&stack, "network");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

fn page_network(stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) -> gtk::Box {
    let (outer, content, footer) = shell(
        "STEP 3 OF 4",
        "Management network",
        "Pick the NIC used for appliance management and how it gets an address. \
         NIC names are final: the installed system uses the same nic0…nicN names.",
    );

    let nics: Rc<RefCell<Vec<sysinfo::Nic>>> = Rc::new(RefCell::new(Vec::new()));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);

    let populate = {
        let list = list.clone();
        let nics = nics.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let found = sysinfo::nics();
            for nic in &found {
                let speed = match nic.speed_mbps {
                    Some(mbps) => format!("{mbps} Mb/s"),
                    None => "-".into(),
                };
                let state = if nic.link_up { "link up" } else { "no link" };
                list.append(&list_row(
                    &nic.name,
                    &format!("{} · {state} · {speed}", nic.mac),
                ));
            }
            *nics.borrow_mut() = found;
            list.select_row(list.row_at_index(0).as_ref());
        }
    };
    populate();

    let refresh = gtk::Button::with_label("Rescan");
    {
        let populate = populate.clone();
        refresh.connect_clicked(move |_| populate());
    }

    let dhcp_radio = gtk::CheckButton::with_label("DHCP");
    dhcp_radio.set_active(true);
    let static_radio = gtk::CheckButton::with_label("Static");
    static_radio.set_group(Some(&dhcp_radio));

    let cidr_entry = gtk::Entry::new();
    cidr_entry.set_placeholder_text(Some("address/prefix, e.g. 192.168.10.5/24"));
    let gateway_entry = gtk::Entry::new();
    gateway_entry.set_placeholder_text(Some("gateway, e.g. 192.168.10.1"));
    let dns_entry = gtk::Entry::new();
    dns_entry.set_placeholder_text(Some("DNS servers, comma-separated (optional)"));

    let static_form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    static_form.append(&cidr_entry);
    static_form.append(&gateway_entry);
    static_form.append(&dns_entry);
    static_form.set_sensitive(false);
    {
        let static_form = static_form.clone();
        static_radio.connect_toggled(move |radio| static_form.set_sensitive(radio.is_active()));
    }

    let error = gtk::Label::new(None);
    error.add_css_class("qz-error");
    error.set_halign(gtk::Align::Start);

    let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    mode_box.append(&dhcp_radio);
    mode_box.append(&static_radio);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("qz-card");
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&list));
    scroll.set_min_content_height(180);
    card.append(&scroll);
    card.append(&refresh);
    card.append(&mode_box);
    card.append(&static_form);
    card.append(&error);
    content.append(&card);

    let back = gtk::Button::with_label("◂ Back");
    let next = nav_button("Next ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "timezone"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let list = list.clone();
        let nics = nics.clone();
        let dhcp_radio = dhcp_radio.clone();
        let cidr_entry = cidr_entry.clone();
        let gateway_entry = gateway_entry.clone();
        let dns_entry = dns_entry.clone();
        let error = error.clone();
        next.connect_clicked(move |_| {
            let Some(row) = list.selected_row() else {
                error.set_text("Select a NIC.");
                return;
            };
            let nics = nics.borrow();
            let Some(nic) = nics.get(row.index() as usize) else {
                error.set_text("Select a NIC.");
                return;
            };
            let mut d = draft.borrow_mut();
            d.nic = nic.name.clone();
            d.dhcp = dhcp_radio.is_active();
            if !d.dhcp {
                let cidr = cidr_entry.text().trim().to_string();
                let gateway = gateway_entry.text().trim().to_string();
                if !valid_cidr(&cidr) {
                    error.set_text("Invalid address/prefix (expected a.b.c.d/nn).");
                    return;
                }
                if Ipv4Addr::from_str(&gateway).is_err() {
                    error.set_text("Invalid gateway address.");
                    return;
                }
                let dns: Vec<String> = dns_entry
                    .text()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if dns.iter().any(|s| Ipv4Addr::from_str(s).is_err()) {
                    error.set_text("Invalid DNS server address.");
                    return;
                }
                d.cidr = cidr;
                d.gateway = gateway;
                d.dns = dns;
            }
            drop(d);
            error.set_text("");
            go(&stack, "disk");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

fn page_disk(stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) -> gtk::Box {
    let (outer, content, footer) = shell(
        "STEP 4 OF 4",
        "Boot drive",
        "The selected drive is completely erased: EFI system partition, ext4 \
         /boot, and a ZFS pool (rpool) holding the operating system.",
    );

    let disks: Rc<RefCell<Vec<sysinfo::Disk>>> = Rc::new(RefCell::new(Vec::new()));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);

    let populate = {
        let list = list.clone();
        let disks = disks.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let found = sysinfo::disks();
            for disk in &found {
                let details = format!(
                    "{} · {} · {}{}",
                    sysinfo::human_size(disk.size_bytes),
                    if disk.model.is_empty() {
                        "unknown model"
                    } else {
                        &disk.model
                    },
                    if disk.transport.is_empty() {
                        "?"
                    } else {
                        &disk.transport
                    },
                    if disk.serial.is_empty() {
                        String::new()
                    } else {
                        format!(" · s/n {}", disk.serial)
                    },
                );
                list.append(&list_row(&disk.path, &details));
            }
            *disks.borrow_mut() = found;
            list.select_row(list.row_at_index(0).as_ref());
        }
    };
    populate();

    let refresh = gtk::Button::with_label("Rescan");
    {
        let populate = populate.clone();
        refresh.connect_clicked(move |_| populate());
    }

    let error = gtk::Label::new(None);
    error.add_css_class("qz-error");
    error.set_halign(gtk::Align::Start);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("qz-card");
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&list));
    scroll.set_min_content_height(220);
    card.append(&scroll);
    card.append(&refresh);
    card.append(&error);
    content.append(&card);

    let back = gtk::Button::with_label("◂ Back");
    let next = nav_button("Review ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "network"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let list = list.clone();
        let disks = disks.clone();
        let error = error.clone();
        next.connect_clicked(move |_| {
            let Some(row) = list.selected_row() else {
                error.set_text("Select a target drive.");
                return;
            };
            let disks = disks.borrow();
            let Some(disk) = disks.get(row.index() as usize) else {
                error.set_text("Select a target drive.");
                return;
            };
            {
                let mut d = draft.borrow_mut();
                d.disk = disk.path.clone();
                d.disk_label = format!(
                    "{} ({}, {})",
                    disk.path,
                    sysinfo::human_size(disk.size_bytes),
                    if disk.model.is_empty() {
                        "unknown model"
                    } else {
                        &disk.model
                    }
                );
            }
            // The summary page rebuilds its text when it becomes visible.
            go(&stack, "summary");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

fn summary_text(d: &Draft) -> String {
    let network = if d.dhcp {
        format!("{} — DHCP", d.nic)
    } else {
        format!(
            "{} — static {} via {}{}",
            d.nic,
            d.cidr,
            d.gateway,
            if d.dns.is_empty() {
                String::new()
            } else {
                format!(", DNS {}", d.dns.join(", "))
            }
        )
    };
    format!(
        "Root password   set ({} characters)\n\
         Time zone       {}\n\
         Network         {}\n\
         Boot drive      {}\n\n\
         Layout: 1 GiB EFI + 2 GiB /boot (ext4) + rest ZFS pool \"rpool\"",
        d.password.len(),
        d.timezone,
        network,
        d.disk_label,
    )
}

fn page_summary(
    stack: &gtk::Stack,
    draft: &Rc<RefCell<Draft>>,
    window: &gtk::ApplicationWindow,
) -> gtk::Box {
    let (outer, content, footer) = shell(
        "REVIEW",
        "Ready to install",
        "Nothing has been written yet. Installation erases the selected drive.",
    );

    let label = gtk::Label::new(None);
    label.set_halign(gtk::Align::Start);
    label.add_css_class("qz-card");
    label.set_selectable(false);
    content.append(&label);

    // Keep the label reachable for refresh_summary without custom properties:
    // rebuild the text every time the page becomes visible instead.
    {
        let label = label.clone();
        let draft = draft.clone();
        stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() == Some("summary") {
                label.set_text(&summary_text(&draft.borrow()));
            }
        });
    }

    let back = gtk::Button::with_label("◂ Back");
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "disk"));
    }

    let install = gtk::Button::with_label("Erase disk & install");
    install.add_css_class("destructive-action");
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let window = window.clone();
        install.connect_clicked(move |_| {
            let disk_label = draft.borrow().disk_label.clone();
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message("Erase disk and install Lumen?")
                .detail(format!("ALL DATA on {disk_label} will be destroyed."))
                .buttons(["Cancel", "Erase & Install"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let stack = stack.clone();
            let draft = draft.clone();
            dialog.choose(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
                if result == Ok(1) {
                    start_install(&stack, &draft.borrow());
                }
            });
        });
    }
    footer.append(&back);
    footer.append(&install);
    outer
}

struct ProgressPage {
    root: gtk::Box,
    bar: gtk::ProgressBar,
    status: gtk::Label,
    log: gtk::TextView,
    reboot: gtk::Button,
}

impl ProgressPage {
    fn new() -> Self {
        let (outer, content, footer) = shell(
            "INSTALLING",
            "Installing Lumen",
            "Do not power off the machine.",
        );

        let bar = gtk::ProgressBar::new();
        bar.set_show_text(true);
        let status = gtk::Label::new(Some("Starting…"));
        status.set_halign(gtk::Align::Start);

        let log = gtk::TextView::new();
        log.set_editable(false);
        log.set_monospace(true);
        log.set_cursor_visible(false);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&log));
        scroll.set_vexpand(true);
        scroll.set_min_content_height(260);

        content.append(&bar);
        content.append(&status);
        content.append(&scroll);

        let reboot = gtk::Button::with_label("Reboot");
        reboot.set_visible(false);
        reboot.connect_clicked(|_| {
            let _ = std::process::Command::new("systemctl")
                .arg("reboot")
                .spawn();
        });
        footer.append(&reboot);

        Self {
            root: outer,
            bar,
            status,
            log,
            reboot,
        }
    }

    fn append_log(&self, line: &str) {
        let buffer = self.log.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, line);
        buffer.insert(&mut end, "\n");
    }
}

fn start_install(stack: &gtk::Stack, d: &Draft) {
    let network = if d.dhcp {
        NetworkConfig::Dhcp
    } else {
        NetworkConfig::Static {
            cidr: d.cidr.clone(),
            gateway: d.gateway.clone(),
            dns: d.dns.clone(),
        }
    };

    let hash = match sysinfo::hash_password(&d.password) {
        Ok(hash) => hash,
        Err(err) => {
            PROGRESS.with(|slot| {
                if let Some(page) = slot.borrow().as_ref() {
                    page.status
                        .set_text(&format!("Cannot hash password: {err}"));
                    page.status.add_css_class("qz-error");
                }
            });
            go(stack, "progress");
            return;
        }
    };

    let cfg = InstallConfig {
        root_password_hash: hash,
        timezone: d.timezone.clone(),
        nic: d.nic.clone(),
        network,
        disk: d.disk.clone(),
    };
    let plan = engine::plan::build_plan(&cfg, &BuildPins::load());

    let (tx, rx) = async_channel::unbounded::<engine::Event>();
    std::thread::spawn(move || engine::run(&plan, &tx));

    go(stack, "progress");
    let stack = stack.clone();
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            PROGRESS.with(|slot| {
                let borrow = slot.borrow();
                let Some(page) = borrow.as_ref() else { return };
                match &event {
                    engine::Event::StepStarted {
                        index,
                        total,
                        title,
                    } => {
                        page.bar.set_fraction(*index as f64 / *total as f64);
                        page.bar.set_text(Some(&format!("{}/{}", index + 1, total)));
                        page.status.set_text(title);
                    }
                    engine::Event::Line(line) => page.append_log(line),
                    engine::Event::Finished(Ok(())) => {
                        page.bar.set_fraction(1.0);
                        go(&stack, "done");
                    }
                    engine::Event::Finished(Err(message)) => {
                        page.status.set_text(&format!(
                            "Installation failed: {message} (full log: {})",
                            engine::LOG_PATH
                        ));
                        page.status.add_css_class("qz-error");
                        page.reboot.set_visible(true);
                    }
                }
            });
            if matches!(event, engine::Event::Finished(_)) {
                break;
            }
        }
    });
}

fn page_done() -> gtk::Box {
    let (outer, content, footer) = shell(
        "COMPLETE",
        "Installation complete",
        "Remove the installation media, then reboot into Lumen.",
    );
    let note = gtk::Label::new(Some(
        "First boot relabels the filesystem for SELinux and may take a few \
         minutes. Log in as root with the password you chose.",
    ));
    note.set_wrap(true);
    note.set_halign(gtk::Align::Start);
    content.append(&note);

    let reboot = nav_button("Reboot ▸", true);
    reboot.connect_clicked(|_| {
        let _ = std::process::Command::new("systemctl")
            .arg("reboot")
            .spawn();
    });
    footer.append(&reboot);
    outer
}

fn valid_cidr(text: &str) -> bool {
    match text.split_once('/') {
        Some((addr, prefix)) => {
            Ipv4Addr::from_str(addr).is_ok()
                && prefix
                    .parse::<u8>()
                    .map(|p| (1..=32).contains(&p))
                    .unwrap_or(false)
        }
        None => false,
    }
}
