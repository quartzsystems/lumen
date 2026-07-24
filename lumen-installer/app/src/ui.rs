//! GTK4 wizard: welcome -> root password -> location & time zone ->
//! management network -> boot drive -> summary -> progress -> done.
//! Linear flow over a GtkStack; state collected into `Draft`, converted to
//! an InstallConfig when the operator confirms the summary.

use crate::config::{parse_netmask, BuildPins, InstallConfig, NetworkConfig, PoolTopology};
use crate::engine;
use crate::sysinfo;

use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::str::FromStr;

const PAGES: [&str; 8] = [
    "welcome", "password", "location", "network", "disk", "summary", "progress", "done",
];

#[derive(Default)]
struct Draft {
    password: String,
    country: String,
    timezone: String,
    keymap_label: String,
    keymap: String,
    nic: String,
    hostname: String,
    dhcp: bool,
    ip: String,
    netmask: String,
    prefix: u8,
    gateway: String,
    dns: Vec<String>,
    disks: Vec<String>,
    disk_labels: Vec<String>,
    topology: PoolTopology,
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
        country: "Other (UTC)".into(),
        timezone: "UTC".into(),
        keymap_label: "English (US)".into(),
        keymap: "us".into(),
        hostname: "lumen".into(),
        ..Default::default()
    }));

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    // The overlay hosts the confirm modal (scrim + centered card); a
    // transient window is placed by the compositor, not centered.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&stack));

    stack.add_named(&page_welcome(&stack), Some(PAGES[0]));
    stack.add_named(&page_password(&stack, &draft), Some(PAGES[1]));
    stack.add_named(&page_location(&stack, &draft), Some(PAGES[2]));
    stack.add_named(&page_network(&stack, &draft), Some(PAGES[3]));
    stack.add_named(&page_disk(&stack, &draft), Some(PAGES[4]));
    stack.add_named(&page_summary(&stack, &draft, &overlay), Some(PAGES[5]));

    let progress = ProgressPage::new();
    stack.add_named(&progress.root, Some(PAGES[6]));
    stack.add_named(&page_done(), Some(PAGES[7]));

    PROGRESS.with(|slot| *slot.borrow_mut() = Some(progress));

    window.set_child(Some(&overlay));
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

mod logo {
    //! Full-resolution logo texture behind a fixed logical size.
    //!
    //! GtkPicture sizes itself to the paintable's intrinsic size, so handing
    //! it the full 808x288 texture renders huge, while pre-scaling to a 40px
    //! bitmap bakes in a blurry logo (bilinear pixbuf downscaling, stretched
    //! again on scaled displays). This paintable reports the small layout
    //! size but snapshots the full-resolution texture, so GSK filters it for
    //! the actual output scale at render time.

    use gtk::subclass::prelude::*;
    use gtk::{gdk, glib, graphene, gsk, prelude::*};

    mod imp {
        use super::*;
        use std::cell::{Cell, OnceCell};

        #[derive(Default)]
        pub struct LogoPaintable {
            pub texture: OnceCell<gdk::Texture>,
            pub height: Cell<i32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for LogoPaintable {
            const NAME: &'static str = "LumenLogoPaintable";
            type Type = super::LogoPaintable;
            type Interfaces = (gdk::Paintable,);
        }

        impl ObjectImpl for LogoPaintable {}

        impl PaintableImpl for LogoPaintable {
            fn flags(&self) -> gdk::PaintableFlags {
                gdk::PaintableFlags::SIZE | gdk::PaintableFlags::CONTENTS
            }

            fn intrinsic_height(&self) -> i32 {
                self.height.get()
            }

            fn intrinsic_width(&self) -> i32 {
                let texture = self.texture.get().unwrap();
                self.height.get() * texture.width() / texture.height()
            }

            fn intrinsic_aspect_ratio(&self) -> f64 {
                let texture = self.texture.get().unwrap();
                f64::from(texture.width()) / f64::from(texture.height())
            }

            fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
                let Some(texture) = self.texture.get() else {
                    return;
                };
                let snapshot = snapshot.downcast_ref::<gtk::Snapshot>().unwrap();
                snapshot.append_scaled_texture(
                    texture,
                    gsk::ScalingFilter::Trilinear,
                    &graphene::Rect::new(0.0, 0.0, width as f32, height as f32),
                );
            }
        }
    }

