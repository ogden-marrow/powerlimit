//! Native GTK4 window ("Console" look: sepia paper or Monokai Pro dark, monospace, keycap buttons).
//! Runs unprivileged; anything that writes limits goes through `pkexec powerlimit ...`
//! so the polkit dialog handles authentication.

use gtk::cairo;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::{candidates, current, last_applied, load_profiles, sample, Limits};

enum Msg {
    Log(String),
    TuneDone,
    Applied(Result<(), String>),
    Watch(String),
    WatchEnded,
}

// ---------------------------------------------------------------- palettes, settings

#[derive(Clone, Copy)]
struct Palette {
    bg: &'static str,
    panel: &'static str,
    track: &'static str,
    ink: &'static str,
    mut_: &'static str,
    line: &'static str,
    line_a: f64,
    accent: &'static str,
    accent_ink: &'static str,
    accent_lo: &'static str,
    blue: &'static str,
    ok: &'static str,
    bad: &'static str,
}

/// Monokai Pro style
const DARK: Palette = Palette {
    bg: "#2D2A2E", panel: "#221F22", track: "#403E41", ink: "#FCFCFA", mut_: "#9C9B9C", line: "#FCFCFA", line_a: 0.34,
    accent: "#FFD866", accent_ink: "#2D2A2E", accent_lo: "#B8962E", blue: "#78DCE8", ok: "#A9DC76", bad: "#FF6188",
};
const SEPIA: Palette = Palette {
    bg: "#F4ECD8", panel: "#FBF5E6", track: "#E6D9BC", ink: "#3B2F22", mut_: "#725F4B", line: "#3B2F22", line_a: 1.0,
    accent: "#9C4A1A", accent_ink: "#FBF5E6", accent_lo: "#6B3010", blue: "#5C7F8E", ok: "#5B7F3A", bad: "#A8322D",
};

fn rgb(h: &str) -> (f64, f64, f64) {
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
    (v(1), v(3), v(5))
}

fn rgba_css(h: &str, a: f64) -> String {
    let (r, g, b) = rgb(h);
    format!("rgba({:.0},{:.0},{:.0},{a})", r * 255.0, g * 255.0, b * 255.0)
}

fn hex(cr: &cairo::Context, h: &str, a: f64) {
    let (r, g, b) = rgb(h);
    cr.set_source_rgba(r, g, b, a);
}

fn css(p: &Palette, font: &str) -> String {
    let stack = format!("\"{font}\", \"IBM Plex Mono\", \"JetBrains Mono\", \"DejaVu Sans Mono\", monospace");
    include_str!("style.css")
        .replace("@@bg@@", p.bg).replace("@@panel@@", p.panel).replace("@@track@@", p.track).replace("@@ink@@", p.ink)
        .replace("@@mut@@", p.mut_).replace("@@line@@", &rgba_css(p.line, p.line_a)).replace("@@rule@@", &rgba_css(p.line, 0.45))
        .replace("@@accentInk@@", p.accent_ink).replace("@@accentLo@@", p.accent_lo).replace("@@accent@@", p.accent)
        .replace("@@blue@@", p.blue).replace("@@ok@@", p.ok).replace("@@bad@@", p.bad).replace("@@font@@", &stack)
}

fn cfg_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("powerlimit").join("ui.conf")
}

#[derive(Clone)]
struct Cfg {
    theme: String,
    font: String,
    tray: bool,
    close_to_tray: bool,
    startup: String,
    login_watch: bool,
    profiles: String,
    minutes: f64,
    cpu_max: f64,
    gpu_max: f64,
    last: String,
}

fn load_cfg() -> Cfg {
    let t = std::fs::read_to_string(cfg_path()).unwrap_or_default();
    let get = |k: &str| t.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_string);
    let flag = |k: &str, d: bool| get(k).map(|v| v == "1").unwrap_or(d);
    let num = |k: &str, d: f64| get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    Cfg {
        theme: get("theme").unwrap_or("auto".into()),
        font: get("font").unwrap_or("IBM Plex Mono".into()),
        tray: flag("tray", true),
        close_to_tray: flag("close_to_tray", true),
        startup: get("startup").unwrap_or("none".into()),
        login_watch: flag("login_watch", false),
        profiles: get("profiles").unwrap_or_default(),
        minutes: num("minutes", 30.0),
        cpu_max: num("cpu_max", 85.0),
        gpu_max: num("gpu_max", 80.0),
        last: get("last").unwrap_or_default(),
    }
}

fn save_cfg(c: &Cfg) {
    if std::env::var("POWERLIMIT_SHOT").is_ok() {
        return;
    }
    let p = cfg_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let b = |v: bool| if v { 1 } else { 0 };
    let _ = std::fs::write(p, format!(
        "theme={}\nfont={}\ntray={}\nclose_to_tray={}\nstartup={}\nlogin_watch={}\nprofiles={}\nminutes={}\ncpu_max={}\ngpu_max={}\nlast={}\n",
        c.theme, c.font, b(c.tray), b(c.close_to_tray), c.startup, b(c.login_watch), c.profiles, c.minutes, c.cpu_max, c.gpu_max, c.last));
}

fn autostart_path() -> std::path::PathBuf {
    cfg_path().parent().and_then(|d| d.parent()).map(|d| d.join("autostart").join("io.github.powerlimit.desktop")).unwrap_or_default()
}

fn set_autostart(on: bool) {
    let p = autostart_path();
    if !on {
        let _ = std::fs::remove_file(p);
        return;
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, format!(
        "[Desktop Entry]\nType=Application\nName=Power Limiter\nComment=Menu-bar indicator and login limits\nExec={} gui --background\nIcon=powerlimit\nTerminal=false\nX-GNOME-Autostart-enabled=true\nX-GNOME-Autostart-Delay=5\n", exe()));
}

/// Shared look state: current palette, CSS provider, and every custom-drawn widget to repaint on change.
struct Look {
    pal: Cell<Palette>,
    provider: gtk::CssProvider,
    draws: RefCell<Vec<gtk::DrawingArea>>,
    system_dark: bool,
}

impl Look {
    fn apply(&self, theme: &str, font: &str) {
        let dark = match theme {
            "dark" => true,
            "sepia" => false,
            _ => self.system_dark,
        };
        let p = if dark { DARK } else { SEPIA };
        self.pal.set(p);
        #[allow(deprecated)]
        self.provider.load_from_data(&css(&p, font));
        for d in self.draws.borrow().iter() {
            d.queue_draw();
        }
    }

    fn drawing(self: &Rc<Self>, w: i32, h: i32, f: impl Fn(&cairo::Context, f64, f64, &Palette) + 'static) -> gtk::DrawingArea {
        let d = gtk::DrawingArea::new();
        d.set_content_width(w);
        d.set_content_height(h);
        let look = self.clone();
        d.set_draw_func(move |_, cr, w, h| f(cr, w as f64, h as f64, &look.pal.get()));
        self.draws.borrow_mut().push(d.clone());
        d
    }
}

// ---------------------------------------------------------------- drawing

fn rrect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * PI / 2.0);
    cr.close_path();
}

/// Two concentric rounded rings (outer = CPU, inner = GPU), no tick marks.
fn rings(cr: &cairo::Context, w: f64, h: f64, cpu: f64, gpu: f64, track: (&str, f64), c1: &str, c2: &str) {
    let s = w.min(h);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let rw = s * 0.095;
    let r1 = s / 2.0 - rw / 2.0 - 1.0;
    let r2 = r1 - rw - s * 0.04;
    cr.set_line_width(rw);
    cr.set_line_cap(cairo::LineCap::Round);
    for (r, f, col) in [(r1, cpu, c1), (r2, gpu, c2)] {
        hex(cr, track.0, track.1);
        cr.arc(cx, cy, r, 0.0, 2.0 * PI);
        let _ = cr.stroke();
        if f > 0.0 {
            hex(cr, col, 1.0);
            cr.arc(cx, cy, r, -PI / 2.0, -PI / 2.0 + f.clamp(0.002, 1.0) * 2.0 * PI);
            let _ = cr.stroke();
        }
    }
}

fn meter(cr: &cairo::Context, w: f64, h: f64, frac: f64, p: &Palette, hot: bool) {
    let n = 24;
    let gap = 3.0;
    let sw = ((w - gap * (n as f64 - 1.0)) / n as f64).max(2.0);
    let lit = (frac.clamp(0.0, 1.0) * n as f64).round() as i32;
    for i in 0..n {
        hex(cr, if i < lit { if hot { p.bad } else { p.accent } } else { p.track }, 1.0);
        rrect(cr, i as f64 * (sw + gap), 0.0, sw, h, 3.5);
        let _ = cr.fill();
    }
}

fn progress(cr: &cairo::Context, w: f64, h: f64, frac: f64, p: &Palette) {
    hex(cr, p.line, p.line_a);
    cr.set_line_width(1.5);
    rrect(cr, 0.75, 0.75, w - 1.5, h - 1.5, (h - 1.5) / 2.0);
    let _ = cr.stroke();
    let fw = ((w - 6.0) * frac.clamp(0.0, 1.0)).max(0.0);
    if fw > 1.0 {
        rrect(cr, 3.0, 3.0, fw, h - 6.0, (h - 6.0) / 2.0);
        cr.clip();
        hex(cr, p.accent, 1.0);
        let mut y = 3.0;
        let mut row = 0;
        while y < h - 3.0 {
            let mut x = 3.0 + if row % 2 == 0 { 0.0 } else { 3.0 };
            while x < 3.0 + fw {
                cr.rectangle(x, y, 3.0, 3.0);
                x += 6.0;
            }
            y += 3.0;
            row += 1;
        }
        let _ = cr.fill();
        cr.reset_clip();
    }
}

