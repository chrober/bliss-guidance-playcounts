// SPDX-License-Identifier: GPL-3.0-only

use bliss_playlist_guidance_spi::{
    encode, ArtifactDescriptor, Candidate, Capability, ChannelDescriptor, Diagnostics,
    GuidanceRequest, GuidanceResponse, GuidanceScope, GuidanceSignal, Manifest, ResourceAccess,
    ResourceDescriptor, PROTOCOL_NAME, SPI_VERSION,
};
use rusqlite::{params_from_iter, Connection, OpenFlags};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

const PROVIDER_ID: &str = "playcount-guidance";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROGRAM: &str = env!("CARGO_PKG_NAME");
const SQLITE_BATCH_LIMIT: usize = 900;

fn version_metadata_json() -> String {
    format!(
        "{{\"schema_version\":1,\"program\":\"{PROGRAM}\",\"version\":\"{PROVIDER_VERSION}\",\"provider_id\":\"{PROVIDER_ID}\",\"spi_version\":{SPI_VERSION}}}"
    )
}

fn usage() -> &'static str {
    "Usage:\n  bliss-guidance-playcounts version [--json]\n  bliss-guidance-playcounts"
}

#[derive(Debug, Deserialize)]
struct IdentityArtifact {
    schema_version: u8,
    schema_identity: String,
    candidates: Vec<CandidateIdentity>,
}

#[derive(Debug, Deserialize)]
struct CandidateIdentity {
    candidate_id: String,
    #[serde(default)]
    lms_urlmd5: Option<String>,
}