    glib::wrapper! {
        pub struct LogoPaintable(ObjectSubclass<imp::LogoPaintable>)
            @implements gdk::Paintable;
    }

    impl LogoPaintable {
        pub fn new(texture: gdk::Texture, height: i32) -> Self {
            let obj: Self = glib::Object::new();
            obj.imp().texture.set(texture).unwrap();
            obj.imp().height.set(height);
            obj
        }
    }
}

/// Lumen lockup (dark-background PNG compiled into the binary), the same
/// height on every page. The texture stays at full resolution — LogoPaintable
/// gives it a 40px intrinsic size and lets the renderer downsample per
/// output scale, keeping the logo sharp.
fn lumen_lockup() -> Option<gtk::Picture> {
    const HEIGHT: i32 = 40;
    let bytes = glib::Bytes::from_static(include_bytes!(
        "../../../branding/logos/png/lumen-lockup-dark-bg.png"
    ));
    let texture = gtk::gdk::Texture::from_bytes(&bytes).ok()?;
    let paintable = logo::LogoPaintable::new(texture, HEIGHT);
    let picture = gtk::Picture::for_paintable(&paintable);
    picture.set_halign(gtk::Align::Start);
    Some(picture)
}

fn shell(step: &str, title: &str, subtitle: &str) -> (gtk::Box, gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_top(40);
    // Bottom margin matches the side margin so the nav buttons sit the
    // same distance from the bottom edge as from the right edge.
    outer.set_margin_bottom(96);
    outer.set_margin_start(96);
    outer.set_margin_end(96);

    if let Some(logo) = lumen_lockup() {
        logo.set_margin_bottom(28);
        outer.append(&logo);
    }

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
    let button = gtk::Button::new();
    // The ◂/▸ glyphs need to render larger than the label text, but mixed
    // Pango sizes in one label share a baseline, which sits the whole line
    // below the button's vertical center. Give each glyph its own label
    // (sized via .qz-nav-arrow) so the box centers them as widgets.
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::Center);
    for part in label.split(' ') {
        let piece = gtk::Label::new(Some(part));
        piece.set_valign(gtk::Align::Center);
        if part == "◂" || part == "▸" {
            piece.add_css_class("qz-nav-arrow");
        }
        row.append(&piece);
    }
    button.set_child(Some(&row));
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn go(stack: &gtk::Stack, page: &str) {
    stack.set_visible_child_name(page);
}

/// DropDown over plain strings with type-to-search enabled.
fn string_dropdown(items: &[&str]) -> gtk::DropDown {
    let dd = gtk::DropDown::from_strings(items);
    dd.set_enable_search(true);
    dd.set_expression(Some(&gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        gtk::Expression::NONE,
        "string",
    )));
    dd
}

fn set_strings(dd: &gtk::DropDown, items: &[String]) {
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    dd.set_model(Some(&gtk::StringList::new(&refs)));
    dd.set_selected(0);
}

fn form_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Start);
    label
}

fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

// --- pages -------------------------------------------------------------------

fn page_welcome(stack: &gtk::Stack) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_top(40);
    outer.set_margin_bottom(96);
    outer.set_margin_start(96);
    outer.set_margin_end(96);

    match lumen_lockup() {
        Some(logo) => {
            logo.set_margin_bottom(28);
            outer.append(&logo);
        }
        None => {
            let title = gtk::Label::new(Some("Lumen"));
            title.add_css_class("qz-title");
            title.set_halign(gtk::Align::Start);
            title.set_margin_bottom(28);
            outer.append(&title);
        }
    }

    // "Quartz Systems Lumen release 0.1.0" -> "Quartz Systems Lumen 0.1.0".
    let subtitle = gtk::Label::new(Some(&sysinfo::lumen_version().replace(" release ", " ")));
    subtitle.add_css_class("qz-subtitle");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_margin_top(16);
    outer.append(&subtitle);

    let tagline = gtk::Label::new(Some(
        "Lumen — a KVM hypervisor built to illuminate your infrastructure.",
    ));
    tagline.add_css_class("qz-tagline");
    tagline.set_halign(gtk::Align::Start);
    tagline.set_wrap(true);
    tagline.set_margin_top(6);
    tagline.set_margin_bottom(24);
    outer.append(&tagline);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_vexpand(true);
    outer.append(&content);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.set_halign(gtk::Align::End);
    footer.set_margin_top(24);
    outer.append(&footer);

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
            "This wizard collects the root password, location, management \
             network, and target drives, then installs Lumen onto a ZFS \
             pool (rpool) — a single drive, a mirror, or RAIDZ. All \
             selected drives are completely erased.",
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
        "Root Password",
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
    form.set_size_request(520, -1);
    form.append(&form_label("Password"));
    form.append(&entry);
    form.append(&form_label("Confirm password"));
    form.append(&confirm);
    form.append(&hint);
    content.append(&form);

    let back = nav_button("◂ Back", false);
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
            go(&stack, "location");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

