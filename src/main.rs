// SPDX-License-Identifier: GPL-3.0-only

use bliss_playlist_guidance_spi::{
    encode, Candidate, Capability, Diagnostics, GuidanceRequest, GuidanceResponse, GuidanceScope,
    GuidanceSignal, Manifest, PROTOCOL_NAME, SPI_VERSION,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};

const PROVIDER_ID: &str = "playcount-guidance";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct PlayCountArtifact {
    schema_version: u8,
    schema_identity: String,
    #[serde(default)]
    generated_at: u64,
    #[serde(default)]
    database_cache_identity: String,
    tracks: Vec<PlayCountTrack>,
}

#[derive(Debug, Deserialize)]
struct PlayCountTrack {
    database_file: String,
    play_count: Option<u64>,
}

#[derive(Default)]
struct Provider {
    counts: HashMap<String, Option<u64>>,
    percentiles: HashMap<String, f64>,
    snapshot_id: Option<String>,
    prepared: bool,
}

impl Provider {
    fn manifest() -> Manifest {
        Manifest {
            spi_version: SPI_VERSION,
            provider_id: PROVIDER_ID.to_owned(),
            provider_version: PROVIDER_VERSION.to_owned(),
            protocol: PROTOCOL_NAME.to_owned(),
            capabilities: vec![Capability::GlobalCandidateGuidance],
            required_context: vec!["candidate_identity".to_owned()],
            configuration_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_path": {"type": "string", "minLength": 1}
                },
                "required": ["artifact_path"],
                "additionalProperties": false
            })),
        }
    }

    fn prepare(&mut self, options: &Value) -> Result<(Option<String>, Diagnostics), String> {
        let artifact_path = options
            .get("artifact_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "options.artifact_path is required".to_owned())?;
        let bytes = fs::read(artifact_path)
            .map_err(|error| format!("cannot read play-count artifact: {error}"))?;
        let artifact: PlayCountArtifact = serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot decode play-count artifact: {error}"))?;
        if artifact.schema_version != 1 || artifact.schema_identity != "lms-play-counts-v1" {
            return Err("unsupported play-count artifact schema".to_owned());
        }

        self.counts.clear();
        for track in artifact.tracks {
            self.counts.insert(track.database_file, track.play_count);
        }

        let mut values: Vec<u64> = self
            .counts
            .values()
            .map(|value| value.unwrap_or(0))
            .collect();
        values.sort_unstable();
        values.dedup();
        self.percentiles = self
            .counts
            .iter()
            .map(|(database_file, value)| {
                let normalized = value.unwrap_or(0);
                let percentile = if values.len() <= 1 {
                    0.0
                } else {
                    values.binary_search(&normalized).unwrap_or(0) as f64
                        / (values.len() - 1) as f64
                };
                (database_file.clone(), percentile)
            })
            .collect();

        let snapshot_id = format!(
            "{}:{}:{}",
            artifact.database_cache_identity,
            artifact.generated_at,
            values.len()
        );
        self.snapshot_id = Some(snapshot_id.clone());
        self.prepared = true;
        Ok((
            Some(snapshot_id),
            Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({
                    "known_tracks": self.counts.values().filter(|value| value.is_some()).count(),
                    "unknown_tracks": self.counts.values().filter(|value| value.is_none()).count(),
                    "distinct_play_counts": values.len(),
                })),
            },
        ))
    }

    fn score(&self, request_id: &str, candidates: &[Candidate]) -> GuidanceResponse {
        if !self.prepared {
            return GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "NOT_PREPARED".to_owned(),
                message: "provider must receive prepare before score".to_owned(),
                retryable: false,
            };
        }

        let signals: Vec<GuidanceSignal> = candidates
            .iter()
            .filter_map(|candidate| {
                let database_file = candidate.database_file.as_ref()?;
                let percentile = *self.percentiles.get(database_file)?;
                Some(
                    GuidanceSignal {
                        candidate_id: candidate.candidate_id.clone(),
                        scope: GuidanceScope::Global,
                        // Convert [0, 1] frequency percentile into a symmetric
                        // preference score. The optimizer applies the signed
                        // job weight; this addon remains provider-neutral.
                        score: (2.0 * percentile - 1.0).clamp(-1.0, 1.0),
                        confidence: 1.0,
                        rationale: Some(format!("LMS play-count percentile {:.3}", percentile)),
                        observed_at: None,
                    }
                    .bounded(),
                )
            })
            .collect();
        let matched = signals.len();
        GuidanceResponse::Scores {
            provider_id: PROVIDER_ID.to_owned(),
            request_id: request_id.to_owned(),
            signals,
            diagnostics: Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({"matched_candidates": matched})),
            },
        }
    }
}

