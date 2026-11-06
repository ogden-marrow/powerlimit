//! Native GTK4 window. Runs unprivileged; anything that writes limits goes through
//! `pkexec powerlimit ...` so the polkit dialog handles authentication.

use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::{candidates, cpu_mhz, current, energy_uj, load_profiles, sample};

enum Msg {
    Log(String),
    TuneDone,
    Applied(Result<(), String>),
    Watch(String),
    WatchEnded,
}

/// name, CPU sustained, CPU burst, GPU
const PRESETS: [(&str, f64, f64, f64); 3] = [
    ("Quiet", 25.0, 35.0, 25.0),
    ("Balanced", 35.0, 45.0, 30.0),
    ("Performance", 45.0, 65.0, 40.0),
];

pub fn run() {
    let app = gtk::Application::builder().application_id("io.github.powerlimit").build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[]);
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "powerlimit".into())
}

/// Run `pkexec powerlimit <args>` off the UI thread.
fn privileged(args: Vec<String>, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let r = match Command::new("pkexec").arg(exe()).args(&args).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) if o.status.code() == Some(126) => Err("Authentication cancelled".into()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("pkexec: {e}")),
        };
        let _ = tx.send(Msg::Applied(r));
    });
}

// ---------------------------------------------------------------- small widget helpers

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn card() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("card");
    b
}

fn separator() -> gtk::Box {
    let s = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    s.add_css_class("row-sep");
    s
}

fn page(children: Vec<gtk::Widget>) -> gtk::ScrolledWindow {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 14);
    col.add_css_class("pl-page");
    col.set_margin_top(18);
    col.set_margin_bottom(24);
    col.set_margin_start(24);
    col.set_margin_end(24);
    for c in &children {
        col.append(c);
    }
    let clamp = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    clamp.add_css_class("pl-page");
    clamp.set_halign(gtk::Align::Fill);
    col.set_hexpand(true);
    clamp.append(&col);
    let sw = gtk::ScrolledWindow::new();
    sw.set_hscrollbar_policy(gtk::PolicyType::Never);
    sw.set_child(Some(&clamp));
    sw
}

fn set_heat(l: &gtk::Label, temp: f64) {
    for c in ["ok", "warm", "hot"] {
        l.remove_css_class(c);
    }
    l.add_css_class(if temp < 70.0 { "ok" } else if temp < 82.0 { "warm" } else { "hot" });
}

/// Big-number tile: name, temperature, detail line.
fn tile(name: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let c = card();
    c.set_hexpand(true);
    c.set_homogeneous(false);
    c.append(&label(name, &["tile-name"]));
    let temp = label("–", &["temp-big"]);
    c.append(&temp);
    let detail = label(" ", &["tile-detail"]);
    c.append(&detail);
    (c, temp, detail)
}

fn slider_row(title: &str, min: f64, max: f64, val: f64, card: &gtk::Box) -> gtk::Scale {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    row.set_margin_top(7);
    row.set_margin_bottom(7);
    let t = label(title, &["row-title"]);
    t.set_width_chars(14);
    row.append(&t);
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, 1.0);
    s.set_hexpand(true);
    s.set_draw_value(false);
    s.set_value(val);
    row.append(&s);
    let pill = label(&format!("{val:.0} W"), &["value-pill"]);
    pill.set_xalign(0.5);
    row.append(&pill);
    s.connect_value_changed(move |s| pill.set_text(&format!("{:.0} W", s.value())));
    card.append(&row);
    s
}

fn settings_row(title: &str, sub: &str, w: &impl IsA<gtk::Widget>, card: &gtk::Box) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.append(&label(title, &["row-title"]));
    if !sub.is_empty() {
        let s = label(sub, &["hint"]);
        s.set_wrap(true);
        text.append(&s);
    }
    row.append(&text);
    w.set_valign(gtk::Align::Center);
    row.append(w);
    card.append(&row);
}

fn spin(min: f64, max: f64, val: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, 1.0);
    s.set_value(val);
    s
}