// ---------------------------------------------------------------- small helpers

fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "powerlimit".into())
}

/// `pkexec powerlimit [--profiles PATH]`: root helpers use the profiles file the user chose.
fn pk() -> Command {
    let mut c = Command::new("pkexec");
    c.arg(exe());
    if let Ok(p) = std::env::var("POWERLIMIT_PROFILES") {
        c.args(["--profiles", &p]);
    }
    c
}

/// Run `pkexec powerlimit <args>` off the UI thread.
fn privileged(args: Vec<String>, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let r = match pk().args(&args).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) if o.status.code() == Some(126) => Err("Authentication cancelled".into()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("pkexec: {e}")),
        };
        let _ = tx.send(Msg::Applied(r));
    });
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn wrapped(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_wrap(true);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    l
}

fn key(text: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    b.add_css_class("key");
    b
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

fn panel() -> gtk::Box {
    let b = vbox(0);
    b.add_css_class("panel");
    b
}

fn rule() -> gtk::Box {
    let r = hbox(0);
    r.add_css_class("heavy-rule");
    r
}

/// "CPU sustained ........ 32 W"
fn leader_row(name: &str) -> (gtk::Box, gtk::Label) {
    let row = hbox(8);
    row.append(&label(name, &[]));
    let dots = hbox(0);
    dots.add_css_class("leader");
    dots.set_hexpand(true);
    dots.set_valign(gtk::Align::End);
    row.append(&dots);
    let v = label("–", &["val"]);
    row.append(&v);
    (row, v)
}

fn spin(min: f64, max: f64, val: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, 1.0);
    s.set_value(val);
    s
}

fn setting_row(title: &str, sub: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = hbox(12);
    let text = vbox(2);
    text.set_hexpand(true);
    text.append(&wrapped(title, &[]));
    if !sub.is_empty() {
        text.append(&wrapped(sub, &["mut", "sans"]));
    }
    row.append(&text);
    w.set_valign(gtk::Align::Center);
    row.append(w);
    row
}

fn nums(s: &str) -> Vec<f64> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in s.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || (c == '.' && !cur.is_empty()) {
            cur.push(c);
        } else {
            if let Ok(v) = cur.parse() {
                out.push(v);
            }
            cur.clear();
        }
    }
    out
}

fn applied_name(l: &Limits) -> String {
    let same = |v: &Limits| (l.pl1 - v.pl1).abs() < 0.5 && (l.pl2 - v.pl2).abs() < 0.5 && (l.gpu - v.gpu).abs() < 0.5;
    if let Some((n, _)) = load_profiles().iter().find(|(_, v)| same(v)) {
        return n.clone();
    }
    for (n, a, b, c) in PRESETS {
        if same(&Limits { pl1: a, pl2: b, gpu: c }) {
            return format!("{n} preset");
        }
    }
    if same(&Limits { pl1: 45.0, pl2: 109.0, gpu: 40.0 }) {
        return "Dell defaults".into();
    }
    "custom limits".into()
}

const PRESETS: [(&str, f64, f64, f64); 3] = [("QUIET", 25.0, 35.0, 25.0), ("BALANCED", 35.0, 45.0, 30.0), ("PERFORM", 45.0, 65.0, 40.0)];

pub fn run(background: bool) {
    let flags = if std::env::var("POWERLIMIT_SHOT").is_ok() { gtk::gio::ApplicationFlags::NON_UNIQUE } else { gtk::gio::ApplicationFlags::FLAGS_NONE };
    let app = gtk::Application::builder().application_id("io.github.powerlimit").flags(flags).build();
    let first = Cell::new(background);
    app.connect_activate(move |a| {
        if let Some(w) = a.windows().first() {
            w.set_visible(true);
            w.present();
            return;
        }
        build(a, first.replace(false));
    });
    app.run_with_args::<&str>(&[]);
}

// ---------------------------------------------------------------- window