fn page_location(stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) -> gtk::Box {
    let (outer, content, footer) = shell(
        "STEP 2 OF 4",
        "Location and Time Zone",
        "Selecting a country narrows the time zone list. The keyboard layout \
         applies to the installed system's console.",
    );

    let countries = Rc::new(sysinfo::countries());
    let country_names: Vec<&str> = countries.iter().map(|c| c.name.as_str()).collect();
    let country_dd = string_dropdown(&country_names);
    let zone_dd = string_dropdown(&["UTC"]);
    let keymap_labels: Vec<&str> = sysinfo::keymaps().iter().map(|(label, _)| *label).collect();
    let keymap_dd = string_dropdown(&keymap_labels);

    // Zones offered for the currently selected country.
    let current_zones: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(vec!["UTC".into()]));

    let refresh_zones = {
        let countries = countries.clone();
        let zone_dd = zone_dd.clone();
        let current_zones = current_zones.clone();
        move |country_index: u32| {
            // Only the selected country's zones are offered.
            let Some(country) = countries.get(country_index as usize) else {
                return;
            };
            set_strings(&zone_dd, &country.zones);
            *current_zones.borrow_mut() = country.zones.clone();
        }
    };
    refresh_zones(0);
    {
        let refresh_zones = refresh_zones.clone();
        country_dd.connect_selected_notify(move |dd| refresh_zones(dd.selected()));
    }

    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.add_css_class("qz-card");
    form.set_halign(gtk::Align::Start);
    form.set_size_request(520, -1);
    form.append(&form_label("Country"));
    form.append(&country_dd);
    form.append(&form_label("Time Zone"));
    form.append(&zone_dd);
    form.append(&form_label("Keyboard layout"));
    form.append(&keymap_dd);
    content.append(&form);

    let back = nav_button("◂ Back", false);
    let next = nav_button("Next ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "password"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let countries = countries.clone();
        let country_dd = country_dd.clone();
        let zone_dd = zone_dd.clone();
        let keymap_dd = keymap_dd.clone();
        let current_zones = current_zones.clone();
        next.connect_clicked(move |_| {
            let mut d = draft.borrow_mut();
            if let Some(country) = countries.get(country_dd.selected() as usize) {
                d.country = country.name.clone();
            }
            {
                let zones = current_zones.borrow();
                d.timezone = zones
                    .get(zone_dd.selected() as usize)
                    .cloned()
                    .unwrap_or_else(|| "UTC".into());
            }
            let (label, code) = sysinfo::keymaps()
                .get(keymap_dd.selected() as usize)
                .copied()
                .unwrap_or(("English (US)", "us"));
            d.keymap_label = label.into();
            d.keymap = code.into();
            drop(d);
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
        "Management Network",
        "The interface used to manage this appliance. NIC names are final: \
         the installed system uses the same nic0…nicN names.",
    );

    let nics: Rc<RefCell<Vec<sysinfo::Nic>>> = Rc::new(RefCell::new(Vec::new()));
    let nic_dd = string_dropdown(&["(scanning…)"]);

    let populate = {
        let nic_dd = nic_dd.clone();
        let nics = nics.clone();
        move || {
            let found = sysinfo::nics();
            let labels: Vec<String> = found
                .iter()
                .map(|nic| {
                    let state = if nic.link_up {
                        match nic.speed_mbps {
                            Some(mbps) => format!("link up · {mbps} Mb/s"),
                            None => "link up".into(),
                        }
                    } else {
                        "no link".to_string()
                    };
                    let driver = if nic.driver.is_empty() {
                        "unknown driver"
                    } else {
                        nic.driver.as_str()
                    };
                    format!("{} · {} · {state} · {driver}", nic.name, nic.mac)
                })
                .collect();
            set_strings(&nic_dd, &labels);
            *nics.borrow_mut() = found;
        }
    };
    populate();

    let rescan = gtk::Button::with_label("Rescan");
    {
        let populate = populate.clone();
        rescan.connect_clicked(move |_| populate());
    }

    let hostname_entry = gtk::Entry::new();
    hostname_entry.set_text("lumen");
    hostname_entry.set_placeholder_text(Some("hostname or FQDN, e.g. lumen01.example.lan"));

    let dhcp_radio = gtk::CheckButton::with_label("DHCP");
    dhcp_radio.set_active(true);
    let static_radio = gtk::CheckButton::with_label("Static");
    static_radio.set_group(Some(&dhcp_radio));
    let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    mode_box.append(&dhcp_radio);
    mode_box.append(&static_radio);

    let ip_entry = gtk::Entry::new();
    ip_entry.set_placeholder_text(Some("e.g. 192.168.10.5"));
    let netmask_entry = gtk::Entry::new();
    netmask_entry.set_placeholder_text(Some("e.g. 255.255.255.0 (or prefix length)"));
    let gateway_entry = gtk::Entry::new();
    gateway_entry.set_placeholder_text(Some("e.g. 192.168.10.1"));
    let dns_entry = gtk::Entry::new();
    dns_entry.set_placeholder_text(Some("e.g. 192.168.10.1 (comma-separated for several)"));

    // Everything except the hostname greys out while DHCP is selected.
    let static_grid = gtk::Grid::new();
    static_grid.set_row_spacing(8);
    static_grid.set_column_spacing(16);
    for (row, (label, entry)) in [
        ("IP address", &ip_entry),
        ("Netmask", &netmask_entry),
        ("Gateway", &gateway_entry),
        ("DNS server", &dns_entry),
    ]
    .iter()
    .enumerate()
    {
        static_grid.attach(&form_label(label), 0, row as i32, 1, 1);
        entry.set_hexpand(true);
        static_grid.attach(*entry, 1, row as i32, 1, 1);
    }
    let dhcp_hint = gtk::Label::new(Some(
        "DHCP assigns the address, gateway, and DNS automatically.",
    ));
    dhcp_hint.add_css_class("qz-hint");
    dhcp_hint.set_halign(gtk::Align::Start);

    static_grid.set_sensitive(false);
    {
        let static_grid = static_grid.clone();
        let dhcp_hint = dhcp_hint.clone();
        static_radio.connect_toggled(move |radio| {
            static_grid.set_sensitive(radio.is_active());
            dhcp_hint.set_visible(!radio.is_active());
        });
    }

    let error = gtk::Label::new(None);
    error.add_css_class("qz-error");
    error.set_halign(gtk::Align::Start);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 10);
    form.add_css_class("qz-card");
    form.set_halign(gtk::Align::Start);
    form.set_size_request(680, -1);
    form.append(&form_label("Management interface"));
    let nic_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    nic_dd.set_hexpand(true);
    nic_row.append(&nic_dd);
    nic_row.append(&rescan);
    form.append(&nic_row);
    form.append(&form_label("Hostname (FQDN)"));
    form.append(&hostname_entry);
    form.append(&mode_box);
    form.append(&dhcp_hint);
    form.append(&static_grid);
    form.append(&error);
    content.append(&form);

    let back = nav_button("◂ Back", false);
    let next = nav_button("Next ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "location"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let nics = nics.clone();
        let nic_dd = nic_dd.clone();
        let hostname_entry = hostname_entry.clone();
        let dhcp_radio = dhcp_radio.clone();
        let ip_entry = ip_entry.clone();
        let netmask_entry = netmask_entry.clone();
        let gateway_entry = gateway_entry.clone();
        let dns_entry = dns_entry.clone();
        let error = error.clone();
        next.connect_clicked(move |_| {
            let nics = nics.borrow();
            let Some(nic) = nics.get(nic_dd.selected() as usize) else {
                error.set_text("Select a management interface.");
                return;
            };
            let hostname = hostname_entry.text().trim().to_string();
            if !valid_hostname(&hostname) {
                error.set_text("Invalid hostname (letters, digits, dashes, dot-separated).");
                return;
            }
            let mut d = draft.borrow_mut();
            d.nic = nic.name.clone();
            d.hostname = hostname;
            d.dhcp = dhcp_radio.is_active();
            if !d.dhcp {
                let ip = ip_entry.text().trim().to_string();
                if Ipv4Addr::from_str(&ip).is_err() {
                    error.set_text("Invalid IP address.");
                    return;
                }
                let netmask = netmask_entry.text().trim().to_string();
                let Some(prefix) = parse_netmask(&netmask) else {
                    error.set_text("Invalid netmask (e.g. 255.255.255.0 or 24).");
                    return;
                };
                let gateway = gateway_entry.text().trim().to_string();
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
                d.ip = ip;
                d.netmask = netmask;
                d.prefix = prefix;
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
        "Boot Drives",
        "Select the drives for the ZFS pool (rpool) and its layout. Every \
         selected drive is completely erased; the first selected drive also \
         carries the EFI system partition and /boot.",
    );

    let disks: Rc<RefCell<Vec<sysinfo::Disk>>> = Rc::new(RefCell::new(Vec::new()));
    let checks: Rc<RefCell<Vec<gtk::CheckButton>>> = Rc::new(RefCell::new(Vec::new()));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);

    let populate = {
        let list = list.clone();
        let disks = disks.clone();
        let checks = checks.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            checks.borrow_mut().clear();
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
                let check = gtk::CheckButton::new();
                check.set_valign(gtk::Align::Center);
                let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
                let title_label = gtk::Label::new(Some(&disk.path));
                title_label.set_halign(gtk::Align::Start);
                let subtitle_label = gtk::Label::new(Some(&details));
                subtitle_label.set_halign(gtk::Align::Start);
                subtitle_label.add_css_class("qz-hint");
                text.append(&title_label);
                text.append(&subtitle_label);
                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row_box.append(&check);
                row_box.append(&text);
                let row = gtk::ListBoxRow::new();
                row.set_activatable(true);
                row.set_child(Some(&row_box));
                list.append(&row);
                checks.borrow_mut().push(check);
            }
            *disks.borrow_mut() = found;
            // Preselect the first drive for the common single-disk case.
            if let Some(first) = checks.borrow().first() {
                first.set_active(true);
            }
        }
    };
    populate();
    {
        // Clicking anywhere on a row toggles its checkbox.
        let checks = checks.clone();
        list.connect_row_activated(move |_, row| {
            if let Some(check) = checks.borrow().get(row.index() as usize) {
                check.set_active(!check.is_active());
            }
        });
    }

    let rescan = gtk::Button::with_label("Rescan");
    {
        let populate = populate.clone();
        rescan.connect_clicked(move |_| populate());
    }

    let topology_labels: Vec<&str> = PoolTopology::ALL.iter().map(|t| t.label()).collect();
    let topology_dd = string_dropdown(&topology_labels);
    let topology_hint = gtk::Label::new(Some(
        "Mirror needs 2 or more drives, RAIDZ1 needs 3, RAIDZ2 needs 4. \
         Pool capacity follows the smallest drive.",
    ));
    topology_hint.add_css_class("qz-hint");
    topology_hint.set_halign(gtk::Align::Start);
    topology_hint.set_wrap(true);

    let error = gtk::Label::new(None);
    error.add_css_class("qz-error");
    error.set_halign(gtk::Align::Start);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("qz-card");
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&list));
    scroll.set_min_content_height(220);
    card.append(&scroll);
    card.append(&rescan);
    card.append(&form_label("Pool layout"));
    card.append(&topology_dd);
    card.append(&topology_hint);
    card.append(&error);
    content.append(&card);

    let back = nav_button("◂ Back", false);
    let next = nav_button("Review ▸", true);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "network"));
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let disks = disks.clone();
        let checks = checks.clone();
        let topology_dd = topology_dd.clone();
        let error = error.clone();
        next.connect_clicked(move |_| {
            let disks = disks.borrow();
            let selected: Vec<&sysinfo::Disk> = checks
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, check)| check.is_active())
                .filter_map(|(i, _)| disks.get(i))
                .collect();
            let topology = PoolTopology::ALL[topology_dd.selected() as usize];
            if selected.is_empty() {
                error.set_text("Select at least one target drive.");
                return;
            }
            if topology == PoolTopology::Single && selected.len() != 1 {
                error.set_text(
                    "Single disk uses exactly one drive — pick a RAID layout for several.",
                );
                return;
            }
            if selected.len() < topology.min_disks() {
                error.set_text(&format!(
                    "{} needs at least {} drives ({} selected).",
                    topology.label(),
                    topology.min_disks(),
                    selected.len()
                ));
                return;
            }
            {
                let mut d = draft.borrow_mut();
                d.topology = topology;
                d.disks = selected.iter().map(|disk| disk.path.clone()).collect();
                d.disk_labels = selected
                    .iter()
                    .map(|disk| {
                        format!(
                            "{} ({}, {})",
                            disk.path,
                            sysinfo::human_size(disk.size_bytes),
                            if disk.model.is_empty() {
                                "unknown model"
                            } else {
                                &disk.model
                            }
                        )
                    })
                    .collect();
            }
            error.set_text("");
            // The summary page rebuilds its rows when it becomes visible.
            go(&stack, "summary");
        });
    }
    footer.append(&back);
    footer.append(&next);
    outer
}

