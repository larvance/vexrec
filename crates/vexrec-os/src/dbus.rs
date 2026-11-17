use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use futures_lite::stream::StreamExt;
use tracing::{info, warn};
use vexrec_core::error::{Result, VexrecError};
use zbus::blocking::{Connection as BlockingConn, Proxy as BlockingProxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Debug, Clone)]
pub struct PortalScreenCastStream {
    pub pipewire_fd: Arc<OwnedFd>,
    pub node_id: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

fn next_token(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

/// Grants non-interactive screenshot permissions in `PermissionStore` natively over `zbus` D-Bus.
pub fn ensure_portal_screenshot_permission() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let Ok(conn) = BlockingConn::session() else {
            return;
        };
        let Ok(proxy) = BlockingProxy::new(
            &conn,
            "org.freedesktop.impl.portal.PermissionStore",
            "/org/freedesktop/impl/portal/PermissionStore",
            "org.freedesktop.impl.portal.PermissionStore",
        ) else {
            return;
        };

        let perms: Vec<&str> = vec!["yes"];
        let data = Value::from(false);
        for app_id in ["", "vexrec", "net.larvance.vexrec", "net.aerovex.vexrec"] {
            let _: std::result::Result<(), zbus::Error> =
                proxy.call("SetPermission", &("screenshot", true, "screenshot", app_id, &perms));
            let _ = data.value_signature();
        }
    });
}

/// Waits on an XDG Desktop Portal `org.freedesktop.portal.Request` Response signal asynchronously via `zbus` + `async_io`.
async fn await_portal_response(
    conn: &zbus::Connection,
    req_path: &OwnedObjectPath,
) -> Result<(u32, HashMap<String, OwnedValue>)> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        req_path.as_ref(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|e| VexrecError::Dbus(format!("Portal Request proxy error: {e}")))?;

    let mut stream = proxy
        .receive_signal("Response")
        .await
        .map_err(|e| VexrecError::Dbus(format!("Portal Response signal subscribe error: {e}")))?;

    if let Some(msg) = stream.next().await {
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg
            .body()
            .deserialize()
            .map_err(|e| VexrecError::Dbus(format!("Bad Portal Response body: {e}")))?;
        return Ok((code, results));
    }

    Err(VexrecError::Portal("Portal Request stream ended unexpectedly".to_string()))
}

/// Pure-Rust XDG Desktop Portal Screenshot client via `zbus` (replaces `ashpd` & `gdbus`).
pub fn portal_screenshot(interactive: bool) -> Result<String> {
    if !interactive {
        ensure_portal_screenshot_permission();
    }

    async_io::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| VexrecError::Dbus(format!("Session D-Bus connect failed: {e}")))?;

        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Screenshot",
        )
        .await
        .map_err(|e| VexrecError::Portal(format!("Screenshot portal unavailable: {e}")))?;

        let token = next_token("vexrec_shot");
        let unique = conn
            .unique_name()
            .map(|u| u.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "0".to_string());
        let expected_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{token}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let req_proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            expected_path.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut stream = req_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("interactive", Value::from(interactive));

        let actual_path: OwnedObjectPath = proxy
            .call("Screenshot", &("", opts))
            .await
            .map_err(|e| VexrecError::Portal(format!("Screenshot D-Bus call failed: {e}")))?;

        let (code, results) = if actual_path == expected_path {
            let msg = stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No Screenshot Response signal".into()))?;
            msg.body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual_path).await?
        };

        if code != 0 {
            return Err(VexrecError::Portal(format!("Screenshot rejected or cancelled (code {code})")));
        }

        let uri_val = results
            .get("uri")
            .ok_or_else(|| VexrecError::Portal("Missing uri in Screenshot response".into()))?;
        let uri: String = uri_val
            .clone()
            .try_into()
            .map_err(|_| VexrecError::Portal("Invalid uri variant type".into()))?;
        Ok(uri)
    })
}