fn build(app: &gtk::Application, background: bool) {
    gtk::Window::set_default_icon_name("powerlimit");

    let probe = gtk::Label::new(None);
    #[allow(deprecated)]
    let system_dark = {
        let c = probe.style_context().color();
        (c.red() + c.green() + c.blue()) / 3.0 > 0.5
    };
    let look = Rc::new(Look { pal: Cell::new(DARK), provider: gtk::CssProvider::new(), draws: RefCell::new(vec![]), system_dark });
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &look.provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    let cfg0 = load_cfg();
    if !cfg0.profiles.is_empty() && std::env::var("POWERLIMIT_PROFILES").is_err() {
        std::env::set_var("POWERLIMIT_PROFILES", &cfg0.profiles);
    }
    let cfg = Rc::new(RefCell::new(cfg0));
    look.apply(&cfg.borrow().theme, &cfg.borrow().font);

    let (tx, rx) = mpsc::channel::<Msg>();
    let stdin: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));
    let watcher: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));

    let win = gtk::ApplicationWindow::builder().application(app).title("Power Limiter").default_width(980).default_height(760).build();
    win.add_css_class("pl-window");
    win.set_size_request(480, 380);

    // header: striped title like a classic window, plus an appearance menu
    let header = gtk::HeaderBar::new();
    header.add_css_class("pl-header");
    let title = hbox(14);
    let s1 = hbox(0);
    s1.add_css_class("stripes");
    s1.set_hexpand(true);
    s1.set_valign(gtk::Align::Center);
    let s2 = hbox(0);
    s2.add_css_class("stripes");
    s2.set_hexpand(true);
    s2.set_valign(gtk::Align::Center);
    title.append(&s1);
    title.append(&label("POWER LIMITER", &["title"]));
    title.append(&s2);
    title.set_hexpand(true);
    header.set_title_widget(Some(&title));

    let families: Vec<String> = {
        let installed: Vec<String> = win.pango_context().list_families().iter().map(|f| f.name().to_string()).collect();
        let mut v: Vec<String> = ["IBM Plex Mono", "JetBrains Mono", "Space Mono", "DM Mono", "Martian Mono", "DejaVu Sans Mono"]
            .iter().filter(|f| installed.iter().any(|i| i == *f)).map(|s| s.to_string()).collect();
        if v.is_empty() {
            v.push("monospace".into());
        }
        v
    };
    header.set_decoration_layout(Some(":minimize,maximize,close"));
    win.set_titlebar(Some(&header));

    if !families.contains(&cfg.borrow().font) {
        cfg.borrow_mut().font = families[0].clone();
    }
    let restyle: Rc<dyn Fn()> = {
        let (look, cfg) = (look.clone(), cfg.clone());
        Rc::new(move || {
            look.apply(&cfg.borrow().theme, &cfg.borrow().font);
            save_cfg(&cfg.borrow());
        })
    };

    // body: tabs, pages, status bar
    let root = vbox(0);
    root.add_css_class("pl-root");
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&root));
    let toast_label = label("", &["toast"]);
    let toast = gtk::Revealer::new();
    toast.set_child(Some(&toast_label));
    toast.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    toast.set_halign(gtk::Align::Center);
    toast.set_valign(gtk::Align::End);
    toast.set_margin_bottom(56);
    overlay.add_overlay(&toast);
    let show_toast: Rc<dyn Fn(&str)> = {
        let (toast, toast_label) = (toast.clone(), toast_label.clone());
        let gen = Rc::new(Cell::new(0u32));
        Rc::new(move |m: &str| {
            toast_label.set_text(m);
            toast.set_reveal_child(true);
            gen.set(gen.get() + 1);
            let (toast, gen2, mine) = (toast.clone(), gen.clone(), gen.get());
            glib::timeout_add_seconds_local_once(4, move || {
                if gen2.get() == mine {
                    toast.set_reveal_child(false);
                }
            });
        })
    };
    win.set_child(Some(&overlay));

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    let tabs = hbox(8);
    tabs.set_margin_start(24);
    tabs.set_margin_end(24);
    tabs.set_margin_top(16);
    let mut tab_btns: Vec<gtk::ToggleButton> = vec![];
    for (id, name) in [("overview", "OVERVIEW"), ("tune", "TUNE"), ("profiles", "PROFILES"), ("settings", "SETTINGS")] {
        let b = gtk::ToggleButton::with_label(name);
        b.add_css_class("key");
        if let Some(first) = tab_btns.first() {
            b.set_group(Some(first));
        }
        let st = stack.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                st.set_visible_child_name(id);
            }
        });
        tabs.append(&b);
        tab_btns.push(b);
    }
    root.append(&tabs);
    root.append(&stack);

    let page = |child: &gtk::Box| -> gtk::ScrolledWindow {
        child.set_margin_top(24);
        child.set_margin_bottom(24);
        child.set_margin_start(24);
        child.set_margin_end(24);
        let sw = gtk::ScrolledWindow::new();
        // content that cannot fit scrolls sideways instead of forcing the window wider
        sw.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        sw.set_child(Some(child));
        sw
    };

    // ======================= Overview
    let pending: Rc<RefCell<(f64, f64, f64)>> = {
        let l = current();
        Rc::new(RefCell::new((l.pl1.clamp(15.0, 65.0), l.pl2.clamp(15.0, 109.0), if l.gpu >= 10.0 { l.gpu } else { 30.0 })))
    };
    let dial_state = Rc::new(Cell::new((0.5f64, 0.6f64)));
    let temps = Rc::new(Cell::new((0.0f64, 0.0f64)));

    let overview = vbox(0);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 24);
    overview.append(&body);

    let dial_panel = panel();
    dial_panel.set_valign(gtk::Align::Start);
    dial_panel.append(&label("EXHIBIT 1 · POWER BUDGET", &["eyebrow"]));
    let dial_overlay = gtk::Overlay::new();
    let dial = {
        let ds = dial_state.clone();
        look.drawing(230, 230, move |cr, w, h, p| {
            let (c, g) = ds.get();
            rings(cr, w, h, c, g, (p.track, 1.0), p.accent, p.blue);
        })
    };
    dial_overlay.set_child(Some(&dial));
    let center = vbox(0);
    center.set_halign(gtk::Align::Center);
    center.set_valign(gtk::Align::Center);
    let big = hbox(4);
    big.set_halign(gtk::Align::Center);
    let big_num = label("–", &["big-num"]);
    big.append(&big_num);
    let unit = label("W", &["big-unit"]);
    unit.set_valign(gtk::Align::End);
    unit.set_margin_bottom(14);
    big.append(&unit);
    center.append(&big);
    let cap = label("TOTAL BUDGET", &["eyebrow"]);
    cap.set_halign(gtk::Align::Center);
    center.append(&cap);
    dial_overlay.add_overlay(&center);
    let af = gtk::AspectFrame::new(0.5, 0.5, 1.0, false);
    af.set_child(Some(&dial_overlay));
    af.set_margin_top(14);
    af.set_size_request(230, 230);
    dial_panel.append(&af);
    let legend = hbox(18);
    legend.set_halign(gtk::Align::Center);
    legend.set_margin_top(14);
    legend.append(&label("● CPU", &["accent"]));
    let gpu_leg = label("● GPU", &["blue"]);
    legend.append(&gpu_leg);
    dial_panel.append(&legend);
    body.append(&dial_panel);

    let right = vbox(0);
    right.set_hexpand(true);
    right.append(&label("LIMITS APPLIED", &["eyebrow"]));
    let r1 = rule();
    r1.set_margin_top(8);
    right.append(&r1);
    let rows = vbox(10);
    rows.set_margin_top(14);
    let (row_c1, val_c1) = leader_row("CPU sustained");
    let (row_c2, val_c2) = leader_row("CPU burst");
    let (row_g, val_g) = leader_row("GPU");
    rows.append(&row_c1);
    rows.append(&row_c2);
    rows.append(&row_g);
    let tr = hbox(0);
    tr.add_css_class("total-rule");
    rows.append(&tr);
    let (row_t, val_t) = leader_row("Total");
    val_t.add_css_class("accent");
    rows.append(&row_t);
    right.append(&rows);
    let gpu_note = wrapped("The driver doesn't allow changing the GPU limit on this laptop, so only the CPU limit is applied.", &["bad", "sans"]);
    gpu_note.set_margin_top(10);
    gpu_note.set_visible(false);
    right.append(&gpu_note);

    let th = label("THERMALS", &["eyebrow"]);
    th.set_margin_top(22);
    right.append(&th);
    let r2 = rule();
    r2.set_margin_top(8);
    right.append(&r2);
    let fr = Rc::new(Cell::new((0.0f64, 0.0f64)));
    let thermals = vbox(12);
    thermals.set_margin_top(14);
    let mk_meter = |name: &str, pick: fn((f64, f64)) -> f64| -> (gtk::Box, gtk::Label) {
        let row = hbox(12);
        let n = label(name, &[]);
        n.set_width_chars(4);
        row.append(&n);
        let fr = fr.clone();
        let m = look.drawing(120, 16, move |cr, w, h, p| {
            let f = pick(fr.get());
            meter(cr, w, h, f, p, f >= 0.85);
        });
        m.set_hexpand(true);
        m.set_valign(gtk::Align::Center);
        row.append(&m);
        let v = label("–", &["val"]);
        v.set_width_chars(5);
        v.set_xalign(1.0);
        row.append(&v);
        (row, v)
    };
    let (row_ct, val_ct) = mk_meter("CPU", |f| f.0);
    let (row_gt, val_gt) = mk_meter("GPU", |f| f.1);
    thermals.append(&row_ct);
    thermals.append(&row_gt);
    right.append(&thermals);

    // presets + custom + apply
    let controls = vbox(12);
    controls.set_margin_top(22);
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_homogeneous(false);
    flow.set_column_spacing(10);
    flow.set_row_spacing(4);
    flow.set_min_children_per_line(1);
    flow.set_max_children_per_line(5);
    let guard = Rc::new(Cell::new(false));
    let sel: Rc<Cell<usize>> = Rc::new(Cell::new(9));
    let stepper_rev = gtk::Revealer::new();
    let step_labels: Rc<RefCell<Vec<gtk::Label>>> = Rc::new(RefCell::new(vec![]));
    let sync_steppers = {
        let (pending, step_labels) = (pending.clone(), step_labels.clone());
        move || {
            let p = *pending.borrow();
            for (l, v) in step_labels.borrow().iter().zip([p.0, p.1, p.2]) {
                l.set_text(&format!("{v:.0} W"));
            }
        }
    };
    let mut preset_btns: Vec<gtk::ToggleButton> = vec![];
    let mut glyphs: Vec<gtk::DrawingArea> = vec![];
    for (i, (name, a, _b, c)) in PRESETS.iter().enumerate() {
        let btn = gtk::ToggleButton::new();
        btn.add_css_class("key");
        let inner = hbox(9);
        let (cf, gf) = (*a / 65.0, *c / 45.0);
        let sel2 = sel.clone();
        let g = look.drawing(30, 30, move |cr, w, h, p| {
            let on = sel2.get() == i;
            let (tr, c1, c2) = if on { (("#000000", 0.25), p.accent_ink, p.accent_ink) } else { ((p.track, 1.0), p.accent, p.blue) };
            rings(cr, w, h, cf, gf, tr, c1, c2);
        });
        g.set_valign(gtk::Align::Center);
        inner.append(&g);
        let t = vbox(1);
        t.append(&label(name, &[]));
        let w = label(&format!("{a:.0} / {c:.0} W"), &["mut"]);
        t.append(&w);
        inner.append(&t);
        btn.set_child(Some(&inner));
        if let Some(first) = preset_btns.first() {
            btn.set_group(Some(first));
        }
        flow.insert(&btn, -1);
        preset_btns.push(btn);
        glyphs.push(g);
    }
    let custom_btn = gtk::ToggleButton::with_label("CUSTOM");
    custom_btn.add_css_class("key");
    custom_btn.set_group(Some(&preset_btns[0]));
    flow.insert(&custom_btn, -1);
    let apply_btn = key("APPLY");
    apply_btn.add_css_class("on");
    flow.insert(&apply_btn, -1);
    controls.append(&flow);

    {
        // custom steppers
        let sbox = vbox(8);
        sbox.set_margin_top(6);
        let defs: [(&str, f64, f64); 3] = [("CPU sustained", 15.0, 65.0), ("CPU burst", 15.0, 109.0), ("GPU", 10.0, 45.0)];
        for (idx, (name, lo, hi)) in defs.into_iter().enumerate() {
            let row = hbox(10);
            let n = label(name, &[]);
            n.set_hexpand(true);
            row.append(&n);
            let minus = key("−");
            let plus = key("+");
            let v = label("", &["stepper-val"]);
            v.set_xalign(0.5);
            step_labels.borrow_mut().push(v.clone());
            for (btn, d) in [(&minus, -1.0), (&plus, 1.0)] {
                let (pending, sync, sel, glyphs, custom) = (pending.clone(), sync_steppers.clone(), sel.clone(), glyphs.clone(), custom_btn.clone());
                let _ = (&sel, &glyphs);
                btn.connect_clicked(move |_| {
                    let mut p = pending.borrow_mut();
                    let slot = match idx { 0 => &mut p.0, 1 => &mut p.1, _ => &mut p.2 };
                    *slot = (*slot + d).clamp(lo, hi);
                    if p.1 < p.0 {
                        p.1 = p.0;
                    }
                    drop(p);
                    sync();
                    custom.set_active(true);
                });
            }
            row.append(&minus);
            row.append(&v);
            row.append(&plus);
            sbox.append(&row);
        }
        stepper_rev.set_child(Some(&sbox));
        stepper_rev.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        controls.append(&stepper_rev);
    }
    sync_steppers();

    for (i, b) in preset_btns.iter().enumerate() {
        let (pending, sync, sel, guard, glyphs, rev) = (pending.clone(), sync_steppers.clone(), sel.clone(), guard.clone(), glyphs.clone(), stepper_rev.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                sel.set(i);
                if !guard.get() {
                    *pending.borrow_mut() = (PRESETS[i].1, PRESETS[i].2, PRESETS[i].3);
                    sync();
                }
                rev.set_reveal_child(false);
            }
            for g in &glyphs {
                g.queue_draw();
            }
        });
    }
    {
        let (sel, glyphs, rev) = (sel.clone(), glyphs.clone(), stepper_rev.clone());
        custom_btn.connect_toggled(move |b| {
            if b.is_active() {
                sel.set(9);
                rev.set_reveal_child(true);
            }
            for g in &glyphs {
                g.queue_draw();
            }
        });
    }
    let keep = gtk::Switch::new();
    keep.set_active(std::path::Path::new("/etc/udev/rules.d/99-powerlimit.rules").exists());
    let keep_row = setting_row("Keep after restart", "Re-applies the CPU limits at startup. The GPU limit isn't kept.", &keep);
    keep_row.set_margin_top(4);
    controls.append(&keep_row);
    {
        let (tx, pending, keep) = (tx.clone(), pending.clone(), keep.clone());
        apply_btn.connect_clicked(move |_| {
            let p = *pending.borrow();
            let a = vec!["set".to_string(), p.0.to_string(), p.1.max(p.0).to_string(), p.2.to_string(),
                if keep.is_active() { "--persist" } else { "--no-persist" }.to_string()];
            privileged(a, tx.clone());
        });
    }
    let reset = key("RESET TO DELL DEFAULTS");
    reset.add_css_class("danger");
    reset.set_halign(gtk::Align::Start);
    {
        let tx = tx.clone();
        reset.connect_clicked(move |_| privileged(vec!["reset".into()], tx.clone()));
    }
    controls.append(&reset);
    right.append(&controls);
    body.append(&right);
    stack.add_named(&page(&overview), Some("overview"));

    // responsive: dial beside the tables when wide, stacked when narrow
    {
        let dp = dial_panel.clone();
        let wide = Cell::new(false);
        body.add_tick_callback(move |b, _| {
            let w_now = b.width();
            let want = w_now >= 700;
            if want != wide.get() {
                wide.set(want);
                b.set_orientation(if want { gtk::Orientation::Horizontal } else { gtk::Orientation::Vertical });
                dp.set_size_request(if want { 330 } else { -1 }, -1);
                dp.set_hexpand(!want);
            }
            glib::ControlFlow::Continue
        });
        dial_panel.set_hexpand(true);
    }

    // ======================= Tune
    let tune_stack = gtk::Stack::new();
    let idle = vbox(14);
    let hero = panel();
    hero.append(&label("EXHIBIT 4 · TUNER", &["eyebrow"]));
    let h = wrapped("Tune for a game", &["hero"]);
    h.set_margin_top(8);
    hero.append(&h);
    let blurb = wrapped("Start your game and get into real gameplay, then press START. The tuner tries different CPU and GPU limits, watches clock speeds, temperatures and throttling, and saves the best setting for that game.", &["mut", "sans"]);
    blurb.set_margin_top(8);
    hero.append(&blurb);
    idle.append(&hero);

    let opts = panel();
    let game = gtk::DropDown::from_strings(&["Auto-detect"]);
    game.set_hexpand(true);
    let game_ids: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(vec!["auto".into()]));
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.add_css_class("quiet");
    reload.set_tooltip_text(Some("Rescan running games"));
    let game_box = hbox(4);
    game_box.append(&game);
    game_box.append(&reload);
    let fill = {
        let (game, game_ids) = (game.clone(), game_ids.clone());
        move || {
            let mut names = vec!["Auto-detect".to_string()];
            let mut ids = vec!["auto".to_string()];
            for (pid, name) in candidates() {
                names.push(format!("{name} · pid {pid}"));
                ids.push(pid.to_string());
            }
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            game.set_model(Some(&gtk::StringList::new(&refs)));
            game.set_selected(0);
            *game_ids.borrow_mut() = ids;
        }
    };
    fill();
    {
        let fill = fill.clone();
        reload.connect_clicked(move |_| fill());
    }
    let minutes = spin(6.0, 240.0, cfg.borrow().minutes);
    let cmax = spin(60.0, 100.0, cfg.borrow().cpu_max);
    let gmax = spin(60.0, 95.0, cfg.borrow().gpu_max);
    opts.append(&setting_row("Game", "Pick the running game, or auto-detect.", &game_box));
    for (t, s, w) in [("Duration", "Minutes. Longer gives steadier readings.", &minutes), ("CPU temperature limit", "°C. Never go hotter than this.", &cmax), ("GPU temperature limit", "°C", &gmax)] {
        let sep = rule();
        sep.set_margin_top(10);
        sep.set_margin_bottom(10);
        opts.append(&sep);
        opts.append(&setting_row(t, s, w));
    }
    idle.append(&opts);
    let result = wrapped("", &["good"]);
    idle.append(&result);
    let start = key("START TUNING");
    start.add_css_class("on");
    start.set_halign(gtk::Align::Start);
    idle.append(&start);
    tune_stack.add_named(&idle, Some("idle"));

    let running = vbox(14);
    running.append(&label("EXHIBIT 4 · TUNING RUN", &["eyebrow"]));
    let head_row = hbox(8);
    let phase = wrapped("Starting", &["hero"]);
    phase.set_hexpand(false);
    head_row.append(&phase);
    let cur = hbox(0);
    cur.add_css_class("cursor");
    cur.set_valign(gtk::Align::Center);
    head_row.append(&cur);
    running.append(&head_row);
    let trial = label("", &["mut"]);
    running.append(&trial);
    let detail = wrapped("Keep playing normally. Nothing is saved until the run finishes.", &["mut", "sans"]);
    running.append(&detail);
    let prog = Rc::new(Cell::new(0.0f64));
    let bar = {
        let prog = prog.clone();
        look.drawing(100, 22, move |cr, w, h, p| progress(cr, w, h, prog.get(), p))
    };
    bar.set_hexpand(true);
    running.append(&bar);
    let trials_panel = panel();
    let tgrid = gtk::Grid::new();
    tgrid.set_column_spacing(14);
    trials_panel.append(&tgrid);
    running.append(&trials_panel);
    let stop = key("STOP RUN");
    stop.add_css_class("danger");
    stop.set_halign(gtk::Align::Start);
    running.append(&stop);
    let log = gtk::TextView::new();
    log.set_editable(false);
    log.set_cursor_visible(false);
    log.add_css_class("log");
    log.set_wrap_mode(gtk::WrapMode::WordChar);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.add_css_class("log-frame");
    log_scroll.set_min_content_height(160);
    log_scroll.set_child(Some(&log));
    let expander = gtk::Expander::new(Some("DETAILS"));
    expander.set_child(Some(&log_scroll));
    running.append(&expander);
    tune_stack.add_named(&running, Some("running"));
    let tune_page = vbox(0);
    tune_page.append(&tune_stack);
    stack.add_named(&page(&tune_page), Some("tune"));

    let trials: Rc<RefCell<Vec<(u32, String, String, String, String)>>> = Rc::new(RefCell::new(vec![]));
    let render_trials = {
        let (trials, tgrid) = (trials.clone(), tgrid.clone());
        move || {
            while let Some(c) = tgrid.first_child() {
                tgrid.remove(&c);
            }
            for (col, t) in ["#", "CPU / GPU W", "CLOCKS GHz", "PEAK °C", "RESULT"].iter().enumerate() {
                let l = label(t, &["th"]);
                l.set_margin_bottom(8);
                l.set_hexpand(col > 0 && col < 4);
                tgrid.attach(&l, col as i32, 0, 1, 1);
            }
            let sep0 = hbox(0);
            sep0.add_css_class("heavy-rule");
            tgrid.attach(&sep0, 0, 1, 5, 1);
            for (i, (n, w, c, t, r)) in trials.borrow().iter().enumerate() {
                let row = 2 + 2 * i as i32;
                let cls = match r.as_str() { "STABLE" => "good", "MEASURING" => "accent", _ => "bad" };
                for (col, (txt, classes)) in [(n.to_string(), vec!["val"]), (w.clone(), vec![]), (c.clone(), vec![]), (t.clone(), vec![]), (r.clone(), vec![cls, "val"])].into_iter().enumerate() {
                    let l = label(&txt, &classes);
                    l.set_margin_top(9);
                    l.set_margin_bottom(9);
                    l.set_hexpand(col > 0 && col < 4);
                    tgrid.attach(&l, col as i32, row, 1, 1);
                }
                let sep = hbox(0);
                sep.add_css_class("leader");
                sep.set_margin_bottom(0);
                tgrid.attach(&sep, 0, row + 1, 5, 1);
            }
        }
    };
    render_trials();

    {
        let (tx, stdin, tune_stack) = (tx.clone(), stdin.clone(), tune_stack.clone());
        let (game, game_ids, minutes, cmax, gmax) = (game.clone(), game_ids.clone(), minutes.clone(), cmax.clone(), gmax.clone());
        let (phase, trial, detail, bar, log, result, prog, trials, render) =
            (phase.clone(), trial.clone(), detail.clone(), bar.clone(), log.clone(), result.clone(), prog.clone(), trials.clone(), render_trials.clone());
        start.connect_clicked(move |_| {
            let mut cmd = pk();
            cmd.args(["tune", "--stdin-stop", "--minutes", &minutes.value().to_string(),
                "--cpu-max", &cmax.value().to_string(), "--gpu-max", &gmax.value().to_string()]);
            if let Some(pid) = game_ids.borrow().get(game.selected() as usize).filter(|id| id.as_str() != "auto") {
                cmd.args(["--pid", pid.as_str()]);
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            match cmd.spawn() {
                Ok(mut child) => {
                    *stdin.borrow_mut() = child.stdin.take();
                    let out = child.stdout.take().unwrap();
                    let err = child.stderr.take().unwrap();
                    let t = tx.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(err).lines().map_while(Result::ok) {
                            let _ = t.send(Msg::Log(l));
                        }
                    });
                    let t = tx.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(out).lines().map_while(Result::ok) {
                            let _ = t.send(Msg::Log(l));
                        }
                        let _ = child.wait();
                        let _ = t.send(Msg::TuneDone);
                    });
                    log.buffer().set_text("");
                    result.set_text("");
                    phase.set_text("Starting");
                    trial.set_text("");
                    detail.set_text("Keep playing normally. Nothing is saved until the run finishes.");
                    prog.set(0.0);
                    bar.queue_draw();
                    trials.borrow_mut().clear();
                    render();
                    tune_stack.set_visible_child_name("running");
                }
                Err(e) => {
                    let _ = tx.send(Msg::Log(format!("Could not start pkexec: {e}")));
                }
            }
        });
    }
    let stop_tune = {
        let stdin = stdin.clone();
        move || {
            if let Some(s) = stdin.borrow_mut().as_mut() {
                let _ = s.write_all(b"stop\n");
                let _ = s.flush();
            }
        }
    };
    {
        let f = stop_tune.clone();
        stop.connect_clicked(move |b| {
            b.set_sensitive(false);
            f();
        });
    }

    // ======================= Profiles
    let profiles_page = vbox(14);
    profiles_page.append(&label("EXHIBIT 3", &["eyebrow"]));
    let ph = wrapped("Tuned game profiles", &["hero"]);
    profiles_page.append(&ph);
    let auto_row = hbox(12);
    let auto_text = vbox(2);
    auto_text.set_hexpand(true);
    auto_text.append(&wrapped("Switch automatically", &[]));
    let auto_status = wrapped("When you open a tuned game, its limits are applied, then restored when you close it.", &["mut", "sans"]);
    auto_text.append(&auto_status);
    auto_row.append(&auto_text);
    let auto_on = gtk::ToggleButton::with_label("ON");
    auto_on.add_css_class("key");
    let auto_off = gtk::ToggleButton::with_label("OFF");
    auto_off.add_css_class("key");
    auto_off.set_group(Some(&auto_on));
    auto_off.set_active(true);
    let auto_keys = hbox(8);
    auto_keys.set_valign(gtk::Align::Center);
    auto_keys.append(&auto_on);
    auto_keys.append(&auto_off);
    auto_row.append(&auto_keys);
    profiles_page.append(&auto_row);
    let profiles = vbox(12);
    profiles_page.append(&profiles);
    stack.add_named(&page(&profiles_page), Some("profiles"));
    {
        let (tx, watcher) = (tx.clone(), watcher.clone());
        auto_on.connect_toggled(move |c| {
            if c.is_active() {
                let spawned = pk().args(["watch", "--stdin-stop"])
                    .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
                match spawned {
                    Ok(mut child) => {
                        *watcher.borrow_mut() = child.stdin.take();
                        let out = child.stdout.take().unwrap();
                        let t = tx.clone();
                        std::thread::spawn(move || {
                            for l in BufReader::new(out).lines().map_while(Result::ok) {
                                let _ = t.send(Msg::Watch(l));
                            }
                            let _ = child.wait();
                            let _ = t.send(Msg::WatchEnded);
                        });
                    }
                    Err(e) => { let _ = tx.send(Msg::Watch(format!("Could not start pkexec: {e}"))); }
                }
            } else if let Some(mut s) = watcher.borrow_mut().take() {
                let _ = s.write_all(b"stop\n");
                let _ = s.flush();
            }
        });
    }

    // ======================= status bar
    let status = hbox(10);
    status.add_css_class("status-bar");
    let sdot = hbox(0);
    sdot.add_css_class("dot");
    sdot.set_valign(gtk::Align::Center);
    status.append(&sdot);
    status.append(&label("READY", &[]));
    status.append(&label("·", &[]));
    let applied_lbl = label("", &[]);
    applied_lbl.set_ellipsize(pango::EllipsizeMode::End);
    applied_lbl.set_max_width_chars(44);
    status.append(&applied_lbl);
    let scur = hbox(0);
    scur.add_css_class("cursor-sm");
    scur.set_valign(gtk::Align::Center);
    status.append(&scur);
    let spacer = hbox(0);
    spacer.set_hexpand(true);
    status.append(&spacer);
    let temps_lbl = label("", &[]);
    status.append(&temps_lbl);
    root.append(&status);

    // window close: stop children
    {
        let watcher_close = watcher.clone();
        win.connect_close_request(move |_| {
            stop_tune();
            if let Some(mut s) = watcher_close.borrow_mut().take() {
                let _ = s.write_all(b"stop\n");
                let _ = s.flush();
            }
            glib::Propagation::Proceed
        });
    }

    // ======================= refresh: limits every second (cheap), sensors every two
    let limits_tick: Rc<dyn Fn()> = {
        let (tx2, profiles, dial_state, dial, big_num) = (tx.clone(), profiles.clone(), dial_state.clone(), dial.clone(), big_num.clone());
        let (val_c1, val_c2, val_g, val_t, applied_lbl, gpu_note) =
            (val_c1.clone(), val_c2.clone(), val_g.clone(), val_t.clone(), applied_lbl.clone(), gpu_note.clone());
        let last_key = RefCell::new(String::new());
        Rc::new(move || {
            let l = current();
            val_c1.set_text(&format!("{:.0} W", l.pl1));
            val_c2.set_text(&format!("{:.0} W", l.pl2));
            val_g.set_text(&if l.gpu > 0.0 { format!("{:.0} W", l.gpu) } else { "unknown".into() });
            val_t.set_text(&format!("{:.0} W", l.pl1 + l.gpu));
            big_num.set_text(&format!("{:.0}", l.pl1 + l.gpu));
            dial_state.set((l.pl1 / 65.0, l.gpu / 45.0));
            dial.queue_draw();
            applied_lbl.set_text(&format!("Applied: {}", applied_name(&l)));
            gpu_note.set_visible(matches!(last_applied(), Some((_, false))));

            let mut p: Vec<_> = load_profiles().into_iter().collect();
            p.sort_by(|a, b| a.0.cmp(&b.0));
            let sig = format!("{p:?}|{}/{}/{}", l.pl1, l.pl2, l.gpu);
            if sig == *last_key.borrow() {
                return;
            }
            *last_key.borrow_mut() = sig;
            while let Some(c) = profiles.first_child() {
                profiles.remove(&c);
            }
            if p.is_empty() {
                let b = panel();
                b.append(&label("No profiles yet", &["val"]));
                b.append(&wrapped("Open a game and run Tune. The best setting is saved here, one per game.", &["mut", "sans"]));
                profiles.append(&b);
            }
            for (name, v) in &p {
                let active = (l.pl1 - v.pl1).abs() < 0.5 && (l.pl2 - v.pl2).abs() < 0.5 && (l.gpu - v.gpu).abs() < 0.5;
                let card = hbox(16);
                card.add_css_class("row-card");
                if active {
                    card.add_css_class("active");
                }
                let (cf, gf) = (v.pl1 / 65.0, v.gpu / 45.0);
                let glyph = gtk::DrawingArea::new();
                glyph.set_content_width(48);
                glyph.set_content_height(48);
                let colors = if active { ("#000000", 0.0) } else { ("", 0.0) };
                let _ = colors;
                // profile glyphs are rebuilt with the list, so read the live palette through the window's style
                glyph.set_draw_func(move |w, cr, ww, hh| {
                    let dark = {
                        #[allow(deprecated)]
                        let c = w.style_context().color();
                        (c.red() + c.green() + c.blue()) / 3.0 > 0.5
                    };
                    let p = if dark { DARK } else { SEPIA };
                    rings(cr, ww as f64, hh as f64, cf, gf, (p.track, 1.0), p.accent, p.blue);
                });
                glyph.set_valign(gtk::Align::Center);
                card.append(&glyph);
                let text = vbox(3);
                text.set_hexpand(true);
                let top = hbox(10);
                top.append(&wrapped(name, &["val"]));
                if active {
                    top.append(&label("IN USE", &["chip"]));
                }
                text.append(&top);
                text.append(&wrapped(&format!("CPU {:.0} W · GPU {:.0} W", v.pl1, v.gpu), &["mut"]));
                card.append(&text);
                let apply = key(if active { "APPLIED" } else { "APPLY" });
                apply.set_sensitive(!active);
                apply.set_valign(gtk::Align::Center);
                let a = vec!["set".into(), v.pl1.to_string(), v.pl2.to_string(), v.gpu.to_string()];
                let t = tx2.clone();
                apply.connect_clicked(move |_| privileged(a.clone(), t.clone()));
                card.append(&apply);
                let del = gtk::Button::from_icon_name("user-trash-symbolic");
                del.add_css_class("quiet");
                del.set_valign(gtk::Align::Center);
                del.set_tooltip_text(Some("Forget this profile"));
                let (name, t) = (name.clone(), tx2.clone());
                del.connect_clicked(move |_| privileged(vec!["forget".into(), name.clone()], t.clone()));
                card.append(&del);
                profiles.append(&card);
            }
        })
    };
    limits_tick();
    {
        let t = limits_tick.clone();
        glib::timeout_add_seconds_local(1, move || {
            t();
            glib::ControlFlow::Continue
        });
    }
    let sensors_tick = {
        let (fr, row_ct, row_gt, val_ct, val_gt, temps_lbl, temps) = (fr.clone(), row_ct.clone(), row_gt.clone(), val_ct.clone(), val_gt.clone(), temps_lbl.clone(), temps.clone());
        let _ = (&row_ct, &row_gt);
        move || {
            let s = sample();
            fr.set((s.cpu_t / 100.0, s.gpu_t / 100.0));
            temps.set((s.cpu_t, s.gpu_t));
            val_ct.set_text(&format!("{:.0}°", s.cpu_t));
            val_gt.set_text(&format!("{:.0}°", s.gpu_t));
            temps_lbl.set_text(&format!("CPU {:.0}° · GPU {:.0}°", s.cpu_t, s.gpu_t));
            row_ct.queue_draw();
            row_gt.queue_draw();
            glib::ControlFlow::Continue
        }
    };
    {
        let st = sensors_tick.clone();
        st();
        let look2 = look.clone();
        glib::timeout_add_seconds_local(2, move || {
            let r = st();
            for d in look2.draws.borrow().iter() {
                d.queue_draw();
            }
            r
        });
    }

    // ======================= Settings
    let (tray_tx, tray_rx) = mpsc::channel::<crate::tray::Cmd>();
    let tray_h: Rc<RefCell<Option<crate::tray::TrayHandle>>> = Rc::new(RefCell::new(None));
    let start_tray: Rc<dyn Fn() -> bool> = {
        let tray_h = tray_h.clone();
        Rc::new(move || {
            if tray_h.borrow().is_some() {
                return true;
            }
            let names = PRESETS.iter().map(|(n, a, b, c)| format!("{} · {:.0}/{:.0} W · GPU {:.0} W", title_case(n), a, b, c)).collect();
            match crate::tray::spawn(tray_tx.clone(), names) {
                Some(t) => {
                    *tray_h.borrow_mut() = Some(t);
                    true
                }
                None => false,
            }
        })
    };
    let tray_tick: Rc<dyn Fn()> = {
        let (tray_h, temps) = (tray_h.clone(), temps.clone());
        Rc::new(move || {
            if let Some(t) = tray_h.borrow().as_ref() {
                let l = current();
                let (ct, gt) = temps.get();
                let hot = ct.max(gt);
                t.update(
                    tray_icon(l.pl1 / 65.0, hot),
                    format!("Power Limiter · {}", applied_name(&l)),
                    format!("CPU {:.0}/{:.0} W · GPU {:.0} W · {:.0}° / {:.0}°", l.pl1, l.pl2, l.gpu, ct, gt),
                );
            }
        })
    };

    let settings = vbox(24);
    let settings_head = vbox(4);
    settings_head.append(&label("PREFERENCES", &["eyebrow"]));
    settings_head.append(&wrapped("Settings", &["hero"]));
    settings.append(&settings_head);

    // appearance: theme swatches and font samples
    let theme_row = vbox(10);
    theme_row.append(&label("Theme", &[]));
    let themes = gtk::FlowBox::new();
    themes.set_selection_mode(gtk::SelectionMode::None);
    themes.set_max_children_per_line(3);
    themes.set_column_spacing(12);
    themes.set_row_spacing(12);
    themes.set_homogeneous(true);
    let mut first_theme: Option<gtk::ToggleButton> = None;
    for (id, name) in [("auto", "Match system"), ("sepia", "Sepia"), ("dark", "Monokai Pro")] {
        let b = gtk::ToggleButton::new();
        b.add_css_class("choice");
        let v = vbox(8);
        let sw = gtk::DrawingArea::new();
        sw.set_content_width(120);
        sw.set_content_height(64);
        sw.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            if id == "auto" {
                swatch(cr, w, h, &SEPIA);
                cr.rectangle(w / 2.0, 0.0, w / 2.0, h);
                cr.clip();
                swatch(cr, w, h, &DARK);
            } else {
                swatch(cr, w, h, if id == "dark" { &DARK } else { &SEPIA });
            }
        });
        v.append(&sw);
        v.append(&label(name, &["choice-name"]));
        b.set_child(Some(&v));
        if let Some(f) = &first_theme {
            b.set_group(Some(f));
        } else {
            first_theme = Some(b.clone());
        }
        b.set_active(cfg.borrow().theme == id);
        let (cfg, rs) = (cfg.clone(), restyle.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                cfg.borrow_mut().theme = id.to_string();
                rs();
            }
        });
        themes.insert(&b, -1);
    }
    theme_row.append(&themes);

    let font_row = vbox(10);
    font_row.append(&label("Typeface", &[]));
    let fonts = gtk::FlowBox::new();
    fonts.set_selection_mode(gtk::SelectionMode::None);
    fonts.set_max_children_per_line(3);
    fonts.set_column_spacing(12);
    fonts.set_row_spacing(12);
    fonts.set_homogeneous(true);
    let mut first_font: Option<gtk::ToggleButton> = None;
    for f in families.iter() {
        let b = gtk::ToggleButton::new();
        b.add_css_class("choice");
        let v = vbox(6);
        let sample_lbl = label("45 W · 4.2 GHz", &["choice-sample"]);
        let al = pango::AttrList::new();
        let mut fd = pango::FontDescription::new();
        fd.set_family(f);
        fd.set_size(15 * pango::SCALE);
        al.insert(pango::AttrFontDesc::new(&fd));
        sample_lbl.set_attributes(Some(&al));
        v.append(&sample_lbl);
        v.append(&label(f, &["choice-name"]));
        b.set_child(Some(&v));
        if let Some(first) = &first_font {
            b.set_group(Some(first));
        } else {
            first_font = Some(b.clone());
        }
        b.set_active(cfg.borrow().font == *f);
        let (cfg, rs, name) = (cfg.clone(), restyle.clone(), f.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                cfg.borrow_mut().font = name.clone();
                rs();
            }
        });
        fonts.insert(&b, -1);
    }
    font_row.append(&fonts);
    if !families.iter().any(|f| f == "IBM Plex Mono") {
        font_row.append(&wrapped("More typefaces: sudo apt install fonts-ibm-plex fonts-jetbrains-mono", &["mut", "sans"]));
    }
    settings.append(&group("APPEARANCE", &[&theme_row, &font_row]));

    // menu bar and login
    let tray_sw = gtk::Switch::new();
    tray_sw.set_active(cfg.borrow().tray);
    let close_sw = gtk::Switch::new();
    close_sw.set_active(cfg.borrow().close_to_tray);
    close_sw.set_sensitive(cfg.borrow().tray);
    let login_sw = gtk::Switch::new();
    login_sw.set_active(autostart_path().exists());
    let startup_dd = gtk::DropDown::from_strings(&["Leave as is", "Last applied limits", "Quiet", "Balanced", "Performance"]);
    const STARTUP_IDS: [&str; 5] = ["none", "last", "QUIET", "BALANCED", "PERFORM"];
    startup_dd.set_selected(STARTUP_IDS.iter().position(|i| *i == cfg.borrow().startup).unwrap_or(0) as u32);
    let watch_sw = gtk::Switch::new();
    watch_sw.set_active(cfg.borrow().login_watch);
    {
        let (cfg1, start_tray, tray_h, close_sw1, tray_tick, show_toast) = (cfg.clone(), start_tray.clone(), tray_h.clone(), close_sw.clone(), tray_tick.clone(), show_toast.clone());
        tray_sw.connect_active_notify(move |sw| {
            if sw.is_active() {
                if start_tray() {
                    tray_tick();
                } else {
                    show_toast("No menu-bar host found. Enable the AppIndicator extension.");
                    sw.set_active(false);
                    return;
                }
            } else if let Some(t) = tray_h.borrow_mut().take() {
                t.shutdown();
            }
            cfg1.borrow_mut().tray = sw.is_active();
            close_sw1.set_sensitive(sw.is_active());
            save_cfg(&cfg1.borrow());
        });
        let cfg2 = cfg.clone();
        close_sw.connect_active_notify(move |sw| {
            cfg2.borrow_mut().close_to_tray = sw.is_active();
            save_cfg(&cfg2.borrow());
        });
        login_sw.connect_active_notify(|sw| set_autostart(sw.is_active()));
        let cfg3 = cfg.clone();
        startup_dd.connect_selected_notify(move |d| {
            cfg3.borrow_mut().startup = STARTUP_IDS[(d.selected() as usize).min(4)].to_string();
            save_cfg(&cfg3.borrow());
        });
        let cfg4 = cfg.clone();
        watch_sw.connect_active_notify(move |sw| {
            cfg4.borrow_mut().login_watch = sw.is_active();
            save_cfg(&cfg4.borrow());
        });
    }
    settings.append(&group("MENU BAR AND LOGIN", &[
        &setting_row("Show in the menu bar", "A live ring shows your CPU budget and temperature. Click it to open the app.", &tray_sw),
        &setting_row("Keep running when the window is closed", "Closing the window hides it; the indicator stays.", &close_sw),
        &setting_row("Start at login", "Starts quietly in the menu bar.", &login_sw),
        &setting_row("Apply limits at login", "Asks for your password once per login, because limits need administrator rights.", &startup_dd),
        &setting_row("Switch profiles automatically at login", "Watches for tuned games and adjusts CPU and GPU as they start and stop.", &watch_sw),
    ]));

    // data
    let path_entry = gtk::Entry::new();
    path_entry.set_hexpand(true);
    path_entry.set_text(&std::env::var("POWERLIMIT_PROFILES").unwrap_or_else(|_| "/etc/powerlimit/profiles.conf".into()));
    let choose = key("CHOOSE");
    let default_btn = key("DEFAULT");
    let set_path: Rc<dyn Fn(&str)> = {
        let (cfg, entry, limits_tick, show_toast) = (cfg.clone(), path_entry.clone(), limits_tick.clone(), show_toast.clone());
        Rc::new(move |p: &str| {
            let p = p.trim();
            if p.is_empty() || p == "/etc/powerlimit/profiles.conf" {
                std::env::remove_var("POWERLIMIT_PROFILES");
                cfg.borrow_mut().profiles = String::new();
                entry.set_text("/etc/powerlimit/profiles.conf");
            } else {
                std::env::set_var("POWERLIMIT_PROFILES", p);
                cfg.borrow_mut().profiles = p.to_string();
                entry.set_text(p);
            }
            save_cfg(&cfg.borrow());
            limits_tick();
            show_toast("Profiles file updated");
        })
    };
    {
        let sp = set_path.clone();
        path_entry.connect_activate(move |e| sp(&e.text()));
        let sp = set_path.clone();
        default_btn.connect_clicked(move |_| sp(""));
        let (sp, win) = (set_path.clone(), win.clone());
        choose.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Profiles file");
            let sp = sp.clone();
            dialog.save(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                if let Some(path) = res.ok().and_then(|f| f.path()) {
                    sp(&path.to_string_lossy());
                }
            });
        });
    }
    let path_row = vbox(8);
    path_row.append(&wrapped("Profiles file", &[]));
    path_row.append(&wrapped("Where tuned game profiles are kept. Tuning writes here with administrator rights.", &["mut", "sans"]));
    let path_line = hbox(8);
    path_line.append(&path_entry);
    path_line.append(&choose);
    path_line.append(&default_btn);
    path_row.append(&path_line);

    let tune_defaults: Vec<gtk::SpinButton> = vec![
        spin(6.0, 240.0, cfg.borrow().minutes),
        spin(60.0, 100.0, cfg.borrow().cpu_max),
        spin(60.0, 95.0, cfg.borrow().gpu_max),
    ];
    for (i, (sp, target)) in tune_defaults.iter().zip([minutes.clone(), cmax.clone(), gmax.clone()]).enumerate() {
        let cfg = cfg.clone();
        sp.connect_value_changed(move |s| {
            target.set_value(s.value());
            let mut c = cfg.borrow_mut();
            match i {
                0 => c.minutes = s.value(),
                1 => c.cpu_max = s.value(),
                _ => c.gpu_max = s.value(),
            }
            save_cfg(&c);
        });
    }
    settings.append(&group("DATA AND TUNING", &[
        &path_row,
        &setting_row("Tuning duration", "Minutes. Longer gives steadier readings.", &tune_defaults[0]),
        &setting_row("CPU temperature limit", "°C. Tuning never goes hotter than this.", &tune_defaults[1]),
        &setting_row("GPU temperature limit", "°C", &tune_defaults[2]),
    ]));

    // maintenance
    let reset_btn = key("RESET");
    reset_btn.add_css_class("danger");
    {
        let tx = tx.clone();
        reset_btn.connect_clicked(move |_| privileged(vec!["reset".into()], tx.clone()));
    }
    let about = wrapped(&format!("Power Limiter {} · MIT licence · github.com/ogden-marrow/powerlimit", env!("CARGO_PKG_VERSION")), &["mut", "sans"]);
    settings.append(&group("MAINTENANCE", &[
        &setting_row("Dell defaults", "Restore 45 W sustained, 109 W burst, 40 W GPU, and forget saved reboot settings.", &reset_btn),
        &{
            let b = hbox(0);
            b.append(&about);
            b
        },
    ]));
    stack.add_named(&page(&settings), Some("settings"));

    // window close hides to the menu bar; tray clicks and menu items arrive here
    {
        let (cfg, tray_h) = (cfg.clone(), tray_h.clone());
        win.connect_close_request(move |w| {
            if cfg.borrow().close_to_tray && tray_h.borrow().is_some() {
                w.set_visible(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        let (win, app2, tx) = (win.clone(), app.clone(), tx.clone());
        glib::timeout_add_local(Duration::from_millis(250), move || {
            while let Ok(c) = tray_rx.try_recv() {
                match c {
                    crate::tray::Cmd::Open => {
                        win.set_visible(true);
                        win.present();
                    }
                    crate::tray::Cmd::Preset(i) => {
                        let (_, a, b, g) = PRESETS[i];
                        privileged(vec!["set".into(), a.to_string(), b.to_string(), g.to_string()], tx.clone());
                    }
                    crate::tray::Cmd::Reset => privileged(vec!["reset".into()], tx.clone()),
                    crate::tray::Cmd::Quit => app2.quit(),
                }
            }
            glib::ControlFlow::Continue
        });
        let w2 = watcher.clone();
        app.connect_shutdown(move |_| {
            if let Some(mut s) = w2.borrow_mut().take() {
                let _ = s.write_all(b"stop\n");
                let _ = s.flush();
            }
        });
        let tt = tray_tick.clone();
        glib::timeout_add_seconds_local(2, move || {
            tt();
            glib::ControlFlow::Continue
        });
    }
    if cfg.borrow().tray && start_tray() {
        tray_tick();
    }

    // ======================= messages from worker threads
    {
        let (log, bar, prog, stdin, tune_stack) = (log.clone(), bar.clone(), prog.clone(), stdin.clone(), tune_stack.clone());
        let (phase, trial, detail, result, stop) = (phase.clone(), trial.clone(), detail.clone(), result.clone(), stop.clone());
        let cfg_m = cfg.clone();
        let (auto_off, auto_status, watcher, show_toast, limits_tick) = (auto_off.clone(), auto_status.clone(), watcher.clone(), show_toast.clone(), limits_tick.clone());
        let (trials, render, pending, sync, guard, preset_btns, custom_btn) = (trials.clone(), render_trials.clone(), pending.clone(), sync_steppers.clone(), guard.clone(), preset_btns.clone(), custom_btn.clone());
        glib::timeout_add_local(Duration::from_millis(300), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Log(l) => {
                        let t = l.trim();
                        if let Some(rest) = t.strip_prefix("Trial ") {
                            if let Some((a, b)) = rest.split(':').next().and_then(|x| x.split_once('/')) {
                                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                                    prog.set((a - 1.0) / b.max(1.0));
                                    bar.queue_draw();
                                    trial.set_text(&format!("Trial {a:.0} of {b:.0}"));
                                    detail.set_text("Letting temperatures settle…");
                                    let n = nums(rest.split(':').nth(1).unwrap_or(""));
                                    if n.len() >= 3 {
                                        trials.borrow_mut().push((a as u32, format!("{:.0} / {:.0} W", n[0], n[2]), "—".into(), "—".into(), "MEASURING".into()));
                                        render();
                                    }
                                }
                            }
                        } else if t.starts_with("Phase 1") {
                            phase.set_text("Finding the thermal ceiling");
                        } else if t.starts_with("Phase 2") {
                            phase.set_text("Balancing CPU and GPU");
                            if let Some((_, rest)) = t.split_once("->") {
                                detail.set_text(rest.trim());
                            }
                        } else if t.starts_with("peak CPU") {
                            let n = nums(t);
                            if n.len() >= 6 {
                                if let Some(last) = trials.borrow_mut().last_mut() {
                                    last.2 = format!("{:.1} · {:.1}", n[3] / 1000.0, n[5] / 1000.0);
                                    last.3 = format!("{:.0}° / {:.0}°", n[0], n[1]);
                                    last.4 = if t.contains("THROTTLE") { "THROTTLED".into() } else { "STABLE".into() };
                                }
                                render();
                            }
                            detail.set_text(t);
                        } else if t.starts_with("throttling or over") {
                            if let Some(last) = trials.borrow_mut().last_mut() {
                                if last.4 == "STABLE" {
                                    last.4 = "TOO HOT".into();
                                }
                            }
                            render();
                        } else if t.starts_with("cooling") {
                            detail.set_text("Cooling down before the next trial…");
                        } else if t.starts_with("Best for") {
                            result.set_text(&format!("✓ {t}"));
                            prog.set(1.0);
                            bar.queue_draw();
                        } else if t.starts_with("Stopped") || t.starts_with("error") || t.starts_with("the game never") {
                            result.set_text(t);
                        }
                        let buf = log.buffer();
                        buf.insert(&mut buf.end_iter(), &format!("{l}\n"));
                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                        log.scroll_mark_onscreen(&mark);
                    }
                    Msg::TuneDone => {
                        *stdin.borrow_mut() = None;
                        stop.set_sensitive(true);
                        tune_stack.set_visible_child_name("idle");
                        if result.text().is_empty() {
                            result.set_text("Tuning ended without saving a profile.");
                        }
                        limits_tick();
                    }
                    Msg::Watch(l) => auto_status.set_text(&l),
                    Msg::WatchEnded => {
                        auto_status.set_text("When you open a tuned game, its limits are applied, then restored when you close it.");
                        *watcher.borrow_mut() = None;
                        auto_off.set_active(true);
                    }
                    Msg::Applied(Ok(())) => {
                        // show what is really applied, right away
                        limits_tick();
                        let l = current();
                        if l.gpu > 0.0 {
                            let mut c = cfg_m.borrow_mut();
                            c.last = format!("{},{},{}", l.pl1, l.pl2, l.gpu);
                            save_cfg(&c);
                        }
                        show_toast(&format!("Applied · CPU {:.0}/{:.0} W · GPU {:.0} W", l.pl1, l.pl2, l.gpu));
                        *pending.borrow_mut() = (l.pl1.clamp(15.0, 65.0), l.pl2.clamp(15.0, 109.0), if l.gpu >= 10.0 { l.gpu } else { pending.borrow().2 });
                        sync();
                        guard.set(true);
                        let idx = PRESETS.iter().position(|p| (l.pl1 - p.1).abs() < 0.5 && (l.pl2 - p.2).abs() < 0.5 && (l.gpu - p.3).abs() < 0.5);
                        match idx { Some(i) => preset_btns[i].set_active(true), None => custom_btn.set_active(true) }
                        guard.set(false);
                    }
                    Msg::Applied(Err(e)) => show_toast(&format!("Failed: {e}")),
                }
            }
            glib::ControlFlow::Continue
        });
    }
    tab_btns[0].set_active(true);
    if background {
        let c = cfg.borrow().clone();
        let args: Option<Vec<f64>> = match c.startup.as_str() {
            "none" => None,
            "last" => Some(c.last.split(',').filter_map(|v| v.parse().ok()).collect()).filter(|v: &Vec<f64>| v.len() == 3),
            id => PRESETS.iter().find(|p| p.0 == id).map(|p| vec![p.1, p.2, p.3]),
        };
        if let Some(a) = args {
            let mut v = vec!["set".to_string()];
            v.extend(a.iter().map(|x| x.to_string()));
            privileged(v, tx.clone());
        }
        if c.login_watch {
            auto_on.set_active(true);
        }
    }
    if !(background && tray_h.borrow().is_some()) {
        win.present();
    }

    // ======================= test harness (developer aid)
    // POWERLIMIT_SHOT=dir [POWERLIMIT_SHOT_SIZES=1100x760,700x600] [POWERLIMIT_SHOT_THEMES=dark,sepia] saves a PNG per
    // theme/size/page and reports the size the window actually got; POWERLIMIT_DEMO=1 fills the tuning page with sample data.
    if std::env::var("POWERLIMIT_DEMO").is_ok() {
        for l in ["Tuning for 'Demo Game' (pid 1), 30 min", "Phase 1: finding the thermal ceiling",
            "Trial 1/10: CPU 32/42 W, GPU 27 W - settling...", "  peak CPU 71°C, peak GPU 62°C | avg CPU 31.0 W / 4000 MHz, GPU 26.0 W / 1300 MHz, GPU util 97%",
            "Trial 2/10: CPU 41/51 W, GPU 35 W - settling...", "  peak CPU 79°C, peak GPU 68°C | avg CPU 40.0 W / 4300 MHz, GPU 34.0 W / 1500 MHz, GPU util 98%",
            "Trial 3/10: CPU 46/56 W, GPU 39 W - settling...", "  peak CPU 88°C, peak GPU 74°C | avg CPU 45.0 W / 3900 MHz, GPU 38.0 W / 1400 MHz, GPU util 98% | CPU THERMAL THROTTLE",
            "Trial 4/10: CPU 43/53 W, GPU 37 W - settling..."] {
            let _ = tx.send(Msg::Log(l.to_string()));
        }
        tune_stack.set_visible_child_name("running");
    }
    if let Ok(dir) = std::env::var("POWERLIMIT_SHOT") {
        let sizes: Vec<(i32, i32)> = std::env::var("POWERLIMIT_SHOT_SIZES").unwrap_or("980x760".into()).split(',')
            .filter_map(|s| s.split_once('x')).filter_map(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).collect();
        let themes: Vec<String> = std::env::var("POWERLIMIT_SHOT_THEMES").unwrap_or("dark".into()).split(',').map(str::to_string).collect();
        let mut steps: Vec<(String, i32, i32, &'static str)> = vec![];
        for t in &themes {
            for (w, h) in &sizes {
                // two settle steps give the compositor time to apply the new size before any capture
                steps.push((t.clone(), *w, *h, "set"));
                steps.push((t.clone(), *w, *h, "wait"));
                for p in ["overview", "tune", "profiles", "settings"] {
                    steps.push((t.clone(), *w, *h, p));
                }
            }
        }
        let steps = Rc::new(steps);
        let i = Rc::new(Cell::new(0usize));
        let (win2, tabs2, tk, rs) = (win.clone(), tab_btns.clone(), cfg.clone(), restyle.clone());
        glib::timeout_add_local(Duration::from_millis(2500), move || {
            let n = i.get();
            let Some((t, w, h, p)) = steps.get(n).cloned() else {
                if let Some(a) = win2.application() { a.quit(); }
                return glib::ControlFlow::Break;
            };
            i.set(n + 1);
            match p {
                "set" => {
                    tk.borrow_mut().theme = t.clone();
                    rs();
                    win2.set_default_size(w, h);
                    return glib::ControlFlow::Continue;
                }
                "wait" => {
                    // the layout may have switched to its narrow form meanwhile, which lowers the minimum: ask again
                    win2.set_default_size(w, h);
                    return glib::ControlFlow::Continue;
                }
                _ => {}
            }
            if let Some(b) = tabs2.get(["overview", "tune", "profiles", "settings"].iter().position(|x| *x == p).unwrap_or(0)) {
                b.set_active(true);
            }
            let (win3, dir) = (win2.clone(), dir.clone());
            glib::timeout_add_local_once(Duration::from_millis(1100), move || {
                let (aw, ah) = (win3.width(), win3.height());
                let note = if (aw, ah) != (w, h) { "  <-- size differs from request" } else { "" };
                eprintln!("{t} {w}x{h} {p}: window is {aw}x{ah}{note}");
                let paintable = gtk::WidgetPaintable::new(Some(&win3));
                let snap = gtk::Snapshot::new();
                paintable.snapshot(&snap, aw as f64, ah as f64);
                match (snap.to_node(), win3.native()) {
                    (Some(node), Some(native)) => {
                        if let Some(r) = native.renderer() {
                            if let Err(e) = r.render_texture(node, None).save_to_png(format!("{dir}/{t}_{w}x{h}_{p}.png")) {
                                eprintln!("  save failed: {e}");
                            }
                        }
                    }
                    (n, nat) => eprintln!("  no capture (node {}, native {})", n.is_some(), nat.is_some()),
                }
            });
            glib::ControlFlow::Continue
        });
    }
}