/// Summary rows in the order the wizard collected them: password,
/// location, network, then drives.
fn summary_rows(d: &Draft) -> Vec<(String, String)> {
    let mut rows = vec![
        ("Root password".into(), "Password Set".into()),
        ("Country".into(), d.country.clone()),
        ("Time zone".into(), d.timezone.clone()),
        (
            "Keyboard".into(),
            format!("{} ({})", d.keymap_label, d.keymap),
        ),
        ("Management interface".into(), d.nic.clone()),
        ("Hostname".into(), d.hostname.clone()),
    ];
    if d.dhcp {
        rows.push(("Network".into(), "DHCP".into()));
    } else {
        rows.push(("IP address".into(), format!("{}/{}", d.ip, d.prefix)));
        rows.push(("Netmask".into(), d.netmask.clone()));
        rows.push(("Gateway".into(), d.gateway.clone()));
        rows.push((
            "DNS server".into(),
            if d.dns.is_empty() {
                "—".into()
            } else {
                d.dns.join(", ")
            },
        ));
    }
    rows.push(("ZFS pool".into(), format!("rpool — {}", d.topology.label())));
    rows.push((
        if d.disk_labels.len() == 1 {
            "Drive".into()
        } else {
            "Drives".into()
        },
        d.disk_labels.join("\n"),
    ));
    rows
}