/// Pure-Rust XDG Desktop Portal ScreenCast client (`zbus`) returning PipeWire fd + node_id(s).
pub fn portal_screencast(include_cursor: bool, source_type_mask: u32) -> Result<Vec<PortalScreenCastStream>> {
    async_io::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| VexrecError::Dbus(format!("Session D-Bus connect failed: {e}")))?;

        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )
        .await
        .map_err(|e| VexrecError::Portal(format!("ScreenCast portal unavailable: {e}")))?;

        let unique = conn
            .unique_name()
            .map(|u| u.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "0".to_string());

        // 1. CreateSession
        let create_tok = next_token("sc_create");
        let sess_tok = next_token("sc_sess");
        let create_req_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{create_tok}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let req_p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            create_req_path.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let mut create_stream = req_p.receive_signal("Response").await.map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(create_tok.as_str()));
        opts.insert("session_handle_token", Value::from(sess_tok.as_str()));

        let actual_create: OwnedObjectPath = proxy
            .call("CreateSession", &(opts,))
            .await
            .map_err(|e| VexrecError::Portal(format!("CreateSession failed: {e}")))?;

        let (code, res) = if actual_create == create_req_path {
            create_stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No CreateSession response".into()))?
                .body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual_create).await?
        };
        if code != 0 {
            return Err(VexrecError::Portal(format!("CreateSession failed (code {code})")));
        }

        let session_str: String = res
            .get("session_handle")
            .ok_or_else(|| VexrecError::Portal("Missing session_handle".into()))?
            .clone()
            .try_into()
            .map_err(|_| VexrecError::Portal("Invalid session_handle type".into()))?;
        let session_path = OwnedObjectPath::try_from(session_str)
            .map_err(|e| VexrecError::Portal(e.to_string()))?;

        // 2. SelectSources (CursorMode: 1 = Hidden, 2 = Embedded; PersistMode: 0 = DoNot)
        let sel_tok = next_token("sc_sel");
        let sel_req_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{sel_tok}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;
        let sel_req_p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            sel_req_path.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let mut sel_stream = sel_req_p.receive_signal("Response").await.map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let cursor_mode: u32 = if include_cursor { 2 } else { 1 };
        let mut sel_opts: HashMap<&str, Value<'_>> = HashMap::new();
        sel_opts.insert("handle_token", Value::from(sel_tok.as_str()));
        sel_opts.insert("types", Value::from(source_type_mask));
        sel_opts.insert("multiple", Value::from(false));
        sel_opts.insert("cursor_mode", Value::from(cursor_mode));
        sel_opts.insert("persist_mode", Value::from(0u32));

        let actual_sel: OwnedObjectPath = proxy
            .call("SelectSources", &(session_path.as_ref(), sel_opts))
            .await
            .map_err(|e| VexrecError::Portal(format!("SelectSources failed: {e}")))?;

        let (scode, _) = if actual_sel == sel_req_path {
            sel_stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No SelectSources response".into()))?
                .body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual_sel).await?
        };
        if scode != 0 {
            return Err(VexrecError::Portal(format!("SelectSources denied (code {scode})")));
        }

        // 3. Start
        let start_tok = next_token("sc_start");
        let start_req_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{start_tok}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;
        let start_req_p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            start_req_path.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let mut start_stream = start_req_p.receive_signal("Response").await.map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut start_opts: HashMap<&str, Value<'_>> = HashMap::new();
        start_opts.insert("handle_token", Value::from(start_tok.as_str()));

        let actual_start: OwnedObjectPath = proxy
            .call("Start", &(session_path.as_ref(), "", start_opts))
            .await
            .map_err(|e| VexrecError::Portal(format!("ScreenCast Start failed: {e}")))?;

        let (start_code, start_res) = if actual_start == start_req_path {
            start_stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No Start response".into()))?
                .body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual_start).await?
        };
        if start_code != 0 {
            return Err(VexrecError::Portal(format!("ScreenCast Start cancelled (code {start_code})")));
        }

        // 4. OpenPipeWireRemote -> OwnedFd
        let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
        let pw_fd: zbus::zvariant::OwnedFd = proxy
            .call("OpenPipeWireRemote", &(session_path.as_ref(), empty_opts))
            .await
            .map_err(|e| VexrecError::Portal(format!("OpenPipeWireRemote failed: {e}")))?;
        let owned_fd: OwnedFd = pw_fd.into();
        let shared_fd = Arc::new(owned_fd);

        let streams_val = start_res
            .get("streams")
            .ok_or_else(|| VexrecError::Portal("Missing streams in ScreenCast Start response".into()))?;

        let streams: Vec<(u32, HashMap<String, OwnedValue>)> = streams_val
            .clone()
            .try_into()
            .map_err(|_| VexrecError::Portal("Unexpected streams schema in ScreenCast response".into()))?;

        let mut out = Vec::new();
        for (node_id, props) in streams {
            let size: Option<(i32, i32)> = props
                .get("size")
                .and_then(|v| v.clone().try_into().ok());
            info!(node_id, ?size, "Acquired PipeWire ScreenCast node via zbus portal");
            out.push(PortalScreenCastStream {
                pipewire_fd: shared_fd.clone(),
                node_id,
                width: size.map(|(w, _)| w.max(0) as u32),
                height: size.map(|(_, h)| h.max(0) as u32),
            });
        }
        Ok(out)
    })
}

