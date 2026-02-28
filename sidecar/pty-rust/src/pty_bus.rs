use crate::session_manager::{
    lock_state, lock_window, SharedSidecarState, SharedWindowState, WindowState,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn write_input(window: &mut WindowState, input: &[u8]) -> Result<(), String> {
    let writer = window
        .writer
        .as_mut()
        .ok_or_else(|| "window writer unavailable".to_string())?;
    writer
        .write_all(input)
        .map_err(|e| format!("write input failed: {e}"))?;
    writer.flush().map_err(|e| format!("flush failed: {e}"))
}

pub fn resize_window(window: &mut WindowState, cols: u16, rows: u16) {
    if let Some(master) = window.master.as_mut() {
        let _ = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    window.snapshot.cols = cols;
    window.snapshot.rows = rows;
}

pub fn stop_window(window: &mut WindowState) -> Result<bool, String> {
    if let Some(child) = window.child.as_mut() {
        child.kill().map_err(|e| format!("kill failed: {e}"))?;
        window.snapshot.status = "exited".to_string();
        window.snapshot.exited_at = Some(now_unix_seconds());
        window.snapshot.signal = Some("SIGTERM".to_string());
        window.child = None;
        window.master = None;
        window.writer = None;
        return Ok(true);
    }

    Ok(false)
}

pub fn dispose_window(window: &mut WindowState) {
    if let Some(child) = window.child.as_mut() {
        let _ = child.kill();
    }
    window.child = None;
    window.writer = None;
    window.master = None;
    window.snapshot.status = "exited".to_string();
    window.snapshot.exited_at = Some(now_unix_seconds());
}

pub fn spawn_window_process(
    state: &SharedSidecarState,
    window: &SharedWindowState,
    session_name: &str,
    command: String,
) -> Result<(), String> {
    let env = {
        let guard = lock_state(state);
        guard
            .sessions
            .get(session_name)
            .cloned()
            .unwrap_or_default()
    };

    let (cols, rows) = {
        let w = lock_window(window);
        (w.snapshot.cols, w.snapshot.rows)
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty failed: {e}"))?;

    let mut cmd = CommandBuilder::new(shell);
    cmd.arg("-lc");
    cmd.arg(command);
    cmd.cwd(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    cmd.env(
        "TERM",
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
    );
    cmd.env(
        "COLORTERM",
        std::env::var("COLORTERM").unwrap_or_else(|_| "truecolor".to_string()),
    );
    cmd.env("COLUMNS", cols.to_string());
    cmd.env("LINES", rows.to_string());
    for (k, v) in env {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn failed: {e}"))?;
    let pid = child.process_id();
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone reader failed: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take writer failed: {e}"))?;

    {
        let mut w = lock_window(window);
        w.snapshot.status = "running".to_string();
        w.snapshot.pid = pid;
        w.master = Some(pair.master);
        w.child = Some(child);
        w.writer = Some(writer);
        w.buffer.push_str(&format!(
            "[runtime] process started (pid={})\n",
            pid.unwrap_or(0)
        ));
    }

    let max_buffer = {
        let guard = lock_state(state);
        guard.max_buffer_bytes
    };

    let read_window = window.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    if let Ok(mut w) = read_window.lock() {
                        if w.snapshot.status == "running" || w.snapshot.status == "starting" {
                            w.snapshot.status = "exited".to_string();
                            w.snapshot.exited_at = Some(now_unix_seconds());
                        }
                    }
                    break;
                }
                Ok(n) => {
                    if let Ok(mut w) = read_window.lock() {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        w.buffer.push_str(&text);
                        if w.buffer.len() > max_buffer {
                            trim_buffer_to_max_bytes(&mut w.buffer, max_buffer);
                        }
                    }
                }
                Err(_) => {
                    if let Ok(mut w) = read_window.lock() {
                        w.snapshot.status = "error".to_string();
                        w.snapshot.exited_at = Some(now_unix_seconds());
                    }
                    break;
                }
            }
        }
    });

    Ok(())
}

fn trim_buffer_to_max_bytes(buffer: &mut String, max_bytes: usize) {
    if buffer.len() <= max_bytes {
        return;
    }

    let overflow = buffer.len() - max_bytes;
    let mut start = overflow;
    while start < buffer.len() && !buffer.is_char_boundary(start) {
        start += 1;
    }

    buffer.drain(..start);
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