// ---------------------------------------------------------------- settings helpers

/// A titled card of settings rows separated by hairlines.
fn group(title: &str, rows: &[&gtk::Box]) -> gtk::Box {
    let p = panel();
    p.append(&label(title, &["eyebrow"]));
    for r in rows {
        let sep = rule();
        sep.add_css_class("hair");
        sep.set_margin_top(14);
        sep.set_margin_bottom(14);
        p.append(&sep);
        p.append(*r);
    }
    p
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()
}

/// Miniature window used as the theme picker thumbnail.
fn swatch(cr: &cairo::Context, w: f64, h: f64, p: &Palette) {
    rrect(cr, 0.0, 0.0, w, h, 10.0);
    hex(cr, p.bg, 1.0);
    let _ = cr.fill();
    rrect(cr, 10.0, 12.0, w - 20.0, h - 24.0, 7.0);
    hex(cr, p.panel, 1.0);
    let _ = cr.fill_preserve();
    hex(cr, p.line, 0.6);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    rrect(cr, 18.0, 20.0, 34.0, 6.0, 3.0);
    hex(cr, p.accent, 1.0);
    let _ = cr.fill();
    rrect(cr, 18.0, 32.0, w - 40.0, 4.0, 2.0);
    hex(cr, p.mut_, 0.8);
    let _ = cr.fill();
    rrect(cr, 18.0, 41.0, (w - 40.0) * 0.6, 4.0, 2.0);
    hex(cr, p.blue, 0.9);
    let _ = cr.fill();
}

