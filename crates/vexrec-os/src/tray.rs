use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, info, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use vexrec_core::xdg;

const SNI_ITEM_PATH: &str = "/org/ayatana/NotificationItem/vexrec";
const SNI_MENU_PATH: &str = "/org/ayatana/NotificationItem/vexrec/Menu";

/// Actions triggered from the OS-level system tray timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    StopRecording,
    TogglePause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraySnapshot {
    visible: bool,
    elapsed_secs: u64,
    paused: bool,
}

impl Default for TraySnapshot {
    fn default() -> Self {
        Self {
            visible: false,
            elapsed_secs: 0,
            paused: false,
        }
    }
}

#[derive(Debug, Default)]
struct TraySharedState {
    snapshot: TraySnapshot,
    icon_path: String,
    accessible_desc: String,
    menu_revision: u32,
}

impl TraySharedState {
    fn status_str(&self) -> &'static str {
        if self.snapshot.visible {
            "Active"
        } else {
            "Passive"
        }
    }

    fn pause_label(&self) -> &'static str {
        if self.snapshot.paused {
            "Resume Recording"
        } else {
            "Pause Recording"
        }
    }
}

/// Formats elapsed recording seconds as `MM:SS` or `HH:MM:SS`.
pub fn format_timer_text(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours:02}:{mins:02}:{rem:02}")
    } else {
        format!("{mins:02}:{rem:02}")
    }
}

/// Renders a wide, outline-only SVG timer pill (no red dot/circle) for the OS panel tray.
///
/// Aspect width satisfies `width >= height * 1.5` so GNOME's `ubuntu-appindicators`
/// renders the full capsule width cleanly inside the top panel without cropping to a square icon.
pub fn format_tray_timer_svg(elapsed_secs: u64, paused: bool) -> String {
    let time_text = format_timer_text(elapsed_secs);
    let height: u32 = 22;
    let width: u32 = if elapsed_secs >= 3600 { 112 } else { 92 };
    let rect_w = width - 2;
    let cx = width / 2;

    let (stroke_color, stroke_opacity, text_color, text_opacity) = if paused {
        ("#a1a1aa", "0.58", "#a1a1aa", "0.85")
    } else {
        ("#ffffff", "0.78", "#ffffff", "0.96")
    };

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\
         <rect x=\"1\" y=\"1.5\" width=\"{rect_w}\" height=\"19\" rx=\"9.5\" fill=\"none\" stroke=\"{stroke_color}\" stroke-opacity=\"{stroke_opacity}\" stroke-width=\"1.25\"/>\
         <text x=\"{cx}\" y=\"15.2\" fill=\"{text_color}\" fill-opacity=\"{text_opacity}\" font-family=\"JetBrains Mono, Liberation Mono, DejaVu Sans Mono, monospace\" font-size=\"12\" font-weight=\"600\" text-anchor=\"middle\" letter-spacing=\"0.8\">{time_text}</text>\
         </svg>"
    )
}

struct SvgTrayCache {
    dir: PathBuf,
    seq: u64,
    recent_files: Vec<PathBuf>,
}