fn page_summary(
    stack: &gtk::Stack,
    draft: &Rc<RefCell<Draft>>,
    overlay: &gtk::Overlay,
) -> gtk::Box {
    let (outer, content, footer) = shell(
        "REVIEW",
        "Summary",
        "Verify the configuration. Nothing has been written yet — installation \
         starts only after confirmation and erases the selected drives.",
    );

    let table = gtk::ListBox::new();
    table.set_selection_mode(gtk::SelectionMode::None);
    table.add_css_class("qz-summary");
    content.append(&table);

    // Rebuild the table every time the page becomes visible.
    {
        let table = table.clone();
        let draft = draft.clone();
        stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() != Some("summary") {
                return;
            }
            while let Some(child) = table.first_child() {
                table.remove(&child);
            }
            for (key, value) in summary_rows(&draft.borrow()) {
                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 16);
                // Mono-uppercase key column, matching the Quartz table
                // header style (GTK CSS has no text-transform).
                let key_label = gtk::Label::new(Some(&key.to_uppercase()));
                key_label.add_css_class("qz-summary-key");
                key_label.set_halign(gtk::Align::Start);
                key_label.set_valign(gtk::Align::Center);
                key_label.set_size_request(260, -1);
                let value_label = gtk::Label::new(Some(&value));
                value_label.add_css_class("qz-summary-value");
                value_label.set_halign(gtk::Align::Start);
                value_label.set_wrap(true);
                value_label.set_hexpand(true);
                row_box.append(&key_label);
                row_box.append(&value_label);
                let row = gtk::ListBoxRow::new();
                row.set_activatable(false);
                row.set_child(Some(&row_box));
                table.append(&row);
            }
        });
    }

    let back = nav_button("◂ Back", false);
    {
        let stack = stack.clone();
        back.connect_clicked(move |_| go(&stack, "disk"));
    }

    let install = gtk::Button::with_label("Erase & install");
    install.add_css_class("destructive-action");
    {
        let stack = stack.clone();
        let draft = draft.clone();
        let overlay = overlay.clone();
        install.connect_clicked(move |_| {
            confirm_erase(&overlay, &stack, &draft);
        });
    }
    footer.append(&back);
    footer.append(&install);
    outer
}

