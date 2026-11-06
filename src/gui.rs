//! Native GTK4 window. Runs unprivileged; anything that writes limits goes through
//! `pkexec powerlimit ...` so the polkit dialog handles authentication.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::{candidates, current, energy_uj, load_profiles, sample};

enum Msg {
    Log(String),
    TuneDone,
    Applied(Result<(), String>),
    Watch(String),
    WatchEnded,
}

const PRESETS: [(&str, f64, f64, f64); 3] = [
    ("Cool & quiet", 25.0, 35.0, 25.0),
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
            Ok(o) if o.status.code() == Some(126) => Err("authentication cancelled".into()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("pkexec: {e}")),
        };
        let _ = tx.send(Msg::Applied(r));
    });
}

fn spin(min: f64, max: f64, val: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, 1.0);
    s.set_value(val);
    s
}

fn labeled(text: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.add_css_class("dim-label");
    b.append(&l);
    b.append(w);
    b
}

fn slider(label: &str, min: f64, max: f64, val: f64, root: &gtk::Box) -> gtk::Scale {
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, 1.0);
    s.set_value(val);
    s.set_draw_value(true);
    s.set_value_pos(gtk::PositionType::Right);
    s.set_hexpand(true);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(label));
    l.set_width_chars(16);
    l.set_xalign(0.0);
    row.append(&l);
    row.append(&s);
    root.append(&row);
    s
}

fn heading(t: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_markup(&format!("<b>{t}</b>"));
    l.set_xalign(0.0);
    l.set_margin_top(6);
    l
}