#[derive(Default)]
struct Provider {
    connection: Option<Connection>,
    distribution: BTreeMap<u64, u64>,
    eligible_count: u64,
    known_count: u64,
    zero_count: u64,
    cached_counts: HashMap<String, u64>,
    score_batches: u64,
    score_query_batches: u64,
    score_cache_hits: u64,
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
            channels: vec![ChannelDescriptor {
                channel: "playcount".to_owned(),
                scopes: vec![GuidanceScope::Global],
            }],
            required_context: vec!["candidate_identity".to_owned()],
            configuration_schema: Some(serde_json::json!({
                "type": "object",
                "additionalProperties": false
            })),
        }
    }

    fn prepare(
        &mut self,
        artifacts: &[ArtifactDescriptor],
        resources: &[ResourceDescriptor],
    ) -> Result<(Option<String>, Diagnostics), String> {
        self.reset();
        let artifact = required_artifact(artifacts, "eligible-candidate-identities-v1")?;
        let bytes = verified_artifact_bytes(artifact)?;
        let identities: IdentityArtifact = serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot decode candidate-identity artifact: {error}"))?;
        if identities.schema_version != 1
            || identities.schema_identity != "eligible-candidate-identities-v1"
        {
            return Err("unsupported candidate-identity artifact schema".to_owned());
        }
        if identities
            .candidates
            .iter()
            .any(|identity| identity.candidate_id.trim().is_empty())
        {
            return Err("candidate-identity artifact contains an empty candidate ID".to_owned());
        }

        let resource = required_read_only_resource(resources, "lms-persist-sqlite-v1")?;
        let connection = Connection::open_with_flags(
            &resource.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("cannot open Lyrion persist database read-only: {error}"))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| format!("cannot set SQLite busy timeout: {error}"))?;
        connection
            .execute_batch("PRAGMA query_only = ON; BEGIN;")
            .map_err(|error| format!("cannot begin read-only SQLite snapshot: {error}"))?;
        validate_tracks_persistent(&connection)?;

        let started = Instant::now();
        let urlmd5s: Vec<String> = identities
            .candidates
            .iter()
            .filter_map(|identity| identity.lms_urlmd5.clone())
            .filter(|urlmd5| !urlmd5.trim().is_empty())
            .collect();
        let lookup = query_counts(&connection, &urlmd5s)?;
        let mut distribution = BTreeMap::new();
        let mut known_count = 0_u64;
        let mut zero_count = 0_u64;
        for urlmd5 in &urlmd5s {
            let count = lookup.get(urlmd5).copied().unwrap_or(0);
            if lookup.contains_key(urlmd5) {
                known_count += 1;
            }
            if count == 0 {
                zero_count += 1;
            }
            *distribution.entry(count).or_insert(0) += 1;
        }
        for _ in urlmd5s.len()..identities.candidates.len() {
            zero_count += 1;
            *distribution.entry(0).or_insert(0) += 1;
        }

        let eligible_count = identities.candidates.len() as u64;
        let snapshot_id = format!(
            "sqlite:{}:{}",
            &artifact.sha256[..artifact.sha256.len().min(16)],
            eligible_count
        );
        self.connection = Some(connection);
        self.distribution = distribution;
        self.eligible_count = eligible_count;
        self.known_count = known_count;
        self.zero_count = zero_count;
        self.snapshot_id = Some(snapshot_id.clone());
        self.prepared = true;
        Ok((
            Some(snapshot_id),
            Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({
                    "eligible_candidates": eligible_count,
                    "known_counts": known_count,
                    "zero_counts": zero_count,
                    "distinct_play_counts": self.distribution.len(),
                    "prepare_query_batches": batch_count(urlmd5s.len()),
                    "elapsed_ms": started.elapsed().as_millis(),
                })),
            },
        ))
    }

    fn score(&mut self, request_id: &str, candidates: &[Candidate]) -> GuidanceResponse {
        if !self.prepared {
            return GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "NOT_PREPARED".to_owned(),
                message: "provider must receive prepare before score".to_owned(),
                retryable: false,
            };
        }
        let Some(connection) = self.connection.as_ref() else {
            return GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "SNAPSHOT_UNAVAILABLE".to_owned(),
                message: "provider has no active SQLite snapshot".to_owned(),
                retryable: true,
            };
        };
        let started = Instant::now();
        let urls: BTreeSet<String> = candidates
            .iter()
            .filter_map(|candidate| candidate.lms_urlmd5.clone())
            .filter(|urlmd5| !urlmd5.trim().is_empty())
            .collect();
        let uncached: Vec<String> = urls
            .iter()
            .filter(|urlmd5| !self.cached_counts.contains_key(*urlmd5))
            .cloned()
            .collect();
        let cache_hits = urls.len().saturating_sub(uncached.len()) as u64;
        let fetched = match query_counts(connection, &uncached) {
            Ok(fetched) => fetched,
            Err(message) => {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "SCORE_LOOKUP_FAILED".to_owned(),
                    message,
                    retryable: true,
                }
            }
        };
        for urlmd5 in &uncached {
            self.cached_counts
                .insert(urlmd5.clone(), fetched.get(urlmd5).copied().unwrap_or(0));
        }

        let signals: Vec<GuidanceSignal> = candidates
            .iter()
            .filter_map(|candidate| {
                let urlmd5 = candidate.lms_urlmd5.as_ref()?;
                let count = self.cached_counts.get(urlmd5).copied().unwrap_or(0);
                let percentile = self.percentile(count);
                Some(
                    GuidanceSignal {
                        candidate_id: candidate.candidate_id.clone(),
                        channel: "playcount".to_owned(),
                        scope: GuidanceScope::Global,
                        score: (2.0 * percentile - 1.0).clamp(-1.0, 1.0),
                        confidence: 1.0,
                        rationale: Some(format!("LMS play-count percentile {percentile:.3}")),
                        observed_at: None,
                    }
                    .bounded(),
                )
            })
            .collect();
        self.score_batches += 1;
        self.score_query_batches += batch_count(uncached.len());
        self.score_cache_hits += cache_hits;
        GuidanceResponse::Scores {
            provider_id: PROVIDER_ID.to_owned(),
            request_id: request_id.to_owned(),
            signals,
            diagnostics: Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({
                    "eligible_candidates": self.eligible_count,
                    "known_counts": self.known_count,
                    "zero_counts": self.zero_count,
                    "distribution_size": self.distribution.len(),
                    "query_batches": batch_count(uncached.len()),
                    "cache_hits": cache_hits,
                    "total_score_batches": self.score_batches,
                    "total_score_query_batches": self.score_query_batches,
                    "total_cache_hits": self.score_cache_hits,
                    "elapsed_ms": started.elapsed().as_millis(),
                })),
            },
        }
    }

    fn percentile(&self, count: u64) -> f64 {
        if self.eligible_count <= 1 {
            return 0.0;
        }
        let lower: u64 = self
            .distribution
            .range(..count)
            .map(|(_, frequency)| *frequency)
            .sum();
        let tied = self.distribution.get(&count).copied().unwrap_or(0);
        let average_rank = lower as f64 + (tied.saturating_sub(1) as f64 / 2.0);
        (average_rank / (self.eligible_count - 1) as f64).clamp(0.0, 1.0)
    }

    fn reset(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.execute_batch("ROLLBACK;");
        }
        self.distribution.clear();
        self.cached_counts.clear();
        self.eligible_count = 0;
        self.known_count = 0;
        self.zero_count = 0;
        self.score_batches = 0;
        self.score_query_batches = 0;
        self.score_cache_hits = 0;
        self.snapshot_id = None;
        self.prepared = false;
    }
}