/// Menu-bar glyph: a ring filled to the CPU budget, coloured by the hotter chip.
fn tray_icon(frac: f64, hot: f64) -> Vec<ksni::Icon> {
    const N: i32 = 32;
    let Ok(mut s) = cairo::ImageSurface::create(cairo::Format::ARgb32, N, N) else { return vec![] };
    {
        let Ok(cr) = cairo::Context::new(&s) else { return vec![] };
        let c = N as f64 / 2.0;
        cr.set_line_width(5.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.30);
        cr.arc(c, c, 12.0, 0.0, 2.0 * PI);
        let _ = cr.stroke();
        let col = if hot >= 88.0 { "#FF6188" } else if hot >= 78.0 { "#FFD866" } else { "#A9DC76" };
        hex(&cr, col, 1.0);
        cr.arc(c, c, 12.0, -PI / 2.0, -PI / 2.0 + 2.0 * PI * frac.clamp(0.05, 1.0));
        let _ = cr.stroke();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.arc(c, c, 3.0, 0.0, 2.0 * PI);
        let _ = cr.fill();
    }
    s.flush();
    let stride = s.stride() as usize;
    let Ok(d) = s.data() else { return vec![] };
    let mut out = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N as usize {
        for x in 0..N as usize {
            let o = y * stride + x * 4;
            out.extend_from_slice(&[d[o + 3], d[o + 2], d[o + 1], d[o]]);
        }
    }
    vec![ksni::Icon { width: N, height: N, data: out }]
}
