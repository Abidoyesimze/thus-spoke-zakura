//! One JSON-RPC call against the instance's Zakura endpoint.

use super::*;

const RPC_TIMEOUT: Duration = Duration::from_secs(300);
const STOP_REFUSAL: &str = "refusing to call stop without --yes; it shuts down the Zakura node";

impl Runtime {
    pub fn rpc(
        &self,
        name: &InstanceName,
        method: &str,
        params: &[String],
        raw: bool,
        yes: bool,
    ) -> Result<()> {
        // Refuse before looking the instance up, so a missing confirmation
        // cannot reach the node.
        confirm_stop(method, yes)?;
        let url = self.read_instance(name)?.endpoints.rpc;
        if !container_running(&format!("{}-zakura", prefix(name)))? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        println!("{}", call_rpc(&url, method, params, raw)?);
        Ok(())
    }
}

/// `stop` shuts Zakura down. Letter case does not make it a different method.
fn confirm_stop(method: &str, yes: bool) -> Result<()> {
    if method.eq_ignore_ascii_case("stop") && !yes {
        bail!("{STOP_REFUSAL}");
    }
    Ok(())
}

/// Instance metadata names a loopback Zakura port. Do not follow a rewritten
/// URL off this machine.
fn ensure_loopback(url: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).with_context(|| format!("invalid Zakura RPC url {url}"))?;
    match parsed.host_str() {
        Some("127.0.0.1" | "localhost" | "::1") => Ok(()),
        _ => bail!("refusing non-loopback Zakura RPC url {url}"),
    }
}

/// JSON literals stay JSON. Any other argument is a string, as in `cast rpc`.
fn parse_params(params: &[String]) -> Vec<serde_json::Value> {
    params
        .iter()
        .map(|param| {
            serde_json::from_str(param).unwrap_or_else(|_| serde_json::Value::String(param.clone()))
        })
        .collect()
}

fn call_rpc(url: &str, method: &str, params: &[String], raw: bool) -> Result<String> {
    ensure_loopback(url)?;
    let response = reqwest::blocking::Client::builder()
        .timeout(RPC_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the Zakura RPC client")?
        .post(url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": parse_params(params),
        }))
        .send()
        .with_context(|| format!("calling Zakura {method}"))?;
    let status = response.status();
    let body = response.bytes().context("reading Zakura response")?;
    let envelope: serde_json::Value = serde_json::from_slice(&body).with_context(|| {
        if status.is_success() {
            "decoding Zakura response".to_owned()
        } else {
            format!("Zakura returned HTTP {status}")
        }
    })?;
    if !status.is_success() && rpc_error_message(&envelope).is_none() {
        bail!("Zakura returned HTTP {status}");
    }
    format_rpc_output(raw, &envelope)
}

fn rpc_error_message(envelope: &serde_json::Value) -> Option<String> {
    let error = envelope.get("error").filter(|error| !error.is_null())?;
    Some(
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|message| !message.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| "Zakura RPC returned an error".to_owned()),
    )
}

fn format_rpc_output(raw: bool, envelope: &serde_json::Value) -> Result<String> {
    if let Some(message) = rpc_error_message(envelope) {
        bail!("{message}");
    }
    if raw {
        return Ok(serde_json::to_string_pretty(envelope)?);
    }
    let result = envelope
        .get("result")
        .context("Zakura RPC response has no result")?;
    match result {
        serde_json::Value::String(text) => Ok(text.clone()),
        other => Ok(serde_json::to_string_pretty(other)?),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/runtime/tests/rpc.rs"]
mod tests;