fn required_artifact<'a>(
    artifacts: &'a [ArtifactDescriptor],
    kind: &str,
) -> Result<&'a ArtifactDescriptor, String> {
    let matching: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.kind == kind)
        .collect();
    match matching.as_slice() {
        [artifact] => Ok(*artifact),
        [] => Err(format!("missing required {kind} artifact")),
        _ => Err(format!("multiple {kind} artifacts are not allowed")),
    }
}

fn required_read_only_resource<'a>(
    resources: &'a [ResourceDescriptor],
    kind: &str,
) -> Result<&'a ResourceDescriptor, String> {
    let matching: Vec<_> = resources
        .iter()
        .filter(|resource| resource.kind == kind)
        .collect();
    match matching.as_slice() {
        [resource] if resource.access == ResourceAccess::ReadOnly => Ok(*resource),
        [resource] => Err(format!("{} resource must be read_only", resource.kind)),
        [] => Err(format!("missing required {kind} resource")),
        _ => Err(format!("multiple {kind} resources are not allowed")),
    }
}

fn verified_artifact_bytes(artifact: &ArtifactDescriptor) -> Result<Vec<u8>, String> {
    let bytes = fs::read(&artifact.path)
        .map_err(|error| format!("cannot read {} artifact: {error}", artifact.kind))?;
    if format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
        return Err(format!("{} artifact SHA-256 mismatch", artifact.kind));
    }
    Ok(bytes)
}

