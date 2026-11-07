//! Menu-bar indicator (StatusNotifierItem; Ubuntu shows it through its AppIndicator extension).
//! The tray runs on its own thread and talks to the GTK thread over a channel.

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use std::sync::mpsc::Sender;

pub enum Cmd {
    Open,
    Preset(usize),
    Reset,
    Quit,
}

pub struct Tray {
    tx: Sender<Cmd>,
    pub icon: Vec<ksni::Icon>,
    pub headline: String,
    pub detail: String,
    pub presets: Vec<String>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "io.github.powerlimit".into()
    }
    fn title(&self) -> String {
        "Power Limiter".into()
    }
    fn icon_name(&self) -> String {
        if self.icon.is_empty() { "powerlimit".into() } else { String::new() }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.headline.clone(), description: self.detail.clone(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Cmd::Open);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m: Vec<MenuItem<Self>> = vec![
            StandardItem { label: self.headline.clone(), enabled: false, ..Default::default() }.into(),
            StandardItem { label: self.detail.clone(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Power Limiter".into(),
                icon_name: "window-new-symbolic".into(),
                activate: Box::new(|t: &mut Self| { let _ = t.tx.send(Cmd::Open); }),
                ..Default::default()
            }
            .into(),
        ];
        let items = self.presets.iter().enumerate().map(|(i, name)| {
            StandardItem {
                label: name.clone(),
                activate: Box::new(move |t: &mut Self| { let _ = t.tx.send(Cmd::Preset(i)); }),
                ..Default::default()
            }
            .into()
        }).collect();
        m.push(SubMenu { label: "Apply preset".into(), submenu: items, ..Default::default() }.into());
        m.push(StandardItem {
            label: "Reset to Dell defaults".into(),
            activate: Box::new(|t: &mut Self| { let _ = t.tx.send(Cmd::Reset); }),
            ..Default::default()
        }.into());
        m.push(MenuItem::Separator);
        m.push(StandardItem {
            label: "Quit".into(),
            icon_name: "application-exit-symbolic".into(),
            activate: Box::new(|t: &mut Self| { let _ = t.tx.send(Cmd::Quit); }),
            ..Default::default()
        }.into());
        m
    }
}

pub struct TrayHandle(Handle<Tray>);

impl TrayHandle {
    pub fn update(&self, icon: Vec<ksni::Icon>, headline: String, detail: String) {
        self.0.update(move |t: &mut Tray| {
            t.icon = icon;
            t.headline = headline;
            t.detail = detail;
        });
    }
    pub fn shutdown(&self) {
        let _ = self.0.shutdown();
    }
}

/// None when no indicator host is available on the session bus.
pub fn spawn(tx: Sender<Cmd>, presets: Vec<String>) -> Option<TrayHandle> {
    let tray = Tray { tx, icon: vec![], headline: "Power Limiter".into(), detail: "reading limits…".into(), presets };
    tray.spawn().ok().map(TrayHandle)
}