fn handle(provider: &mut Provider, request: GuidanceRequest) -> GuidanceResponse {
    match request {
        GuidanceRequest::Describe { spi_version } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            GuidanceResponse::Manifest(Provider::manifest())
        }
        GuidanceRequest::Prepare {
            spi_version,
            options,
            ..
        } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            match provider.prepare(&options) {
                Ok((snapshot_id, diagnostics)) => GuidanceResponse::Prepared {
                    provider_id: PROVIDER_ID.to_owned(),
                    snapshot_id,
                    diagnostics,
                },
                Err(message) => GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "PREPARE_FAILED".to_owned(),
                    message,
                    retryable: true,
                },
            }
        }
        GuidanceRequest::Score {
            spi_version,
            request_id,
            candidates,
            ..
        } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            provider.score(&request_id, &candidates)
        }
        GuidanceRequest::Close { .. } => GuidanceResponse::Closed {
            provider_id: PROVIDER_ID.to_owned(),
        },
    }
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut provider = Provider::default();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                let response = GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "INPUT_FAILED".to_owned(),
                    message: error.to_string(),
                    retryable: false,
                };
                let _ = writeln!(stdout, "{}", encode(&response).unwrap());
                let _ = stdout.flush();
                break;
            }
        };
        let response = match bliss_playlist_guidance_spi::decode_request(&line) {
            Ok(request) => handle(&mut provider, request),
            Err(error) => GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "INVALID_REQUEST".to_owned(),
                message: error.to_string(),
                retryable: false,
            },
        };
        if writeln!(stdout, "{}", encode(&response).unwrap()).is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
        if matches!(response, GuidanceResponse::Closed { .. }) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bliss-guidance-playcounts-{}-{}.json",
            std::process::id(),
            PROVIDER_VERSION.replace('.', "-")
        ))
    }

    fn candidate(id: &str, path: &str) -> Candidate {
        Candidate {
            candidate_id: id.to_owned(),
            database_file: Some(path.to_owned()),
            title: None,
            artist: None,
            album: None,
            recording_mbid: None,
            artist_mbids: vec![],
        }
    }

    #[test]
    fn manifest_identifies_playcount_guidance_provider() {
        let manifest = Provider::manifest();
        assert_eq!(manifest.provider_id, "playcount-guidance");
        assert_eq!(manifest.protocol, PROTOCOL_NAME);
        assert_eq!(
            manifest.capabilities,
            vec![Capability::GlobalCandidateGuidance]
        );
    }

    #[test]
    fn prepare_and_score_maps_counts_to_percentile_guidance() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "schema_identity": "lms-play-counts-v1",
            "generated_at": 42,
            "database_cache_identity": "cache-1",
            "tracks": [
                {"database_file": "/music/quiet.mp3", "play_count": 0},
                {"database_file": "/music/favorite.mp3", "play_count": 10},
                {"database_file": "/music/unknown.mp3", "play_count": null}
            ]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let mut provider = Provider::default();
        provider
            .prepare(&serde_json::json!({"artifact_path": path}))
            .unwrap();
        let response = provider.score(
            "request-1",
            &[
                candidate("quiet", "/music/quiet.mp3"),
                candidate("favorite", "/music/favorite.mp3"),
                candidate("not-in-snapshot", "/music/missing.mp3"),
            ],
        );
        match response {
            GuidanceResponse::Scores { signals, .. } => {
                assert_eq!(signals.len(), 2);
                assert_eq!(signals[0].candidate_id, "quiet");
                assert_eq!(signals[1].candidate_id, "favorite");
                assert_eq!(signals[0].score, -1.0);
                assert_eq!(signals[1].score, 1.0);
            }
            other => panic!("expected scores response, got {other:?}"),
        }
        let _ = fs::remove_file(path);
    }
}