fn validate_tracks_persistent(connection: &Connection) -> Result<(), String> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tracks_persistent')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("cannot inspect tracks_persistent schema: {error}"))?;
    if !exists {
        return Err("tracks_persistent table is unavailable".to_owned());
    }
    let mut statement = connection
        .prepare("PRAGMA table_info(tracks_persistent)")
        .map_err(|error| format!("cannot inspect tracks_persistent columns: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("cannot read tracks_persistent columns: {error}"))?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|error| format!("cannot decode tracks_persistent columns: {error}"))?;
    for column in ["urlmd5", "playcount"] {
        if !columns.contains(column) {
            return Err(format!("tracks_persistent.{column} column is unavailable"));
        }
    }
    Ok(())
}

fn query_counts(
    connection: &Connection,
    urlmd5s: &[String],
) -> Result<HashMap<String, u64>, String> {
    let mut result = HashMap::new();
    let unique: Vec<String> = urlmd5s
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for batch in unique.chunks(SQLITE_BATCH_LIMIT) {
        let placeholders = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT urlmd5, playcount FROM tracks_persistent WHERE urlmd5 IN ({placeholders})"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| format!("cannot prepare play-count query: {error}"))?;
        let rows = statement
            .query_map(params_from_iter(batch.iter()), |row| {
                let count = row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64;
                Ok((row.get::<_, String>(0)?, count))
            })
            .map_err(|error| format!("cannot query play counts: {error}"))?;
        for row in rows {
            let (urlmd5, count) =
                row.map_err(|error| format!("cannot decode play count: {error}"))?;
            result.insert(urlmd5, count);
        }
    }
    Ok(result)
}

fn batch_count(item_count: usize) -> u64 {
    item_count.div_ceil(SQLITE_BATCH_LIMIT) as u64
}

fn handle(provider: &mut Provider, request: GuidanceRequest) -> GuidanceResponse {
    match request {
        GuidanceRequest::Describe { spi_version } => {
            if spi_version == SPI_VERSION {
                GuidanceResponse::Manifest(Provider::manifest())
            } else {
                unsupported_version()
            }
        }
        GuidanceRequest::Prepare {
            spi_version,
            artifacts,
            resources,
            ..
        } => {
            if spi_version != SPI_VERSION {
                return unsupported_version();
            }
            match provider.prepare(&artifacts, &resources) {
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
            if spi_version == SPI_VERSION {
                provider.score(&request_id, &candidates)
            } else {
                unsupported_version()
            }
        }
        GuidanceRequest::Close { .. } => {
            provider.reset();
            GuidanceResponse::Closed {
                provider_id: PROVIDER_ID.to_owned(),
            }
        }
    }
}

fn unsupported_version() -> GuidanceResponse {
    GuidanceResponse::Error {
        provider_id: Some(PROVIDER_ID.to_owned()),
        code: "UNSUPPORTED_SPI_VERSION".to_owned(),
        message: format!("provider supports SPI version {SPI_VERSION}"),
        retryable: false,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [command] if command == "version" => {
            println!("{PROGRAM} {PROVIDER_VERSION}");
            return;
        }
        [command, format] if command == "version" && format == "--json" => {
            println!("{}", version_metadata_json());
            return;
        }
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut provider = Provider::default();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    encode(&GuidanceResponse::Error {
                        provider_id: Some(PROVIDER_ID.to_owned()),
                        code: "INPUT_FAILED".to_owned(),
                        message: error.to_string(),
                        retryable: false
                    })
                    .unwrap()
                );
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
        if writeln!(stdout, "{}", encode(&response).unwrap()).is_err() || stdout.flush().is_err() {
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
    use rusqlite::{params, Connection};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn version_metadata_identifies_playcount_provider_and_spi() {
        let metadata = version_metadata_json();
        assert!(metadata.contains("\"program\":\"bliss-guidance-playcounts\""));
        assert!(metadata.contains("\"provider_id\":\"playcount-guidance\""));
        assert!(metadata.contains("\"spi_version\":"));
    }
    fn fixture_path(extension: &str) -> PathBuf {
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bliss-guidance-playcounts-{}-{sequence}.{extension}",
            std::process::id()
        ))
    }
    fn sha256(path: &Path) -> String {
        format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
    }
    fn candidate(id: &str, urlmd5: &str) -> Candidate {
        Candidate {
            candidate_id: id.to_owned(),
            lms_urlmd5: Some(urlmd5.to_owned()),
            database_file: None,
            title: None,
            artist: None,
            album: None,
            recording_mbid: None,
            artist_mbids: vec![],
        }
    }
    fn fixture_database(rows: &[(&str, u64)]) -> PathBuf {
        let path = fixture_path("sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE tracks_persistent (urlmd5 TEXT PRIMARY KEY, playcount INTEGER);").unwrap();
        for (urlmd5, playcount) in rows {
            connection
                .execute(
                    "INSERT INTO tracks_persistent(urlmd5, playcount) VALUES (?1, ?2)",
                    params![urlmd5, playcount],
                )
                .unwrap();
        }
        path
    }
    fn identity_artifact(identities: &[(&str, &str)]) -> (PathBuf, ArtifactDescriptor) {
        let path = fixture_path("json");
        let payload = serde_json::json!({"schema_version":1,"schema_identity":"eligible-candidate-identities-v1","candidates": identities.iter().map(|(candidate_id,lms_urlmd5)| serde_json::json!({"candidate_id":candidate_id,"lms_urlmd5":lms_urlmd5})).collect::<Vec<_>>()});
        fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        let descriptor = ArtifactDescriptor {
            kind: "eligible-candidate-identities-v1".to_owned(),
            path: path.display().to_string(),
            sha256: sha256(&path),
        };
        (path, descriptor)
    }
    fn persist_resource(path: &Path) -> ResourceDescriptor {
        ResourceDescriptor {
            kind: "lms-persist-sqlite-v1".to_owned(),
            path: path.display().to_string(),
            access: ResourceAccess::ReadOnly,
        }
    }
    fn score(provider: &mut Provider, candidate_id: &str, urlmd5: &str) -> f64 {
        match provider.score("score", &[candidate(candidate_id, urlmd5)]) {
            GuidanceResponse::Scores { signals, .. } => {
                signals
                    .into_iter()
                    .find(|signal| signal.candidate_id == candidate_id)
                    .expect("candidate receives a signal")
                    .score
            }
            other => panic!("expected score response, got {other:?}"),
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
        assert_eq!(manifest.channels[0].channel, "playcount");
        assert_eq!(manifest.channels[0].scopes, vec![GuidanceScope::Global]);
    }
    #[test]
    fn sqlite_snapshot_scores_zero_and_absent_counts_equally() {
        let database = fixture_database(&[("favorite", 10)]);
        let (artifact, descriptor) = identity_artifact(&[
            ("zero", "zero"),
            ("missing", "missing"),
            ("favorite", "favorite"),
        ]);
        let mut provider = Provider::default();
        provider
            .prepare(&[descriptor], &[persist_resource(&database)])
            .unwrap();
        assert_eq!(
            score(&mut provider, "zero", "zero"),
            score(&mut provider, "missing", "missing")
        );
        assert!(
            score(&mut provider, "zero", "zero") < score(&mut provider, "favorite", "favorite")
        );
        let _ = fs::remove_file(artifact);
        let _ = fs::remove_file(database);
    }
    #[test]
    fn equal_counts_use_the_same_average_rank_across_score_batches() {
        let database = fixture_database(&[("same-a", 5), ("same-b", 5), ("high", 20)]);
        let (artifact, descriptor) =
            identity_artifact(&[("same-a", "same-a"), ("same-b", "same-b"), ("high", "high")]);
        let mut provider = Provider::default();
        provider
            .prepare(&[descriptor], &[persist_resource(&database)])
            .unwrap();
        assert_eq!(
            score(&mut provider, "same-a", "same-a"),
            score(&mut provider, "same-b", "same-b")
        );
        assert!(score(&mut provider, "same-a", "same-a") < score(&mut provider, "high", "high"));
        let _ = fs::remove_file(artifact);
        let _ = fs::remove_file(database);
    }
    #[test]
    fn prepared_snapshot_ignores_a_later_external_database_update() {
        let database = fixture_database(&[("song", 1), ("other", 10)]);
        let (artifact, descriptor) = identity_artifact(&[("song", "song"), ("other", "other")]);
        let mut provider = Provider::default();
        provider
            .prepare(&[descriptor], &[persist_resource(&database)])
            .unwrap();
        // Establish the snapshot through a different candidate, leaving
        // `song` uncached when the concurrent write happens.
        assert_eq!(score(&mut provider, "other", "other"), 1.0);
        Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE tracks_persistent SET playcount = 999 WHERE urlmd5 = 'song'",
                [],
            )
            .unwrap();
        assert_eq!(score(&mut provider, "song", "song"), -1.0);
        let _ = fs::remove_file(artifact);
        let _ = fs::remove_file(database);
    }
    #[test]
    fn missing_persistent_schema_is_rejected_during_prepare() {
        let database = fixture_path("sqlite");
        Connection::open(&database).unwrap();
        let (artifact, descriptor) = identity_artifact(&[("song", "song")]);
        let error = Provider::default()
            .prepare(&[descriptor], &[persist_resource(&database)])
            .unwrap_err();
        assert!(error.contains("tracks_persistent"));
        let _ = fs::remove_file(artifact);
        let _ = fs::remove_file(database);
    }
}
