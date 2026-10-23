//! AcoustID audio fingerprinting (https://acoustid.org/webservice).
//! Fingerprints come from chromaprint's `fpcalc`.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tracing::debug;

pub struct Client {
    agent: ureq::Agent,
    api_key: String,
    next_request: Instant,
}

/// One AcoustID result: the AcoustID track id, its score (0-100) and the
/// MusicBrainz recordings linked to it.
#[derive(Debug, Clone)]
pub struct Hit {
    pub acoustid: String,
    pub score: u8,
    pub recordings: Vec<String>,
}

#[derive(Deserialize)]
struct Fpcalc {
    duration: f64,
    fingerprint: String,
}

#[derive(Deserialize)]
struct Response {
    status: String,
    #[serde(default)]
    results: Vec<ResultItem>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

#[derive(Deserialize)]
struct ResultItem {
    id: String,
    score: f64,
    #[serde(default)]
    recordings: Vec<RecordingId>,
}

#[derive(Deserialize)]
struct RecordingId {
    id: String,
}

impl Client {
    pub fn new(api_key: &str) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!(
                "AcoustID needs an API key: register an application at https://acoustid.org/new-application \
                 and put its key in identify.acoustid.api_key in .musort/config.toml"
            );
        }
        let agent = ureq::AgentBuilder::new()
            .user_agent(&format!("musort/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build();
        Ok(Client { agent, api_key: api_key.trim().to_string(), next_request: Instant::now() })
    }

    pub fn lookup(&mut self, path: &Path) -> Result<Vec<Hit>> {
        let fp = fingerprint(path)?;
        crate::ui::step(crate::ui::Lane::AcoustId, "looking up the fingerprint");
        // AcoustID allows 3 requests per second.
        std::thread::sleep(self.next_request.saturating_duration_since(Instant::now()));
        self.next_request = Instant::now() + Duration::from_millis(350);

        let duration = (fp.duration.round() as u64).to_string();
        debug!("    acoustid: lookup with a {duration}s fingerprint");
        crate::ui::lane_note(crate::ui::Lane::AcoustId, "waiting for AcoustID's answer");
        let resp = self
            .agent
            .post("https://api.acoustid.org/v2/lookup")
            .send_form(&[
                ("client", self.api_key.as_str()),
                ("format", "json"),
                ("meta", "recordingids"),
                ("duration", duration.as_str()),
                ("fingerprint", fp.fingerprint.as_str()),
            ]);
        let body: Response = match resp {
            Ok(r) => r.into_json()?,
            // AcoustID reports errors (e.g. a bad key) as JSON with a 400.
            Err(ureq::Error::Status(_, r)) => r.into_json().context("unexpected response from AcoustID")?,
            Err(e) => return Err(anyhow!(e).context("can't reach AcoustID")),
        };
        if body.status != "ok" {
            bail!("AcoustID: {}", body.error.map(|e| e.message).unwrap_or(body.status));
        }
        let mut hits: Vec<Hit> = body
            .results
            .into_iter()
            .map(|r| Hit {
                acoustid: r.id,
                score: (r.score * 100.0).round().clamp(0.0, 100.0) as u8,
                recordings: r.recordings.into_iter().map(|r| r.id).collect(),
            })
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.score));
        Ok(hits)
    }
}

fn fingerprint(path: &Path) -> Result<Fpcalc> {
    let out = Command::new("fpcalc")
        .arg("-json")
        .arg(path)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("AcoustID needs `fpcalc` (from chromaprint) on PATH, and it wasn't found")
            } else {
                anyhow!(e).context("can't run fpcalc")
            }
        })?;
    if !out.status.success() {
        bail!(
            "fpcalc failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("unexpected output from fpcalc")
}