fn build(app: &gtk::Application) {
    let (tx, rx) = mpsc::channel::<Msg>();
    let stdin: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));
    let hist: Rc<RefCell<Vec<(f64, f64, f64)>>> = Rc::new(RefCell::new(vec![])); // cpu °C, gpu °C, watts

    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Power Limiter")
        .default_width(560)
        .default_height(820)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for f in [gtk::Box::set_margin_top, gtk::Box::set_margin_bottom, gtk::Box::set_margin_start, gtk::Box::set_margin_end] {
        f(&root, 14);
    }
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&root));
    win.set_child(Some(&scroll));

    // --- live stats + graph
    let stats = gtk::Label::new(None);
    stats.set_xalign(0.0);
    root.append(&stats);
    let graph = gtk::DrawingArea::new();
    graph.set_content_height(150);
    {
        let hist = hist.clone();
        graph.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            let hist = hist.borrow();
            cr.set_line_width(1.0);
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.3);
            for t in [20.0, 40.0, 60.0, 80.0, 100.0] {
                let y = h - t / 100.0 * h;
                cr.move_to(0.0, y);
                cr.line_to(w, y);
                let _ = cr.stroke();
                cr.move_to(2.0, y - 2.0);
                let _ = cr.show_text(&format!("{t:.0}"));
            }
            let series: [(fn(&(f64, f64, f64)) -> f64, (f64, f64, f64)); 3] = [
                (|p| p.0, (1.0, 0.48, 0.35)),
                (|p| p.1, (0.42, 0.83, 0.48)),
                (|p| p.2, (0.30, 0.64, 1.0)),
            ];
            cr.set_line_width(2.0);
            for (get, (r, g, b)) in series {
                cr.set_source_rgb(r, g, b);
                for (i, p) in hist.iter().enumerate() {
                    let x = i as f64 / (hist.len().max(2) - 1) as f64 * w;
                    let y = h - get(p).clamp(0.0, 100.0) / 100.0 * h;
                    if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
                }
                let _ = cr.stroke();
            }
        });
    }
    root.append(&graph);
    let legend = gtk::Label::new(None);
    legend.set_markup("<span color='#ff7a59'>■ CPU °C</span>   <span color='#6bd47a'>■ GPU °C</span>   <span color='#4da3ff'>■ CPU+GPU W</span>");
    legend.set_xalign(0.0);
    root.append(&legend);

    // --- manual limits
    root.append(&heading("Set limits"));
    let presets = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    root.append(&presets);
    let s1 = slider("CPU sustained (W)", 15.0, 65.0, 35.0, &root);
    let s2 = slider("CPU burst (W)", 15.0, 109.0, 45.0, &root);
    let s3 = slider("GPU (W)", 10.0, 45.0, 30.0, &root);
    for (name, a, b, c) in PRESETS {
        let btn = gtk::Button::with_label(name);
        let (s1, s2, s3) = (s1.clone(), s2.clone(), s3.clone());
        btn.connect_clicked(move |_| {
            s1.set_value(a);
            s2.set_value(b);
            s3.set_value(c);
        });
        presets.append(&btn);
    }
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    let reset = gtk::Button::with_label("Dell defaults");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&apply);
    row.append(&reset);
    root.append(&row);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    root.append(&status);
    {
        let (tx, s1, s2, s3) = (tx.clone(), s1.clone(), s2.clone(), s3.clone());
        apply.connect_clicked(move |_| {
            let a = ["set".to_string(), s1.value().to_string(), s2.value().max(s1.value()).to_string(), s3.value().to_string()];
            privileged(a.to_vec(), tx.clone());
        });
    }
    {
        let tx = tx.clone();
        reset.connect_clicked(move |_| privileged(vec!["reset".into()], tx.clone()));
    }

    // --- tuner
    root.append(&heading("Auto-tune for a game"));
    let hint = gtk::Label::new(Some("Start the game first, then press Start. It steps through power levels, watches temperatures, and saves the best level under your caps."));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");
    root.append(&hint);
    let game = gtk::ComboBoxText::new();
    let game_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Rescan running games"));
    game_row.append(&game);
    game_row.append(&reload);
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
    let opts = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    opts.append(&labeled("Running game", &game_row));
    opts.append(&labeled("Minutes", &minutes));
    opts.append(&labeled("CPU max °C", &cmax));
    opts.append(&labeled("GPU max °C", &gmax));
    root.append(&opts);
    let start = gtk::Button::with_label("Start tuning");
    start.add_css_class("suggested-action");
    let stop = gtk::Button::with_label("Stop");
    stop.add_css_class("destructive-action");
    stop.set_sensitive(false);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&start);
    row.append(&stop);
    root.append(&row);
    let bar = gtk::ProgressBar::new();
    root.append(&bar);
    let log = gtk::TextView::new();
    log.set_editable(false);
    log.set_monospace(true);
    log.set_wrap_mode(gtk::WrapMode::WordChar);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_min_content_height(170);
    log_scroll.set_child(Some(&log));
    root.append(&log_scroll);

    root.append(&heading("Saved game profiles"));
    let auto = gtk::CheckButton::with_label("Auto-apply a game's profile when it starts (resets to defaults when it closes)");
    root.append(&auto);
    let auto_status = gtk::Label::new(None);
    auto_status.set_xalign(0.0);
    auto_status.add_css_class("dim-label");
    root.append(&auto_status);
    let profiles = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.append(&profiles);
    let watcher: Rc<RefCell<Option<ChildStdin>>> = Rc::new(RefCell::new(None));
    {
        let (tx, watcher) = (tx.clone(), watcher.clone());
        auto.connect_toggled(move |c| {
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
                    Err(e) => { let _ = tx.send(Msg::Watch(format!("could not start pkexec: {e}"))); }
                }
            } else if let Some(mut s) = watcher.borrow_mut().take() {
                let _ = s.write_all(b"stop\n");
                let _ = s.flush();
            }
        });
    }

    {
        let (tx, stdin, start2, stop2) = (tx.clone(), stdin.clone(), start.clone(), stop.clone());
        let (game, minutes, cmax, gmax) = (game.clone(), minutes.clone(), cmax.clone(), gmax.clone());
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
                    start2.set_sensitive(false);
                    stop2.set_sensitive(true);
                }
                Err(e) => {
                    let _ = tx.send(Msg::Log(format!("could not start pkexec: {e}")));
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
        stop.connect_clicked(move |_| f());
    }
    let watcher_close = watcher.clone();
    win.connect_close_request(move |_| {
        stop_tune();
        if let Some(mut s) = watcher_close.borrow_mut().take() {
            let _ = s.write_all(b"stop\n");
            let _ = s.flush();
        }
        glib::Propagation::Proceed
    });

    // --- periodic refresh: sensors every 2 s, messages from workers every 300 ms
    let refresh = {
        let (stats, graph, hist, profiles, tx) = (stats.clone(), graph.clone(), hist.clone(), profiles.clone(), tx.clone());
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
            let watts = cpu_w.unwrap_or(0.0) + s.gpu_w;
            {
                let mut h = hist.borrow_mut();
                h.push((s.cpu_t, s.gpu_t, watts));
                let n = h.len();
                if n > 300 { h.drain(..n - 300); }
            }
            stats.set_markup(&format!(
                "<span size='large'><b>CPU {:.0}°C</b>   <b>GPU {:.0}°C</b>   GPU {:.1} W{}</span>\nLimits: CPU {:.0}/{:.0} W · GPU {:.0} W",
                s.cpu_t, s.gpu_t, s.gpu_w,
                cpu_w.map(|w| format!("   CPU {w:.1} W")).unwrap_or_default(),
                l.pl1, l.pl2, l.gpu));
            let mut p: Vec<_> = load_profiles().into_iter().collect();
            p.sort_by(|a, b| a.0.cmp(&b.0));
            let key = format!("{p:?}|{}/{}/{}", l.pl1, l.pl2, l.gpu);
            if key != last_key {
                last_key = key;
                while let Some(c) = profiles.first_child() {
                    profiles.remove(&c);
                }
                if p.is_empty() {
                    let none = gtk::Label::new(Some("none yet - run an auto-tune while a game is open"));
                    none.set_xalign(0.0);
                    none.add_css_class("dim-label");
                    profiles.append(&none);
                }
                for (name, v) in &p {
                    let active = (l.pl1 - v.pl1).abs() < 0.5 && (l.pl2 - v.pl2).abs() < 0.5 && (l.gpu - v.gpu).abs() < 0.5;
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let lbl = gtk::Label::new(None);
                    lbl.set_markup(&format!("<b>{}</b>{}\n<small>CPU {}/{} W · GPU {} W</small>",
                        glib::markup_escape_text(name), if active { "  <span color='#6bd47a'>● active</span>" } else { "" }, v.pl1, v.pl2, v.gpu));
                    lbl.set_xalign(0.0);
                    lbl.set_hexpand(true);
                    row.append(&lbl);
                    let apply = gtk::Button::with_label("Apply");
                    apply.set_sensitive(!active);
                    let a = vec!["set".into(), v.pl1.to_string(), v.pl2.to_string(), v.gpu.to_string()];
                    let t = tx.clone();
                    apply.connect_clicked(move |_| privileged(a.clone(), t.clone()));
                    row.append(&apply);
                    let del = gtk::Button::from_icon_name("user-trash-symbolic");
                    del.set_tooltip_text(Some("Forget this profile"));
                    let (name, t) = (name.clone(), tx.clone());
                    del.connect_clicked(move |_| privileged(vec!["forget".into(), name.clone()], t.clone()));
                    row.append(&del);
                    profiles.append(&row);
                }
            }
            graph.queue_draw();
            glib::ControlFlow::Continue
        }
    };
    let mut refresh = refresh;
    refresh();
    glib::timeout_add_seconds_local(2, refresh);

    {
        let (log, bar, status, start, stop, stdin) = (log.clone(), bar.clone(), status.clone(), start.clone(), stop.clone(), stdin.clone());
        let (auto, auto_status, watcher) = (auto.clone(), auto_status.clone(), watcher.clone());
        glib::timeout_add_local(Duration::from_millis(300), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Log(l) => {
                        if let Some(rest) = l.strip_prefix("Trial ") {
                            if let Some((a, b)) = rest.split(':').next().and_then(|x| x.split_once('/')) {
                                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                                    bar.set_fraction((a - 1.0) / b.max(1.0));
                                }
                            }
                        }
                        let buf = log.buffer();
                        buf.insert(&mut buf.end_iter(), &format!("{l}\n"));
                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                        log.scroll_mark_onscreen(&mark);
                    }
                    Msg::TuneDone => {
                        *stdin.borrow_mut() = None;
                        start.set_sensitive(true);
                        stop.set_sensitive(false);
                        bar.set_fraction(0.0);
                    }
                    Msg::Watch(l) => auto_status.set_text(&l),
                    Msg::WatchEnded => {
                        auto_status.set_text("");
                        if auto.is_active() { auto.set_active(false); }
                        *watcher.borrow_mut() = None;
                    }
                    Msg::Applied(Ok(())) => status.set_text("Applied."),
                    Msg::Applied(Err(e)) => status.set_text(&format!("Failed: {e}")),
                }
            }
            glib::ControlFlow::Continue
        });
    }
    win.present();
}