impl SvgTrayCache {
    fn new() -> Self {
        let dir = xdg::runtime_dir().join(format!("vexrec-tray-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            seq: 0,
            recent_files: Vec::new(),
        }
    }

    fn write_timer(&mut self, elapsed_secs: u64, paused: bool) -> Option<String> {
        let _ = fs::create_dir_all(&self.dir);
        self.seq = self.seq.wrapping_add(1);
        let path = self.dir.join(format!("timer_{}.svg", self.seq));
        let svg = format_tray_timer_svg(elapsed_secs, paused);
        if fs::write(&path, svg).is_err() {
            return None;
        }
        self.recent_files.push(path.clone());
        // Keep the last 3 frames briefly so async panel loaders never hit ENOENT mid-read.
        while self.recent_files.len() > 3 {
            let old = self.recent_files.remove(0);
            let _ = fs::remove_file(old);
        }
        Some(path.to_string_lossy().into_owned())
    }

    fn clear(&mut self) {
        for old in self.recent_files.drain(..) {
            let _ = fs::remove_file(old);
        }
    }
}

impl Drop for SvgTrayCache {
    fn drop(&mut self) {
        self.clear();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct VexrecStatusNotifierItem {
    state: Arc<Mutex<TraySharedState>>,
    on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl VexrecStatusNotifierItem {
    #[zbus(property)]
    fn category(&self) -> &'static str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &'static str {
        "vexrec"
    }

    #[zbus(property)]
    fn title(&self) -> &'static str {
        "Vexrec Recording Timer"
    }

    #[zbus(property)]
    fn status(&self) -> String {
        self.state.lock().unwrap().status_str().to_string()
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(SNI_MENU_PATH).expect("valid menu object path")
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.state.lock().unwrap().icon_path.clone()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn icon_accessible_desc(&self) -> String {
        self.state.lock().unwrap().accessible_desc.clone()
    }

    #[zbus(property, name = "XAyatanaLabel")]
    fn x_ayatana_label(&self) -> &'static str {
        ""
    }

    #[zbus(property, name = "XAyatanaLabelGuide")]
    fn x_ayatana_label_guide(&self) -> &'static str {
        ""
    }

    fn activate(&self, _x: i32, _y: i32) {
        (self.on_action)(TrayAction::StopRecording);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        (self.on_action)(TrayAction::TogglePause);
    }

    #[zbus(name = "XAyatanaSecondaryActivate")]
    fn x_ayatana_secondary_activate(&self, _timestamp: u32) {
        (self.on_action)(TrayAction::TogglePause);
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn provide_xdg_activation_token(&self, _token: &str) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct VexrecDbusMenu {
    state: Arc<Mutex<TraySharedState>>,
    on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
}

fn owned_str(s: &str) -> OwnedValue {
    OwnedValue::try_from(Value::from(s)).expect("string variant")
}

fn owned_bool(b: bool) -> OwnedValue {
    OwnedValue::try_from(Value::from(b)).expect("bool variant")
}

fn item_props(id: i32, pause_label: &str) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    match id {
        0 => {
            map.insert("children-display".to_string(), owned_str("submenu"));
        }
        1 => {
            map.insert("type".to_string(), owned_str("standard"));
            map.insert("label".to_string(), owned_str("Stop Recording"));
            map.insert("enabled".to_string(), owned_bool(true));
            map.insert("visible".to_string(), owned_bool(true));
        }
        2 => {
            map.insert("type".to_string(), owned_str("standard"));
            map.insert("label".to_string(), owned_str(pause_label));
            map.insert("enabled".to_string(), owned_bool(true));
            map.insert("visible".to_string(), owned_bool(true));
        }
        _ => {}
    }
    map
}

fn build_menu_node(id: i32, props: HashMap<String, OwnedValue>, children: Vec<OwnedValue>) -> OwnedValue {
    let val_props: HashMap<String, Value<'_>> = props
        .into_iter()
        .map(|(k, v)| (k, Value::from(v)))
        .collect();
    let val_children: Vec<Value<'_>> = children.into_iter().map(Value::from).collect();
    let node_val = Value::from((id, val_props, val_children));
    OwnedValue::try_from(node_val).expect("dbusmenu node tuple variant")
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl VexrecDbusMenu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &'static str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &'static str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)) {
        let guard = self.state.lock().unwrap();
        let pause_label = guard.pause_label().to_string();
        let rev = guard.menu_revision;
        drop(guard);

        let children = if parent_id == 0 && recursion_depth != 0 {
            vec![
                build_menu_node(1, item_props(1, &pause_label), Vec::new()),
                build_menu_node(2, item_props(2, &pause_label), Vec::new()),
            ]
        } else {
            Vec::new()
        };

        (rev, (parent_id, item_props(parent_id, &pause_label), children))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let pause_label = self.state.lock().unwrap().pause_label().to_string();
        ids.into_iter()
            .filter(|id| matches!(id, 0 | 1 | 2))
            .map(|id| (id, item_props(id, &pause_label)))
            .collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        let pause_label = self.state.lock().unwrap().pause_label().to_string();
        item_props(id, &pause_label)
            .remove(name)
            .ok_or_else(|| zbus::fdo::Error::UnknownProperty(format!("Unknown menu property {id}.{name}")))
    }

    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
        if event_id == "clicked" {
            match id {
                1 => (self.on_action)(TrayAction::StopRecording),
                2 => (self.on_action)(TrayAction::TogglePause),
                _ => {}
            }
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, _, _) in events {
            if event_id == "clicked" {
                match id {
                    1 => (self.on_action)(TrayAction::StopRecording),
                    2 => (self.on_action)(TrayAction::TogglePause),
                    _ => {}
                }
            }
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    async fn items_properties_updated(
        emitter: &SignalEmitter<'_>,
        updated_props: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

/// Manages Vexrec's native OS top-panel `StatusNotifierItem` tray timer via D-Bus (`zbus`).
pub struct TrayIndicator {
    desired: Arc<Mutex<TraySnapshot>>,
    wake_tx: Option<UnixStream>,
    host_registered: Arc<AtomicBool>,
    item_registered: Arc<AtomicBool>,
}

impl TrayIndicator {
    /// Starts the D-Bus `StatusNotifierItem` background worker on the user session bus.
    pub fn start<F>(on_action: F) -> Self
    where
        F: Fn(TrayAction) + Send + Sync + 'static,
    {
        let desired = Arc::new(Mutex::new(TraySnapshot::default()));
        let host_registered = Arc::new(AtomicBool::new(false));
        let item_registered = Arc::new(AtomicBool::new(false));
        let action_cb: Arc<dyn Fn(TrayAction) + Send + Sync> = Arc::new(on_action);

        let (wake_tx, wake_rx) = match UnixStream::pair() {
            Ok((tx, rx)) => {
                let _ = tx.set_nonblocking(true);
                let _ = rx.set_nonblocking(true);
                (Some(tx), Some(rx))
            }
            Err(_) => (None, None),
        };

        if let Some(wake_rx) = wake_rx {
            let desired_bg = Arc::clone(&desired);
            let host_bg = Arc::clone(&host_registered);
            let item_bg = Arc::clone(&item_registered);

            let _ = std::thread::Builder::new()
                .name("vexrec-sni-tray".into())
                .spawn(move || {
                    let res = async_io::block_on(run_tray_dbus_loop(
                        wake_rx,
                        desired_bg,
                        host_bg,
                        item_bg,
                        action_cb,
                    ));
                    if let Err(e) = res {
                        warn!("OS tray D-Bus worker exited: {e}");
                    }
                });
        }

        Self {
            desired,
            wake_tx,
            host_registered,
            item_registered,
        }
    }

    /// Returns true when the desktop has a live `StatusNotifierWatcher` host (e.g. GNOME AppIndicators).
    pub fn is_host_active(&self) -> bool {
        self.host_registered.load(Ordering::Relaxed) && self.item_registered.load(Ordering::Relaxed)
    }

    pub fn show_recording(&self, elapsed_secs: u64, paused: bool) {
        self.push_snapshot(TraySnapshot {
            visible: true,
            elapsed_secs,
            paused,
        });
    }

    pub fn update_timer(&self, elapsed_secs: u64, paused: bool) {
        self.push_snapshot(TraySnapshot {
            visible: true,
            elapsed_secs,
            paused,
        });
    }

    pub fn hide(&self) {
        self.push_snapshot(TraySnapshot {
            visible: false,
            elapsed_secs: 0,
            paused: false,
        });
    }

    fn push_snapshot(&self, next: TraySnapshot) {
        let changed = {
            let mut guard = self.desired.lock().unwrap();
            if *guard == next {
                false
            } else {
                *guard = next;
                true
            }
        };
        if changed {
            if let Some(ref tx) = self.wake_tx {
                let _ = (&*tx).write_all(&[1u8]);
            }
        }
    }
}

async fn query_host_registered(conn: &zbus::Connection) -> bool {
    let Ok(proxy) = zbus::Proxy::new(
        conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await
    else {
        return false;
    };

    proxy
        .get_property::<bool>("IsStatusNotifierHostRegistered")
        .await
        .unwrap_or(false)
}

async fn register_with_watcher(conn: &zbus::Connection) -> bool {
    let Ok(proxy) = zbus::Proxy::new(
        conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await
    else {
        return false;
    };

    match proxy
        .call::<_, _, ()>("RegisterStatusNotifierItem", &(SNI_ITEM_PATH,))
        .await
    {
        Ok(()) => {
            debug!("Registered Vexrec StatusNotifierItem with org.kde.StatusNotifierWatcher");
            true
        }
        Err(e) => {
            debug!("RegisterStatusNotifierItem failed: {e}");
            false
        }
    }
}

async fn run_tray_dbus_loop(
    mut wake_rx: UnixStream,
    desired: Arc<Mutex<TraySnapshot>>,
    host_registered: Arc<AtomicBool>,
    item_registered: Arc<AtomicBool>,
    on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
) -> zbus::Result<()> {
    let shared = Arc::new(Mutex::new(TraySharedState::default()));
    let item = VexrecStatusNotifierItem {
        state: Arc::clone(&shared),
        on_action: Arc::clone(&on_action),
    };
    let menu = VexrecDbusMenu {
        state: Arc::clone(&shared),
        on_action,
    };

    let well_known = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let conn = zbus::connection::Builder::session()?
        .name(well_known.as_str())?
        .serve_at(SNI_ITEM_PATH, item)?
        .serve_at(SNI_MENU_PATH, menu)?
        .build()
        .await?;

    let mut cache = SvgTrayCache::new();

    if register_with_watcher(&conn).await {
        item_registered.store(true, Ordering::Relaxed);
    }
    let initial_host = query_host_registered(&conn).await;
    host_registered.store(initial_host, Ordering::Relaxed);
    info!(
        host_active = initial_host,
        registered = item_registered.load(Ordering::Relaxed),
        "Vexrec OS-level StatusNotifierItem tray ready"
    );

    let fdo_proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let mut name_owner_stream = fdo_proxy.receive_signal("NameOwnerChanged").await?;

    let async_wake = async_io::Async::new(wake_rx.try_clone().map_err(|e| {
        zbus::Error::InputOutput(Arc::new(e))
    })?)?;
    let mut applied = TraySnapshot::default();

    loop {
        use futures_lite::stream::StreamExt;

        let wake_fut = async {
            let _ = async_wake.readable().await;
            let mut buf = [0u8; 64];
            while wake_rx.read(&mut buf).is_ok() {}
            true
        };
        let dbus_fut = async {
            while let Some(msg) = name_owner_stream.next().await {
                if let Ok((name, _old, new_owner)) =
                    msg.body().deserialize::<(String, String, String)>()
                {
                    if name == "org.kde.StatusNotifierWatcher" {
                        if new_owner.is_empty() {
                            item_registered.store(false, Ordering::Relaxed);
                            host_registered.store(false, Ordering::Relaxed);
                        } else {
                            let reg = register_with_watcher(&conn).await;
                            item_registered.store(reg, Ordering::Relaxed);
                            host_registered
                                .store(query_host_registered(&conn).await, Ordering::Relaxed);
                        }
                    }
                }
            }
            false
        };

        let woke = futures_lite::future::or(wake_fut, dbus_fut).await;
        if !woke {
            break;
        }

        let target = desired.lock().unwrap().clone();
        if target == applied {
            continue;
        }

        let item_iface = conn
            .object_server()
            .interface::<_, VexrecStatusNotifierItem>(SNI_ITEM_PATH)
            .await?;
        let menu_iface = conn
            .object_server()
            .interface::<_, VexrecDbusMenu>(SNI_MENU_PATH)
            .await?;

        let status_changed = target.visible != applied.visible;
        let pause_changed = target.paused != applied.paused;
        let timer_changed =
            target.visible && (!applied.visible || target.elapsed_secs != applied.elapsed_secs || pause_changed);

        if !target.visible {
            {
                let mut g = shared.lock().unwrap();
                g.snapshot = target.clone();
                g.icon_path.clear();
                g.accessible_desc = "Vexrec Idle".to_string();
            }
            cache.clear();
            let item_guard = item_iface.get().await;
            let ctxt = item_iface.signal_emitter();
            let _ = item_guard.status_changed(ctxt).await;
            let _ = VexrecStatusNotifierItem::new_status(ctxt, "Passive").await;
        } else {
            let mut new_icon_path = None;
            if timer_changed {
                new_icon_path = cache.write_timer(target.elapsed_secs, target.paused);
            }
            let desc = format!("Vexrec Recording {}", format_timer_text(target.elapsed_secs));
            {
                let mut g = shared.lock().unwrap();
                g.snapshot = target.clone();
                if let Some(ref p) = new_icon_path {
                    g.icon_path = p.clone();
                }
                g.accessible_desc = desc;
                if pause_changed {
                    g.menu_revision = g.menu_revision.wrapping_add(1);
                }
            }

            // Refresh watcher state lazily when starting recording in case shell finished startup late.
            if status_changed && !item_registered.load(Ordering::Relaxed) {
                let reg = register_with_watcher(&conn).await;
                item_registered.store(reg, Ordering::Relaxed);
                host_registered.store(query_host_registered(&conn).await, Ordering::Relaxed);
            }

            let item_guard = item_iface.get().await;
            let ctxt = item_iface.signal_emitter();
            if status_changed {
                let _ = item_guard.status_changed(ctxt).await;
                let _ = VexrecStatusNotifierItem::new_status(ctxt, "Active").await;
            }
            if timer_changed {
                let _ = item_guard.icon_name_changed(ctxt).await;
                let _ = item_guard.icon_accessible_desc_changed(ctxt).await;
                let _ = VexrecStatusNotifierItem::new_icon(ctxt).await;
            }
            if pause_changed {
                let rev;
                let pause_label;
                {
                    let g = shared.lock().unwrap();
                    rev = g.menu_revision;
                    pause_label = g.pause_label().to_string();
                }
                let menu_ctxt = menu_iface.signal_emitter();
                let mut props = HashMap::new();
                props.insert("label".to_string(), owned_str(&pause_label));
                let _ = VexrecDbusMenu::items_properties_updated(
                    menu_ctxt,
                    vec![(2, props)],
                    Vec::new(),
                )
                .await;
                let _ = VexrecDbusMenu::layout_updated(menu_ctxt, rev, 0).await;
            }
        }

        applied = target;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{format_timer_text, format_tray_timer_svg};

    #[test]
    fn test_tray_timer_svg_is_long_outlined_and_has_no_red_circle() {
        let svg_short = format_tray_timer_svg(5, false);
        assert!(svg_short.contains("width=\"92\" height=\"22\""));
        assert!(svg_short.contains(">00:05</text>"));
        assert!(svg_short.contains("fill=\"none\""));
        assert!(svg_short.contains("stroke=\"#ffffff\""));
        assert!(!svg_short.contains("<circle"));
        assert!(!svg_short.contains("#ef4444"));
        assert!(!svg_short.contains("#EF4444"));

        let svg_long = format_tray_timer_svg(3661, true);
        assert!(svg_long.contains("width=\"112\" height=\"22\""));
        assert!(svg_long.contains(">01:01:01</text>"));
        assert!(svg_long.contains("stroke=\"#a1a1aa\""));
        assert_eq!(format_timer_text(65), "01:05");
    }
}
