#[cfg(unix)]
mod grid_scrollback;

#[cfg(unix)]
mod pty_bus;

#[cfg(unix)]
mod renderer;

#[cfg(unix)]
mod rpc;

#[cfg(unix)]
mod screen;

#[cfg(unix)]
mod session_manager;

#[cfg(unix)]
mod terminal_pane;

#[cfg(unix)]
mod vt_lite;

#[cfg(not(unix))]
fn main() {
    eprintln!("discode-pty-sidecar currently supports unix domain sockets only");
    std::process::exit(1);
}

#[cfg(unix)]
mod unix_main {
    use crate::pty_bus::{
        dispose_window, resize_window as resize_window_bus, spawn_window_process,
        stop_window as stop_window_bus, write_input,
    };
    use crate::session_manager::{
        idle_window_state, lock_state, lock_window, new_shared_state, window_key, with_window,
        SharedSidecarState,
    };
    use crate::vt_lite::build_styled_frame;
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Deserialize, Serialize)]
    struct RpcRequest {
        method: String,
        #[serde(default)]
        params: Value,
    }

    #[derive(Serialize)]
    struct RpcResponse {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }

    pub fn main() {
        let args = std::env::args().collect::<Vec<_>>();
        if args.len() < 2 {
            eprintln!("usage: discode-pty-sidecar <server|request> ...");
            std::process::exit(1);
        }

        match args[1].as_str() {
            "server" => {
                let socket = parse_flag(&args, "--socket").unwrap_or_else(|| {
                    eprintln!("missing --socket");
                    std::process::exit(1);
                });
                if let Err(err) = run_server(PathBuf::from(socket)) {
                    eprintln!("server error: {err}");
                    std::process::exit(1);
                }
            }
            "request" => {
                let socket = parse_flag(&args, "--socket").unwrap_or_else(|| {
                    eprintln!("missing --socket");
                    std::process::exit(1);
                });
                let method = parse_flag(&args, "--method").unwrap_or_else(|| {
                    eprintln!("missing --method");
                    std::process::exit(1);
                });
                let params = parse_flag(&args, "--params")
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .unwrap_or_else(|| json!({}));
                let req = RpcRequest { method, params };

                match send_request(Path::new(&socket), &req) {
                    Ok(value) => {
                        print!("{value}");
                    }
                    Err(err) => {
                        eprintln!("request error: {err}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                eprintln!("unknown command: {}", args[1]);
                std::process::exit(1);
            }
        }
    }

    fn parse_flag(args: &[String], name: &str) -> Option<String> {
        let idx = args.iter().position(|it| it == name)?;
        args.get(idx + 1).cloned()
    }

    fn send_request(socket_path: &Path, req: &RpcRequest) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path)
            .map_err(|e| format!("connect {}: {e}", socket_path.display()))?;

        let payload = serde_json::to_vec(req).map_err(|e| format!("encode request: {e}"))?;
        stream
            .write_all(&payload)
            .map_err(|e| format!("write request: {e}"))?;
        let _ = stream.shutdown(std::net::Shutdown::Write);

        let mut out = String::new();
        stream
            .read_to_string(&mut out)
            .map_err(|e| format!("read response: {e}"))?;
        Ok(out)
    }

    fn run_server(socket_path: PathBuf) -> Result<(), String> {
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create socket parent {}: {e}", parent.display()))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;
        let state = new_shared_state();
        let running = Arc::new(AtomicBool::new(true));

        while running.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(tuple) => tuple,
                Err(err) => return Err(format!("accept failed: {err}")),
            };

            let mut raw = String::new();
            if let Err(err) = stream.read_to_string(&mut raw) {
                let _ = write_response(
                    &mut stream,
                    &RpcResponse {
                        ok: false,
                        result: None,
                        error: Some(format!("failed to read request: {err}")),
                    },
                );
                continue;
            }

            let req = match serde_json::from_str::<RpcRequest>(&raw) {
                Ok(req) => req,
                Err(err) => {
                    let _ = write_response(
                        &mut stream,
                        &RpcResponse {
                            ok: false,
                            result: None,
                            error: Some(format!("invalid request JSON: {err}")),
                        },
                    );
                    continue;
                }
            };

            let mut should_shutdown = false;
            let response = match handle_request(&state, req, &mut should_shutdown) {
                Ok(value) => RpcResponse {
                    ok: true,
                    result: Some(value),
                    error: None,
                },
                Err(err) => RpcResponse {
                    ok: false,
                    result: None,
                    error: Some(err),
                },
            };

            let _ = write_response(&mut stream, &response);
            if should_shutdown {
                running.store(false, Ordering::SeqCst);
            }
        }

        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    fn write_response(stream: &mut UnixStream, response: &RpcResponse) -> Result<(), String> {
        let payload = serde_json::to_vec(response).map_err(|e| format!("encode response: {e}"))?;
        stream
            .write_all(&payload)
            .map_err(|e| format!("write response: {e}"))
    }

    fn handle_request(
        state: &SharedSidecarState,
        req: RpcRequest,
        should_shutdown: &mut bool,
    ) -> Result<Value, String> {
        match req.method.as_str() {
            "hello" => Ok(json!({ "version": 1 })),
            "get_or_create_session" => {
                let project_name = get_str(&req.params, "projectName")?;
                let first_window_name = get_opt_str(&req.params, "firstWindowName");

                let mut guard = lock_state(state);
                guard.sessions.entry(project_name.clone()).or_default();

                if let Some(window_name) = first_window_name {
                    let key = window_key(&project_name, &window_name);
                    guard.windows.entry(key).or_insert_with(|| {
                        Arc::new(Mutex::new(idle_window_state(
                            project_name.clone(),
                            window_name,
                        )))
                    });
                }

                Ok(json!({ "sessionName": project_name }))
            }
            "set_session_env" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let key = get_str(&req.params, "key")?;
                let value = get_str(&req.params, "value")?;

                let mut guard = lock_state(state);
                let env = guard.sessions.entry(session_name).or_default();
                env.insert(key, value);
                Ok(json!({ "ok": true }))
            }
            "window_exists" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let key = window_key(&session_name, &window_name);

                let guard = lock_state(state);
                Ok(json!({ "exists": guard.windows.contains_key(&key) }))
            }
            "start_window" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let command = get_str(&req.params, "command")?;

                start_window(state, session_name, window_name, command)?;
                Ok(json!({ "ok": true }))
            }
            "type_keys" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let keys = get_str(&req.params, "keys")?;
                with_window(state, &session_name, &window_name, |window| {
                    write_input(window, keys.as_bytes())
                })?;
                Ok(json!({ "ok": true }))
            }
            "send_enter" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                with_window(state, &session_name, &window_name, |window| {
                    write_input(window, b"\r")
                })?;
                Ok(json!({ "ok": true }))
            }
            "resize_window" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let cols = get_u16(&req.params, "cols", 140);
                let rows = get_u16(&req.params, "rows", 40);

                with_window(state, &session_name, &window_name, |window| {
                    resize_window_bus(window, cols, rows);
                    Ok(())
                })?;
                Ok(json!({ "ok": true }))
            }
            "list_windows" => {
                let session_filter = get_opt_str(&req.params, "sessionName");
                let windows = {
                    let guard = lock_state(state);
                    guard
                        .windows
                        .values()
                        .filter_map(|window| {
                            let w = window.lock().ok()?;
                            if let Some(ref session) = session_filter {
                                if &w.snapshot.session_name != session {
                                    return None;
                                }
                            }
                            Some(json!({
                                "sessionName": w.snapshot.session_name,
                                "windowName": w.snapshot.window_name,
                                "status": w.snapshot.status,
                                "pid": w.snapshot.pid,
                                "startedAt": w.snapshot.started_at,
                                "exitedAt": w.snapshot.exited_at,
                                "exitCode": w.snapshot.exit_code,
                                "signal": w.snapshot.signal,
                            }))
                        })
                        .collect::<Vec<_>>()
                };
                Ok(json!({ "windows": windows }))
            }
            "get_window_buffer" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let buffer = with_window(state, &session_name, &window_name, |window| {
                    Ok(window.buffer.clone())
                })?;
                Ok(json!({ "buffer": buffer }))
            }
            "get_window_frame" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;
                let requested_cols = get_opt_u16(&req.params, "cols");
                let requested_rows = get_opt_u16(&req.params, "rows");

                let frame = with_window(state, &session_name, &window_name, |window| {
                    let cols = requested_cols.unwrap_or(window.snapshot.cols);
                    let rows = requested_rows.unwrap_or(window.snapshot.rows);
                    Ok(build_styled_frame(&window.buffer, cols, rows))
                })?;
                Ok(frame)
            }
            "stop_window" => {
                let session_name = get_str(&req.params, "sessionName")?;
                let window_name = get_str(&req.params, "windowName")?;

                let stopped = with_window(state, &session_name, &window_name, |window| {
                    stop_window_bus(window)
                })?;

                Ok(json!({ "stopped": stopped }))
            }
            "dispose" => {
                let windows = {
                    let guard = lock_state(state);
                    guard.windows.values().cloned().collect::<Vec<_>>()
                };

                for window in windows {
                    if let Ok(mut window) = window.lock() {
                        dispose_window(&mut window);
                    }
                }

                *should_shutdown = true;
                Ok(json!({ "ok": true }))
            }
            _ => Err(format!("unknown method: {}", req.method)),
        }
    }

    fn get_str(params: &Value, key: &str) -> Result<String, String> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .ok_or_else(|| format!("missing or invalid '{key}'"))
    }

    fn get_opt_str(params: &Value, key: &str) -> Option<String> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
    }

    fn get_opt_u16(params: &Value, key: &str) -> Option<u16> {
        let value = params.get(key)?.as_u64()?;
        Some(value.clamp(10, 400) as u16)
    }

    fn get_u16(params: &Value, key: &str, default: u16) -> u16 {
        get_opt_u16(params, key).unwrap_or(default)
    }

    fn start_window(
        state: &SharedSidecarState,
        session_name: String,
        window_name: String,
        command: String,
    ) -> Result<(), String> {
        let key = window_key(&session_name, &window_name);

        let window = {
            let mut guard = lock_state(state);
            guard
                .windows
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(Mutex::new(idle_window_state(
                        session_name.clone(),
                        window_name.clone(),
                    )))
                })
                .clone()
        };

        {
            let mut w = lock_window(&window);
            if w.child.is_some() && w.snapshot.status == "running" {
                return Ok(());
            }
            w.snapshot.status = "starting".to_string();
            w.snapshot.started_at = Some(now_unix_seconds());
            w.snapshot.exited_at = None;
            w.snapshot.exit_code = None;
            w.snapshot.signal = None;
            w.buffer.clear();
        }

        spawn_window_process(state, &window, &session_name, command)?;

        Ok(())
    }

    fn now_unix_seconds() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::thread;
        use std::time::Duration;

        struct Cleanup(SharedSidecarState);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                let mut should_shutdown = false;
                let _ = handle_request(
                    &self.0,
                    RpcRequest {
                        method: "dispose".to_string(),
                        params: json!({}),
                    },
                    &mut should_shutdown,
                );
            }
        }

        fn call(state: &SharedSidecarState, method: &str, params: Value) -> Value {
            let mut should_shutdown = false;
            handle_request(
                state,
                RpcRequest {
                    method: method.to_string(),
                    params,
                },
                &mut should_shutdown,
            )
            .unwrap_or_else(|err| panic!("{method} failed: {err}"))
        }

        #[test]
        fn preserves_session_and_window_registry_methods() {
            let state = new_shared_state();
            let _cleanup = Cleanup(state.clone());

            let created = call(
                &state,
                "get_or_create_session",
                json!({ "projectName": "proj-a", "firstWindowName": "win-a" }),
            );
            assert_eq!(created["sessionName"].as_str(), Some("proj-a"));

            let exists = call(
                &state,
                "window_exists",
                json!({ "sessionName": "proj-a", "windowName": "win-a" }),
            );
            assert_eq!(exists["exists"].as_bool(), Some(true));

            let listed = call(&state, "list_windows", json!({ "sessionName": "proj-a" }));
            let windows = listed["windows"]
                .as_array()
                .expect("windows should be array");
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0]["status"].as_str(), Some("idle"));
        }

        #[test]
        fn preserves_window_io_methods_through_pty_bus() {
            let state = new_shared_state();
            let _cleanup = Cleanup(state.clone());

            call(
                &state,
                "get_or_create_session",
                json!({ "projectName": "proj-b", "firstWindowName": "win-b" }),
            );

            call(
                &state,
                "start_window",
                json!({
                    "sessionName": "proj-b",
                    "windowName": "win-b",
                    "command": "cat"
                }),
            );

            call(
                &state,
                "type_keys",
                json!({
                    "sessionName": "proj-b",
                    "windowName": "win-b",
                    "keys": "hello-rpc"
                }),
            );
            call(
                &state,
                "send_enter",
                json!({ "sessionName": "proj-b", "windowName": "win-b" }),
            );

            let mut saw_echo = false;
            for _ in 0..40 {
                let buffer = call(
                    &state,
                    "get_window_buffer",
                    json!({ "sessionName": "proj-b", "windowName": "win-b" }),
                );
                if buffer["buffer"]
                    .as_str()
                    .map(|text| text.contains("hello-rpc"))
                    .unwrap_or(false)
                {
                    saw_echo = true;
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            assert!(saw_echo, "expected echoed input in window buffer");

            call(
                &state,
                "resize_window",
                json!({
                    "sessionName": "proj-b",
                    "windowName": "win-b",
                    "cols": 100,
                    "rows": 30
                }),
            );

            let frame = call(
                &state,
                "get_window_frame",
                json!({ "sessionName": "proj-b", "windowName": "win-b" }),
            );
            assert_eq!(frame["cols"].as_u64(), Some(100));
            assert_eq!(frame["rows"].as_u64(), Some(30));

            let stopped = call(
                &state,
                "stop_window",
                json!({ "sessionName": "proj-b", "windowName": "win-b" }),
            );
            assert_eq!(stopped["stopped"].as_bool(), Some(true));
        }
    }
}

#[cfg(unix)]
fn main() {
    unix_main::main();
}