/// Invokes `org.freedesktop.portal.FileChooser` natively via `zbus` (replaces GTK FileChooser).
pub fn portal_choose_folder(title: &str) -> Result<Option<PathBuf>> {
    async_io::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.FileChooser",
        )
        .await
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let token = next_token("fc_dir");
        let unique = conn
            .unique_name()
            .map(|u| u.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "0".to_string());
        let expected = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{token}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let req_p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            expected.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let mut stream = req_p.receive_signal("Response").await.map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("directory", Value::from(true));

        let actual: OwnedObjectPath = proxy
            .call("OpenFile", &("", title, opts))
            .await
            .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let (code, res) = if actual == expected {
            stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No FileChooser response".into()))?
                .body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual).await?
        };
        if code != 0 {
            return Ok(None);
        }
        if let Some(uris_val) = res.get("uris") {
            if let Ok(uris) = Vec::<String>::try_from(uris_val.clone()) {
                if let Some(first) = uris.first() {
                    if let Some(path) = first.strip_prefix("file://") {
                        return Ok(Some(PathBuf::from(percent_decode(path))));
                    }
                }
            }
        }
        Ok(None)
    })
}

/// Invokes `org.freedesktop.portal.FileChooser::SaveFile` natively via `zbus`.
pub fn portal_save_file(title: &str, current_name: &str) -> Result<Option<PathBuf>> {
    async_io::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.FileChooser",
        )
        .await
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let token = next_token("fc_save");
        let unique = conn
            .unique_name()
            .map(|u| u.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "0".to_string());
        let expected = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{unique}/{token}"
        ))
        .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let req_p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            expected.as_ref(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let mut stream = req_p.receive_signal("Response").await.map_err(|e| VexrecError::Dbus(e.to_string()))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(token.as_str()));
        opts.insert("current_name", Value::from(current_name));

        let actual: OwnedObjectPath = proxy
            .call("SaveFile", &("", title, opts))
            .await
            .map_err(|e| VexrecError::Portal(e.to_string()))?;

        let (code, res) = if actual == expected {
            stream
                .next()
                .await
                .ok_or_else(|| VexrecError::Portal("No SaveFile response".into()))?
                .body()
                .deserialize()
                .map_err(|e| VexrecError::Dbus(e.to_string()))?
        } else {
            await_portal_response(&conn, &actual).await?
        };
        if code != 0 {
            return Ok(None);
        }
        if let Some(uris_val) = res.get("uris") {
            if let Ok(uris) = Vec::<String>::try_from(uris_val.clone()) {
                if let Some(first) = uris.first() {
                    if let Some(path) = first.strip_prefix("file://") {
                        return Ok(Some(PathBuf::from(percent_decode(path))));
                    }
                }
            }
        }
        Ok(None)
    })
}

