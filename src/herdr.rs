// Minimal client for herdr's socket API: one connection per request,
// one JSON line out, one JSON line back (same framing the official plugins use).
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.

use serde_json::{json, Map, Value};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, PLUGIN_ID};

const MAX_RESPONSE: u64 = 16 << 20;

pub struct Herdr {
    pub socket: PathBuf,
    counter: u64,
    last_seq: u64,
}

impl Herdr {
    pub fn from_env() -> Self {
        let socket = env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| config::home().join(".config"))
                    .join("herdr/herdr.sock")
            });
        Herdr { socket, counter: 0, last_seq: 0 }
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.counter += 1;
        let id = format!("{PLUGIN_ID}:{}:{}", std::process::id(), self.counter);
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("connect {}: {e}", self.socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
            .map_err(|e| e.to_string())?;
        line.push(b'\n');
        (&stream).write_all(&line).map_err(|e| format!("{method}: write: {e}"))?;
        let mut reply = String::new();
        BufReader::new(stream.take(MAX_RESPONSE))
            .read_line(&mut reply)
            .map_err(|e| format!("{method}: read: {e}"))?;
        let value: Value =
            serde_json::from_str(&reply).map_err(|e| format!("{method}: bad JSON: {e}"))?;
        if let Some(err) = value.get("error") {
            let msg = err.get("message").and_then(Value::as_str).map(str::to_string);
            return Err(format!("{method}: {}", msg.unwrap_or_else(|| err.to_string())));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn panes(&mut self) -> Result<Vec<Value>, String> {
        let result = self.request("pane.list", json!({}))?;
        match result.get("panes") {
            Some(Value::Array(list)) => Ok(list.clone()),
            _ => Err("pane.list returned no pane list".into()),
        }
    }

    pub fn workspace_labels(&mut self) -> Vec<(String, String)> {
        let Ok(result) = self.request("workspace.list", json!({})) else {
            return Vec::new();
        };
        result
            .get("workspaces")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|w| {
                        Some((
                            w.get("workspace_id")?.as_str()?.to_string(),
                            w.get("label")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn shell_pid(&mut self, pane_id: &str) -> Result<Option<u32>, String> {
        let result = self.request("pane.process_info", json!({"pane_id": pane_id}))?;
        Ok(result
            .get("process_info")
            .and_then(|p| p.get("shell_pid"))
            .and_then(Value::as_u64)
            .map(|v| v as u32))
    }

    /// Patches the three badge tokens of one pane. `None` values clear a token.
    pub fn report(
        &mut self,
        pane_id: &str,
        tokens: Map<String, Value>,
        ttl_ms: Option<u64>,
    ) -> Result<(), String> {
        // Wall-clock milliseconds keep the sequence rising across daemon restarts;
        // the max() keeps it strictly rising within one process.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_seq = now_ms.max(self.last_seq + 1);
        let seq = self.last_seq;
        let mut params = json!({
            "pane_id": pane_id,
            "source": PLUGIN_ID,
            "tokens": tokens,
            "seq": seq,
        });
        if let Some(ttl) = ttl_ms {
            params["ttl_ms"] = json!(ttl);
        }
        self.request("pane.report_metadata", params).map(|_| ())
    }
}