/// Quartz-styled destructive confirmation. GtkAlertDialog brings the stock
/// theme's light chrome, and a transient window is placed by the
/// compositor (top-ish under gnome-kiosk) — an overlay child with a scrim
/// is dead-centered instead.
fn confirm_erase(overlay: &gtk::Overlay, stack: &gtk::Stack, draft: &Rc<RefCell<Draft>>) {
    let scrim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scrim.add_css_class("qz-scrim");
    scrim.set_hexpand(true);
    scrim.set_vexpand(true);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("qz-dialog");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Center);
    card.set_vexpand(true);
    card.set_size_request(520, -1);

    let many = draft.borrow().disk_labels.len() > 1;
    let title = gtk::Label::new(Some(if many {
        "Erase drives and install Lumen?"
    } else {
        "Erase disk and install Lumen?"
    }));
    title.add_css_class("qz-dialog-title");
    title.set_halign(gtk::Align::Start);
    let detail = gtk::Label::new(Some(&if many {
        format!(
            "ALL DATA on these drives will be destroyed:\n{}",
            draft.borrow().disk_labels.join("\n")
        )
    } else {
        format!(
            "ALL DATA on {} will be destroyed.",
            draft.borrow().disk_labels.join(", ")
        )
    }));
    detail.add_css_class("qz-subtitle");
    detail.set_wrap(true);
    detail.set_halign(gtk::Align::Start);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    let cancel = gtk::Button::with_label("Cancel");
    let erase = gtk::Button::with_label("Erase & Install");
    erase.add_css_class("destructive-action");

    let close = {
        let overlay = overlay.clone();
        let stack = stack.clone();
        let scrim = scrim.clone();
        move || {
            overlay.remove_overlay(&scrim);
            stack.set_sensitive(true);
        }
    };
    {
        let close = close.clone();
        cancel.connect_clicked(move |_| close());
    }
    {
        let stack = stack.clone();
        let draft = draft.clone();
        erase.connect_clicked(move |_| {
            close();
            start_install(&stack, &draft.borrow());
        });
    }
    buttons.append(&cancel);
    buttons.append(&erase);

    card.append(&title);
    card.append(&detail);
    card.append(&buttons);
    scrim.append(&card);
    overlay.add_overlay(&scrim);
    // The scrim swallows pointer events; disable the page beneath so
    // keyboard focus cannot reach it either.
    stack.set_sensitive(false);
    cancel.grab_focus();
}