/// Opens a captured file via `org.freedesktop.portal.OpenURI::OpenFile` (`OwnedFd`) natively without shelling out.
pub fn open_path_via_portal_or_mime(path: &Path) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let fd: OwnedFd = file.into();
    let res = async_io::block_on(async {
        let conn = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.OpenURI",
        )
        .await?;
        let opts: HashMap<&str, Value<'_>> = HashMap::new();
        let _: OwnedObjectPath = proxy
            .call("OpenFile", &("", zbus::zvariant::Fd::from(&fd), opts))
            .await?;
        Ok::<(), zbus::Error>(())
    });

    if let Err(e) = res {
        warn!("OpenURI portal fallback ({e}), trying xdg-open");
        let _ = std::process::Command::new("xdg-open")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Pure-Rust `org.freedesktop.Notifications` daemon client with persistent `ActionInvoked` handling.
pub struct NotificationHub {
    pending: Arc<Mutex<HashMap<u32, PathBuf>>>,
}

impl NotificationHub {
    pub fn start() -> Self {
        let pending: Arc<Mutex<HashMap<u32, PathBuf>>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_bg = pending.clone();

        std::thread::Builder::new()
            .name("vexrec-dbus-notify".into())
            .spawn(move || {
                let _ = async_io::block_on(async move {
                    let conn = zbus::Connection::session().await?;
                    let proxy = zbus::Proxy::new(
                        &conn,
                        "org.freedesktop.Notifications",
                        "/org/freedesktop/Notifications",
                        "org.freedesktop.Notifications",
                    )
                    .await?;

                    let mut actions_stream = proxy.receive_signal("ActionInvoked").await?;
                    let mut closed_stream = proxy.receive_signal("NotificationClosed").await?;

                    loop {
                        futures_lite::future::or(
                            async {
                                if let Some(msg) = actions_stream.next().await {
                                    if let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() {
                                        if action == "default" || action == "open" {
                                            if let Some(p) = pending_bg.lock().unwrap().remove(&id) {
                                                std::thread::spawn(move || open_path_via_portal_or_mime(&p));
                                            }
                                        }
                                    }
                                }
                            },
                            async {
                                if let Some(msg) = closed_stream.next().await {
                                    if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                                        pending_bg.lock().unwrap().remove(&id);
                                    }
                                }
                            },
                        )
                        .await;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), zbus::Error>(())
                });
            })
            .ok();

        Self { pending }
    }

    pub fn notify_saved(&self, icon: &str, title: &str, body: &str, path: PathBuf) {
        let pending = self.pending.clone();
        let icon = icon.to_string();
        let title = title.to_string();
        let body = body.to_string();
        let is_video = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mkv" | "webm" | "gif"))
            .unwrap_or(false);
        let label = if is_video { "Open Video" } else { "Open Screenshot" };

        std::thread::spawn(move || {
            let Ok(conn) = BlockingConn::session() else {
                return;
            };
            let Ok(proxy) = BlockingProxy::new(
                &conn,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            ) else {
                return;
            };
            let actions = vec!["default", label, "open", label];
            let hints: HashMap<&str, Value<'_>> = HashMap::new();
            let res: std::result::Result<u32, _> = proxy.call(
                "Notify",
                &(
                    "Vexrec",
                    0u32,
                    icon.as_str(),
                    title.as_str(),
                    body.as_str(),
                    actions,
                    hints,
                    8000i32,
                ),
            );
            if let Ok(nid) = res {
                let mut map = pending.lock().unwrap();
                if map.len() > 32 {
                    map.clear();
                }
                map.insert(nid, path);
            }
        });
    }
}