fn draw_graph(cr: &cairo::Context, w: f64, h: f64, hist: &[(f64, f64, f64)]) {
    cr.set_font_size(10.0);
    cr.set_line_width(1.0);
    for t in [25.0, 50.0, 75.0, 100.0] {
        let y = h - t / 100.0 * h;
        cr.set_source_rgba(0.5, 0.5, 0.55, 0.25);
        cr.move_to(0.0, y);
        cr.line_to(w, y);
        let _ = cr.stroke();
        cr.set_source_rgba(0.5, 0.5, 0.55, 0.9);
        cr.move_to(w - 24.0, y - 3.0);
        let _ = cr.show_text(&format!("{t:.0}"));
    }
    if hist.len() < 2 {
        return;
    }
    let series: [(fn(&(f64, f64, f64)) -> f64, (f64, f64, f64), bool); 3] = [
        (|p| p.2, (0.0, 0.52, 0.95), false),
        (|p| p.1, (0.20, 0.78, 0.35), true),
        (|p| p.0, (1.0, 0.62, 0.04), true),
    ];
    cr.set_line_join(cairo::LineJoin::Round);
    for (get, (r, g, b), fill) in series {
        let pt = |i: usize| (i as f64 / (hist.len() - 1) as f64 * w, h - get(&hist[i]).clamp(0.0, 100.0) / 100.0 * h);
        cr.new_path();
        for i in 0..hist.len() {
            let (x, y) = pt(i);
            if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
        }
        let line = cr.copy_path().ok();
        if fill {
            cr.line_to(w, h);
            cr.line_to(0.0, h);
            cr.close_path();
            let grad = cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
            grad.add_color_stop_rgba(0.0, r, g, b, 0.22);
            grad.add_color_stop_rgba(1.0, r, g, b, 0.0);
            let _ = cr.set_source(&grad);
            let _ = cr.fill();
        }
        if let Some(p) = line {
            cr.new_path();
            let _ = cr.append_path(&p);
        }
        cr.set_source_rgb(r, g, b);
        cr.set_line_width(2.2);
        let _ = cr.stroke();
    }
}

// ---------------------------------------------------------------- window