struct ProgressPage {
    root: gtk::Box,
    bar: gtk::ProgressBar,
    status: gtk::Label,
    log: gtk::TextView,
    details: gtk::Expander,
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
        log.buffer()
            .create_mark(Some("end"), &log.buffer().end_iter(), false);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&log));
        scroll.set_vexpand(true);
        scroll.set_min_content_height(260);

        // Console output collapsed by default.
        let details = gtk::Expander::new(Some("Show details"));
        details.set_expanded(false);
        details.set_child(Some(&scroll));
        details.add_css_class("qz-details");

        content.append(&bar);
        content.append(&status);
        content.append(&details);

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
            details,
            reboot,
        }
    }

    fn append_log(&self, line: &str) {
        let buffer = self.log.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, line);
        buffer.insert(&mut end, "\n");
        // Follow the newest output ("end" mark is right-gravity, created
        // in new()) so the expanded details pane acts like tail -f.
        if let Some(mark) = buffer.mark("end") {
            self.log.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        }
    }
}

fn start_install(stack: &gtk::Stack, d: &Draft) {
    let network = if d.dhcp {
        NetworkConfig::Dhcp
    } else {
        NetworkConfig::Static {
            cidr: format!("{}/{}", d.ip, d.prefix),
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
        keymap: d.keymap.clone(),
        hostname: d.hostname.clone(),
        nic: d.nic.clone(),
        network,
        disks: d.disks.clone(),
        topology: d.topology,
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
                        // Surface the log so the failure is visible.
                        page.details.set_expanded(true);
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
        "Installation Complete",
        "Remove the installation media, then reboot into Lumen.",
    );
    let note = gtk::Label::new(Some("Log in as root with the password you chose."));
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