/// KDE Plasma Wayland zero-prompt screenshot capture over `org.kde.kwin.Screenshot2`.
pub fn kwin_capture_workspace_pipe(include_cursor: bool) -> Result<(u32, u32, u32, Vec<u8>)> {
    let (read_fd, write_fd) = rustix::pipe::pipe()
        .map_err(|e| VexrecError::Io(std::io::Error::from_raw_os_error(e.raw_os_error())))?;

    let meta: HashMap<String, OwnedValue> = async_io::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| VexrecError::Dbus(e.to_string()))?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.kde.KWin",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
        )
        .await
        .map_err(|e| VexrecError::Display(format!("KWin ScreenShot2 unavailable: {e}")))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("include-cursor", Value::from(include_cursor));
        opts.insert("native-resolution", Value::from(true));

        let reply: HashMap<String, OwnedValue> = proxy
            .call(
                "CaptureWorkspace",
                &(opts, zbus::zvariant::Fd::from(&write_fd)),
            )
            .await
            .map_err(|e| VexrecError::Display(format!("KWin CaptureWorkspace failed: {e}")))?;
        Ok::<_, VexrecError>(reply)
    })?;

    drop(write_fd);
    let mut file = std::fs::File::from(read_fd);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;

    let width: u32 = meta
        .get("width")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or(0);
    let height: u32 = meta
        .get("height")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or(0);
    let stride: u32 = meta
        .get("stride")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or(width * 4);

    if width == 0 || height == 0 || bytes.is_empty() {
        return Err(VexrecError::Display("Empty KWin screenshot buffer".to_string()));
    }
    Ok((width, height, stride, bytes))
}

/// Connects to `org.freedesktop.portal.GlobalShortcuts` for Wayland system keybinds (calls `on_trigger` with shortcut id).
pub fn spawn_portal_global_shortcuts<F>(on_trigger: F)
where
    F: Fn(String) + Send + 'static,
{
    std::thread::Builder::new()
        .name("vexrec-portal-keys".into())
        .spawn(move || {
            let _ = async_io::block_on(async move {
                let Ok(conn) = zbus::Connection::session().await else {
                    return;
                };
                let Ok(proxy) = zbus::Proxy::new(
                    &conn,
                    "org.freedesktop.portal.Desktop",
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.portal.GlobalShortcuts",
                )
                .await else {
                    return;
                };

                let create_tok = next_token("gs_create");
                let sess_tok = next_token("gs_sess");
                let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                opts.insert("handle_token", Value::from(create_tok.as_str()));
                opts.insert("session_handle_token", Value::from(sess_tok.as_str()));

                let Ok(req_path) = proxy.call::<_, _, OwnedObjectPath>("CreateSession", &(opts,)).await else {
                    return;
                };
                let Ok((code, res)) = await_portal_response(&conn, &req_path).await else {
                    return;
                };
                if code != 0 {
                    return;
                }
                let Some(sess_str) = res.get("session_handle").and_then(|v| String::try_from(v.clone()).ok()) else {
                    return;
                };
                let Ok(sess_path) = OwnedObjectPath::try_from(sess_str) else {
                    return;
                };

                let mut shortcut_defs: Vec<(&str, HashMap<&str, Value<'_>>)> = Vec::new();
                let mut m1 = HashMap::new();
                m1.insert("description", Value::from("Trigger Vexrec Freeze Capture"));
                m1.insert("preferred_trigger", Value::from("Print"));
                shortcut_defs.push(("freeze", m1));

                let mut m2 = HashMap::new();
                m2.insert("description", Value::from("Stop Active Vexrec Recording"));
                m2.insert("preferred_trigger", Value::from("SHIFT+Print"));
                shortcut_defs.push(("stop_recording", m2));

                let bind_opts: HashMap<&str, Value<'_>> = HashMap::new();
                let _ = proxy
                    .call::<_, _, OwnedObjectPath>("BindShortcuts", &(sess_path.as_ref(), shortcut_defs, "", bind_opts))
                    .await;

                if let Ok(mut act_stream) = proxy.receive_signal("Activated").await {
                    while let Some(msg) = act_stream.next().await {
                        if let Ok((_sess, id, _ts, _opts)) =
                            msg.body().deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>()
                        {
                            on_trigger(id);
                        }
                    }
                }
            });
        })
        .ok();
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