fn build(app: &gtk::Application) {
    gtk::Window::set_default_icon_name("powerlimit");
    // Follow the system: GTK's own text colour tells us whether the theme is dark.
    let probe = gtk::Label::new(None);
    #[allow(deprecated)]
    let dark = {
        let c = probe.style_context().color();
        (c.red() + c.green() + c.blue()) / 3.0 > 0.5
    };
    if let Some(display) = gtk::gdk::Display::default() {
        for (css, prio) in [(include_str!("style.css"), 0), (include_str!("style-dark.css"), 1)] {
            if prio == 1 && !dark {
                continue;
            }
            let provider = gtk::CssProvider::new();
            #[allow(deprecated)]
            provider.load_from_data(css);
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + prio);
        }
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let stdin: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));
    let watcher: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));
    let hist: Rc<RefCell<Vec<(f64, f64, f64)>>> = Rc::new(RefCell::new(vec![]));

    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Power Limiter")
        .default_width(760)
        .default_height(840)
        .build();
    win.add_css_class("pl-window");
    let header = gtk::HeaderBar::new();
    header.add_css_class("pl-header");
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    header.set_title_widget(Some(&switcher));
    win.set_titlebar(Some(&header));

    // toast
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&stack));
    let toast_label = label("", &["toast"]);
    let toast = gtk::Revealer::new();
    toast.set_child(Some(&toast_label));
    toast.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    toast.set_halign(gtk::Align::Center);
    toast.set_valign(gtk::Align::End);
    toast.set_margin_bottom(28);
    overlay.add_overlay(&toast);
    let show_toast: Rc<dyn Fn(&str)> = {
        let (toast, toast_label) = (toast.clone(), toast_label.clone());
        let gen = Rc::new(Cell::new(0u32));
        Rc::new(move |m: &str| {
            toast_label.set_text(m);
            toast.set_reveal_child(true);
            gen.set(gen.get() + 1);
            let (toast, gen2, mine) = (toast.clone(), gen.clone(), gen.get());
            glib::timeout_add_seconds_local_once(3, move || {
                if gen2.get() == mine {
                    toast.set_reveal_child(false);
                }
            });
        })
    };
    win.set_child(Some(&overlay));

    // ======================= Overview
    let (cpu_tile, cpu_temp, cpu_detail) = tile("CPU");
    let (gpu_tile, gpu_temp, gpu_detail) = tile("GPU");
    let tiles = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    tiles.set_homogeneous(true);
    tiles.append(&cpu_tile);
    tiles.append(&gpu_tile);

    let graph_card = card();
    let graph_head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    graph_head.append(&label("Last 10 minutes", &["row-title"]));
    let legend = gtk::Label::new(None);
    legend.set_markup("<span color='#ff9f0a'>●</span> CPU °C   <span color='#34c759'>●</span> GPU °C   <span color='#0085f2'>●</span> CPU+GPU W");
    legend.add_css_class("legend-dot");
    legend.set_hexpand(true);
    legend.set_xalign(1.0);
    graph_head.append(&legend);
    graph_card.append(&graph_head);
    let graph = gtk::DrawingArea::new();
    graph.set_content_height(150);
    graph.set_margin_top(10);
    {
        let hist = hist.clone();
        graph.set_draw_func(move |_, cr, w, h| draw_graph(cr, w as f64, h as f64, &hist.borrow()));
    }
    graph_card.append(&graph);

    let limits_title = label("POWER LIMITS", &["section"]);
    let power = card();
    let seg = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    seg.add_css_class("segmented");
    seg.set_homogeneous(true);
    let mut seg_btns: Vec<gtk::ToggleButton> = vec![];
    for name in PRESETS.iter().map(|p| p.0).chain(["Custom"]) {
        let b = gtk::ToggleButton::with_label(name);
        if let Some(first) = seg_btns.first() {
            b.set_group(Some(first));
        }
        seg.append(&b);
        seg_btns.push(b);
    }
    power.append(&seg);
    power.append(&separator());
    let now = current();
    let s1 = slider_row("CPU sustained", 15.0, 65.0, now.pl1.clamp(15.0, 65.0), &power);
    let s2 = slider_row("CPU burst", 15.0, 109.0, now.pl2.clamp(15.0, 109.0), &power);
    let s3 = slider_row("GPU", 10.0, 45.0, now.gpu.clamp(10.0, 45.0).max(10.0), &power);
    power.append(&separator());
    let keep = gtk::Switch::new();
    keep.set_active(std::path::Path::new("/etc/udev/rules.d/99-powerlimit.rules").exists());
    settings_row("Keep after reboot", "Re-applies the CPU limits at startup. The GPU limit isn't kept.", &keep, &power);
    power.append(&separator());
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let reset = gtk::Button::with_label("Reset to Dell defaults");
    reset.add_css_class("quiet");
    reset.add_css_class("danger");
    reset.set_hexpand(true);
    reset.set_halign(gtk::Align::Start);
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("pill");
    actions.append(&reset);
    actions.append(&apply);
    power.append(&actions);

    // presets <-> sliders
    let guard = Rc::new(Cell::new(false));
    {
        let matches = |p: &(&str, f64, f64, f64)| (now.pl1 - p.1).abs() < 0.5 && (now.pl2 - p.2).abs() < 0.5 && (now.gpu - p.3).abs() < 0.5;
        let idx = PRESETS.iter().position(matches).unwrap_or(3);
        seg_btns[idx].set_active(true);
    }
    for (i, b) in seg_btns.iter().enumerate().take(3) {
        let (s1, s2, s3, guard) = (s1.clone(), s2.clone(), s3.clone(), guard.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                guard.set(true);
                s1.set_value(PRESETS[i].1);
                s2.set_value(PRESETS[i].2);
                s3.set_value(PRESETS[i].3);
                guard.set(false);
            }
        });
    }
    for s in [&s1, &s2, &s3] {
        let (custom, guard) = (seg_btns[3].clone(), guard.clone());
        s.connect_value_changed(move |_| {
            if !guard.get() {
                custom.set_active(true);
            }
        });
    }
    {
        let (tx, s1, s2, s3, keep) = (tx.clone(), s1.clone(), s2.clone(), s3.clone(), keep.clone());
        apply.connect_clicked(move |_| {
            let a = ["set".to_string(), s1.value().to_string(), s2.value().max(s1.value()).to_string(), s3.value().to_string(),
                if keep.is_active() { "--persist" } else { "--no-persist" }.to_string()];
            privileged(a.to_vec(), tx.clone());
        });
    }
    {
        let tx = tx.clone();
        reset.connect_clicked(move |_| privileged(vec!["reset".into()], tx.clone()));
    }

    let overview = page(vec![tiles.upcast(), graph_card.upcast(), limits_title.upcast(), power.upcast()]);
    stack.add_titled(&overview, Some("overview"), "Overview");

    // ======================= Tune
    let tune_stack = gtk::Stack::new();
    tune_stack.set_transition_type(gtk::StackTransitionType::Crossfade);

    let idle = gtk::Box::new(gtk::Orientation::Vertical, 14);
    let hero = card();
    hero.append(&label("Tune for a game", &["hero-title"]));
    let blurb = label("Start your game and get into actual gameplay, then press Start. Power Limiter tries different CPU and GPU limits, watches clock speeds, temperatures and throttling, and saves the best setting for that game.", &["hint"]);
    blurb.set_wrap(true);
    blurb.set_margin_top(6);
    hero.append(&blurb);
    idle.append(&hero);

    let opts = card();
    let game = gtk::ComboBoxText::new();
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.add_css_class("icon-quiet");
    reload.set_tooltip_text(Some("Rescan running games"));
    let game_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    game_box.append(&game);
    game_box.append(&reload);
    let fill = {
        let game = game.clone();
        move || {
            game.remove_all();
            game.append(Some("auto"), "Auto-detect");
            for (pid, name) in candidates() {
                game.append(Some(&pid.to_string()), &format!("{name} · pid {pid}"));
            }
            game.set_active_id(Some("auto"));
        }
    };
    fill();
    {
        let fill = fill.clone();
        reload.connect_clicked(move |_| fill());
    }
    let minutes = spin(6.0, 240.0, 30.0);
    let cmax = spin(60.0, 100.0, 85.0);
    let gmax = spin(60.0, 95.0, 80.0);
    settings_row("Game", "Pick the running game, or let it auto-detect.", &game_box, &opts);
    opts.append(&separator());
    settings_row("Duration", "Minutes. Longer gives steadier readings.", &minutes, &opts);
    opts.append(&separator());
    settings_row("CPU temperature limit", "°C - never go hotter than this.", &cmax, &opts);
    opts.append(&separator());
    settings_row("GPU temperature limit", "°C", &gmax, &opts);
    idle.append(&opts);

    let result = label("", &["result-ok"]);
    result.set_wrap(true);
    idle.append(&result);
    let start = gtk::Button::with_label("Start tuning");
    start.add_css_class("pill");
    start.set_halign(gtk::Align::Center);
    start.set_margin_top(4);
    idle.append(&start);
    tune_stack.add_named(&idle, Some("idle"));

    let running = gtk::Box::new(gtk::Orientation::Vertical, 14);
    let rc = card();
    let phase = label("Starting…", &["big-phase"]);
    phase.set_wrap(true);
    let trial = label("", &["secondary"]);
    let bar = gtk::ProgressBar::new();
    bar.set_margin_top(14);
    let detail = label("", &["hint"]);
    detail.set_wrap(true);
    detail.set_margin_top(10);
    rc.append(&phase);
    rc.append(&trial);
    rc.append(&bar);
    rc.append(&detail);
    running.append(&rc);
    let stop = gtk::Button::with_label("Stop");
    stop.add_css_class("pill");
    stop.add_css_class("danger");
    stop.set_halign(gtk::Align::Center);
    running.append(&stop);
    let log = gtk::TextView::new();
    log.set_editable(false);
    log.set_cursor_visible(false);
    log.add_css_class("log");
    log.set_wrap_mode(gtk::WrapMode::WordChar);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_min_content_height(200);
    log_scroll.set_child(Some(&log));
    let expander = gtk::Expander::new(Some("Details"));
    expander.set_child(Some(&log_scroll));
    running.append(&expander);
    tune_stack.add_named(&running, Some("running"));

    let tune_wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tune_wrap.append(&tune_stack);
    stack.add_titled(&page(vec![tune_wrap.upcast()]), Some("tune"), "Tune");

    {
        let (tx, stdin, tune_stack) = (tx.clone(), stdin.clone(), tune_stack.clone());
        let (game, minutes, cmax, gmax) = (game.clone(), minutes.clone(), cmax.clone(), gmax.clone());
        let (phase, trial, bar, detail, log, result) = (phase.clone(), trial.clone(), bar.clone(), detail.clone(), log.clone(), result.clone());
        start.connect_clicked(move |_| {
            let mut cmd = Command::new("pkexec");
            cmd.arg(exe()).args(["tune", "--stdin-stop", "--minutes", &minutes.value().to_string(),
                "--cpu-max", &cmax.value().to_string(), "--gpu-max", &gmax.value().to_string()]);
            if let Some(pid) = game.active_id().filter(|id| id.as_str() != "auto") {
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
                    phase.set_text("Starting…");
                    trial.set_text("");
                    detail.set_text("");
                    bar.set_fraction(0.0);
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
    let auto_card = card();
    let auto = gtk::Switch::new();
    settings_row("Switch automatically", "Applies a game's profile when it starts and restores defaults when it closes.", &auto, &auto_card);
    let auto_status = label("", &["hint"]);
    auto_status.set_margin_top(4);
    auto_card.append(&auto_status);
    {
        let (tx, watcher) = (tx.clone(), watcher.clone());
        auto.connect_active_notify(move |c| {
            if c.is_active() {
                let spawned = Command::new("pkexec").arg(exe()).args(["watch", "--stdin-stop"])
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
    let profiles_title = label("GAME PROFILES", &["section"]);
    let profiles = card();
    stack.add_titled(&page(vec![auto_card.upcast(), profiles_title.upcast(), profiles.clone().upcast()]), Some("profiles"), "Profiles");

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

    // ======================= periodic refresh
    let mut refresh = {
        let (graph, hist, profiles, tx) = (graph.clone(), hist.clone(), profiles.clone(), tx.clone());
        let mut last_key = String::new();
        let mut last_e = energy_uj().map(|e| (e, std::time::Instant::now()));
        move || {
            let s = sample();
            let l = current();
            let now = energy_uj();
            let cpu_w = match (last_e, now) {
                (Some((e0, t0)), Some(e1)) => Some((e1 - e0).max(0.0) / 1e6 / t0.elapsed().as_secs_f64()),
                _ => None,
            };
            last_e = now.map(|e| (e, std::time::Instant::now()));
            {
                let mut h = hist.borrow_mut();
                h.push((s.cpu_t, s.gpu_t, cpu_w.unwrap_or(0.0) + s.gpu_w));
                let n = h.len();
                if n > 300 { h.drain(..n - 300); }
            }
            cpu_temp.set_text(&format!("{:.0}°", s.cpu_t));
            set_heat(&cpu_temp, s.cpu_t);
            cpu_detail.set_text(&format!("{}{:.1} GHz · limit {:.0} W", cpu_w.map(|w| format!("{w:.0} W · ")).unwrap_or_default(), cpu_mhz() / 1000.0, l.pl1));
            gpu_temp.set_text(&format!("{:.0}°", s.gpu_t));
            set_heat(&gpu_temp, s.gpu_t);
            gpu_detail.set_text(&format!("{:.0} W · {:.1} GHz · {:.0}% busy", s.gpu_w, s.gpu_mhz / 1000.0, s.gpu_util));
            graph.queue_draw();

            let mut p: Vec<_> = load_profiles().into_iter().collect();
            p.sort_by(|a, b| a.0.cmp(&b.0));
            let key = format!("{p:?}|{}/{}/{}", l.pl1, l.pl2, l.gpu);
            if key != last_key {
                last_key = key;
                while let Some(c) = profiles.first_child() {
                    profiles.remove(&c);
                }
                if p.is_empty() {
                    profiles.append(&label("No profiles yet", &["row-title"]));
                    let h = label("Open a game and run Tune. The best setting is saved here, one per game.", &["hint"]);
                    h.set_wrap(true);
                    profiles.append(&h);
                }
                for (i, (name, v)) in p.iter().enumerate() {
                    if i > 0 {
                        profiles.append(&separator());
                    }
                    let active = (l.pl1 - v.pl1).abs() < 0.5 && (l.pl2 - v.pl2).abs() < 0.5 && (l.gpu - v.gpu).abs() < 0.5;
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
                    text.set_hexpand(true);
                    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    top.append(&label(name, &["profile-name"]));
                    if active {
                        top.append(&label("IN USE", &["badge"]));
                    }
                    text.append(&top);
                    text.append(&label(&format!("CPU {:.0} / {:.0} W · GPU {:.0} W", v.pl1, v.pl2, v.gpu), &["hint"]));
                    row.append(&text);
                    let apply = gtk::Button::with_label("Apply");
                    apply.add_css_class("quiet");
                    apply.set_sensitive(!active);
                    apply.set_valign(gtk::Align::Center);
                    let a = vec!["set".into(), v.pl1.to_string(), v.pl2.to_string(), v.gpu.to_string()];
                    let t = tx.clone();
                    apply.connect_clicked(move |_| privileged(a.clone(), t.clone()));
                    row.append(&apply);
                    let del = gtk::Button::from_icon_name("user-trash-symbolic");
                    del.add_css_class("icon-quiet");
                    del.set_valign(gtk::Align::Center);
                    del.set_tooltip_text(Some("Forget this profile"));
                    let (name, t) = (name.clone(), tx.clone());
                    del.connect_clicked(move |_| privileged(vec!["forget".into(), name.clone()], t.clone()));
                    row.append(&del);
                    profiles.append(&row);
                }
            }
            glib::ControlFlow::Continue
        }
    };
    refresh();
    glib::timeout_add_seconds_local(2, refresh);

    // ======================= messages from worker threads
    {
        let (log, bar, stdin, tune_stack) = (log.clone(), bar.clone(), stdin.clone(), tune_stack.clone());
        let (phase, trial, detail, result, stop) = (phase.clone(), trial.clone(), detail.clone(), result.clone(), stop.clone());
        let (auto, auto_status, watcher, show_toast) = (auto.clone(), auto_status.clone(), watcher.clone(), show_toast.clone());
        glib::timeout_add_local(Duration::from_millis(300), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Log(l) => {
                        let t = l.trim();
                        if let Some(rest) = t.strip_prefix("Trial ") {
                            if let Some((a, b)) = rest.split(':').next().and_then(|x| x.split_once('/')) {
                                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                                    bar.set_fraction((a - 1.0) / b.max(1.0));
                                    trial.set_text(&format!("Trial {a:.0} of {b:.0}"));
                                    detail.set_text("Letting temperatures settle…");
                                }
                            }
                        } else if t.starts_with("Phase 1") {
                            phase.set_text("Finding the thermal ceiling");
                        } else if t.starts_with("Phase 2") {
                            phase.set_text("Balancing CPU and GPU");
                            if let Some((_, rest)) = t.split_once("->") {
                                detail.set_text(rest.trim());
                            }
                        } else if t.starts_with("peak") {
                            detail.set_text(t);
                        } else if t.starts_with("cooling") {
                            detail.set_text("Cooling down before the next trial…");
                        } else if t.starts_with("Best for") {
                            result.set_text(&format!("✓ {t}"));
                            bar.set_fraction(1.0);
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
                    }
                    Msg::Watch(l) => auto_status.set_text(&l),
                    Msg::WatchEnded => {
                        auto_status.set_text("");
                        *watcher.borrow_mut() = None;
                        if auto.is_active() { auto.set_active(false); }
                    }
                    Msg::Applied(Ok(())) => show_toast("Applied"),
                    Msg::Applied(Err(e)) => show_toast(&format!("Failed: {e}")),
                }
            }
            glib::ControlFlow::Continue
        });
    }
    win.present();

    // developer aid: POWERLIMIT_SHOT=/some/dir saves a PNG of each page, then quits
    if let Ok(dir) = std::env::var("POWERLIMIT_SHOT") {
        for (i, name) in ["overview", "tune", "profiles"].into_iter().enumerate() {
            let (win, stack, app) = (win.clone(), stack.clone(), win.application());
            let dir = dir.clone();
            glib::timeout_add_local_once(Duration::from_millis(2000 + 1500 * i as u64), move || {
                stack.set_visible_child_name(name);
                let (win2, dir) = (win.clone(), dir.clone());
                glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    let (w, h) = (win2.width(), win2.height());
                    let paintable = gtk::WidgetPaintable::new(Some(&win2));
                    let snap = gtk::Snapshot::new();
                    paintable.snapshot(&snap, w as f64, h as f64);
                    if let (Some(node), Some(native)) = (snap.to_node(), win2.native()) {
                        if let Some(r) = native.renderer() {
                            let ok = r.render_texture(node, None).save_to_png(format!("{dir}/{name}.png"));
                            eprintln!("saved {name}: {ok:?}");
                        }
                    }
                });
                if name == "profiles" {
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || { if let Some(a) = app { a.quit(); } });
                }
            });
        }
    }
}
