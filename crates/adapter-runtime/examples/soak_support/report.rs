use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::platform::ResourceSample;
use crate::{Check, REQUIRED_CASES};

const VERSION: u32 = 1;
const MAX_RUNS: usize = 32;
const MAX_REPORT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SAMPLES: usize = 256;
const MAX_ERRORS: usize = 32;
pub const MAX_OBSERVATION_GAP: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Config {
    pub duration_secs: u64,
    pub concurrency: usize,
    pub sample_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration_secs: 60,
            concurrency: 4,
            sample_secs: 5,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Check<()> {
        if !(1..=259_200).contains(&self.duration_secs)
            || !(1..=16).contains(&self.concurrency)
            || !(1..=60).contains(&self.sample_secs)
        {
            return Err(
                "Use duration 1..259200 s, concurrency 1..16, sample interval 1..60 s.".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CaseCounts {
    pub started: u64,
    pub passed: u64,
    pub failed: u64,
    pub max_latency_ms: u64,
}

#[derive(Default)]
pub struct Evidence {
    pub upstream_posts: u64,
    pub cancellations: u64,
    pub rejections: u64,
    pub frames: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Statistics {
    pub cases: BTreeMap<String, CaseCounts>,
    pub http_requests_started: u64,
    pub direct_backend_requests_started: u64,
    pub expected_upstream_posts: u64,
    pub verified_cancellations: u64,
    pub verified_rejections: u64,
    pub verified_sse_frames: u64,
    pub active_cases: u64,
    pub peak_active_cases: u64,
    pub unexpected_errors: u64,
    pub errors: VecDeque<String>,
    pub errors_omitted: u64,
}

impl Statistics {
    pub fn start(&mut self, name: &str) {
        self.cases.entry(name.into()).or_default().started += 1;
        self.active_cases += 1;
        self.peak_active_cases = self.peak_active_cases.max(self.active_cases);
    }

    pub fn finish(&mut self, name: &str, elapsed: Duration, result: Check<Evidence>) -> bool {
        self.active_cases -= 1;
        let counts = self.cases.get_mut(name).expect("started case");
        counts.max_latency_ms = counts.max_latency_ms.max(millis(elapsed));
        match result {
            Ok(evidence) => {
                counts.passed += 1;
                self.expected_upstream_posts += evidence.upstream_posts;
                self.verified_cancellations += evidence.cancellations;
                self.verified_rejections += evidence.rejections;
                self.verified_sse_frames += evidence.frames;
                true
            }
            Err(error) => {
                counts.failed += 1;
                self.error(format!("{name}: {error}"));
                false
            }
        }
    }

    pub fn error(&mut self, error: impl AsRef<str>) {
        self.unexpected_errors += 1;
        if self.errors.len() == MAX_ERRORS {
            self.errors_omitted += 1;
        } else {
            self.errors
                .push_back(error.as_ref().chars().take(512).collect());
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Resources {
    pub sample_count: u64,
    pub samples_omitted: u64,
    pub samples: VecDeque<ResourceSample>,
    pub peak_private_bytes: u64,
    pub peak_working_set_bytes: u64,
    pub peak_handles: u64,
    pub cpu_ms: u64,
}

impl Resources {
    pub fn push(&mut self, sample: ResourceSample) {
        self.sample_count += 1;
        self.peak_private_bytes = self.peak_private_bytes.max(sample.private_bytes);
        self.peak_working_set_bytes = self.peak_working_set_bytes.max(sample.working_set_bytes);
        self.peak_handles = self.peak_handles.max(sample.handles);
        self.cpu_ms = sample.cpu_ms;
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
            self.samples_omitted += 1;
        }
        self.samples.push_back(sample);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Drain {
    pub workers_joined: bool,
    pub active_request_cancelled: bool,
    pub runtime_joined: bool,
    pub upstream_joined: bool,
    pub listeners_released: bool,
    pub synthetic_data_removed: bool,
    pub remaining_upstream_bodies: u64,
    pub remaining_tickets: usize,
    pub elapsed_ms: u64,
}

impl Drain {
    fn clean(&self) -> bool {
        self.workers_joined
            && self.active_request_cancelled
            && self.runtime_joined
            && self.upstream_joined
            && self.listeners_released
            && self.synthetic_data_removed
            && self.remaining_upstream_bodies == 0
            && self.remaining_tickets == 0
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Checks {
    pub duration_met: bool,
    pub required_cases_passed: bool,
    pub exact_upstream_count: bool,
    pub clean_drain: bool,
    pub uninterrupted: bool,
    pub no_unexpected_errors: bool,
    pub native_windows_architecture: bool,
    pub build_attributed: bool,
    pub resources_recorded: bool,
    pub workload_passed: bool,
    pub qualification_passed: bool,
    pub qualified_24h: bool,
    pub qualified_72h: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub state: String,
    pub reason: Option<String>,
    pub pid: u32,
    pub started_unix_ms: u64,
    pub checkpoint_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    pub active_elapsed_ms: u64,
    pub observation_elapsed_ms: u64,
    pub excluded_gap_ms: u64,
    pub largest_observation_gap_ms: u64,
    pub largest_wall_clock_gap_ms: u64,
    pub wall_clock_discontinuities: u64,
    pub identity: Value,
    pub statistics: Statistics,
    pub upstream: Value,
    pub resources: Resources,
    pub drain: Drain,
    pub checks: Checks,
}

impl Run {
    pub fn new(identity: Value) -> Self {
        let now = unix_ms();
        Self {
            id: unique_id(),
            state: "starting".into(),
            reason: None,
            pid: std::process::id(),
            started_unix_ms: now,
            checkpoint_unix_ms: now,
            finished_unix_ms: None,
            active_elapsed_ms: 0,
            observation_elapsed_ms: 0,
            excluded_gap_ms: 0,
            largest_observation_gap_ms: 0,
            largest_wall_clock_gap_ms: 0,
            wall_clock_discontinuities: 0,
            identity,
            statistics: Statistics::default(),
            upstream: json!({}),
            resources: Resources::default(),
            drain: Drain::default(),
            checks: Checks::default(),
        }
    }

    pub fn finish(&mut self, config: &Config, reason: &str) {
        self.reason = Some(reason.into());
        self.finished_unix_ms = Some(unix_ms());
        let build = &self.identity["build"];
        self.checks = Checks {
            duration_met: self.active_elapsed_ms >= config.duration_secs * 1000,
            required_cases_passed: REQUIRED_CASES.iter().all(|name| {
                self.statistics
                    .cases
                    .get(*name)
                    .is_some_and(|case| case.passed > 0 && case.failed == 0)
            }),
            exact_upstream_count: self.upstream["posts"].as_u64()
                == Some(self.statistics.expected_upstream_posts)
                && self.upstream["unexpected_requests"] == 0
                && self.upstream["duplicate_requests"] == 0,
            clean_drain: self.drain.clean(),
            uninterrupted: reason == "duration_elapsed"
                && self.excluded_gap_ms == 0
                && self.wall_clock_discontinuities == 0,
            no_unexpected_errors: self.statistics.unexpected_errors == 0
                && self.statistics.active_cases == 0
                && self
                    .statistics
                    .cases
                    .values()
                    .all(|case| case.started == case.passed + case.failed && case.failed == 0),
            native_windows_architecture: self.identity["platform"]["os"] == "windows"
                && self.identity["platform"]["emulated"] == false
                && self.identity["platform"]["native_architecture"]
                    == self.identity["process_architecture"],
            build_attributed: build["schema_version"] == 1
                && build["profile"] == "release"
                && self.identity["debug_assertions"] == false
                && build["source_sha256"]
                    .as_str()
                    .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                && build["source_head"]
                    .as_str()
                    .is_some_and(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                && ["msvc_toolset", "windows_sdk", "visual_studio"]
                    .iter()
                    .all(|name| build[*name].as_str().is_some_and(|v| !v.is_empty()))
                && build["rustc_verbose"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
                && build["target"].as_str().is_some_and(|target| {
                    target.starts_with(std::env::consts::ARCH)
                        && target.ends_with("-pc-windows-msvc")
                }),
            resources_recorded: self.resources.sample_count >= 2,
            ..Checks::default()
        };
        let checks = &mut self.checks;
        checks.workload_passed = checks.duration_met
            && checks.required_cases_passed
            && checks.exact_upstream_count
            && checks.clean_drain
            && checks.uninterrupted
            && checks.no_unexpected_errors;
        checks.qualification_passed = checks.workload_passed
            && checks.native_windows_architecture
            && checks.build_attributed
            && checks.resources_recorded;
        checks.qualified_24h = checks.qualification_passed && self.active_elapsed_ms >= 86_400_000;
        checks.qualified_72h = checks.qualification_passed && self.active_elapsed_ms >= 259_200_000;
        self.state = if self.statistics.unexpected_errors > 0 || !checks.clean_drain {
            "failed"
        } else if reason != "duration_elapsed" {
            "incomplete"
        } else if checks.workload_passed {
            "completed"
        } else {
            "failed"
        }
        .into();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Document {
    pub schema_version: u32,
    pub kind: String,
    pub config: Config,
    pub runs: Vec<Run>,
}

pub struct Store {
    pub path: PathBuf,
    pub directory: PathBuf,
    pub stop_path: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(path: &Path, config: &Config, resume: bool) -> Check<(Self, Document)> {
        config.validate()?;
        let path = owned_relative_path(path)?;
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("Report must be inside a new dedicated directory below the working directory.")?
            .to_path_buf();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid report filename.")?;
        if !name.ends_with(".json") || name.to_ascii_lowercase().starts_with("native-soak") {
            return Err("Report filename must end in .json and must not use the reserved native-soak prefix.".into());
        }
        let marker = directory.join("native-soak.owner.json");
        if !resume {
            if let Some(parent) = directory.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Create report parent: {e}"))?;
            }
            std::fs::create_dir(&directory)
                .map_err(|e| format!("New runs require a new dedicated report directory: {e}"))?;
            atomic_json(&marker, &json!({"schema_version": VERSION, "report": name}))?;
        }
        let owner = read_json::<Value>(&marker)?;
        if owner != json!({"schema_version": VERSION, "report": name}) {
            return Err("Report directory ownership/version does not match.".into());
        }
        let lock_path = directory.join("native-soak.lock");
        reject_link(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| format!("Open report lock: {e}"))?;
        lock.try_lock()
            .map_err(|e| format!("Report already has a writer (or locking failed): {e}"))?;
        let stop_path = directory.join("native-soak.stop");
        if stop_path.try_exists().map_err(|e| e.to_string())? {
            return Err(
                "The owned native-soak.stop file exists; remove it explicitly before resuming."
                    .into(),
            );
        }
        let mut document = if resume {
            let document: Document = read_json(&path)?;
            if document.schema_version != VERSION
                || document.kind != "native-runtime-soak"
                || document.config != *config
                || document.runs.is_empty()
                || document.runs.len() >= MAX_RUNS
            {
                return Err(
                    "Resume requires the same schema/config and fewer than 32 retained attempts."
                        .into(),
                );
            }
            document
        } else {
            Document {
                schema_version: VERSION,
                kind: "native-runtime-soak".into(),
                config: config.clone(),
                runs: Vec::new(),
            }
        };
        for run in &mut document.runs {
            if run.id.is_empty()
                || run.id.len() > 64
                || !run
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'-')
            {
                return Err("Invalid retained run ID.".into());
            }
            if matches!(run.state.as_str(), "starting" | "running" | "draining") {
                // Preserve the last observed active time, not the time since that checkpoint.
                run.state = "interrupted".into();
                run.reason = Some("unfinished_checkpoint_recovered_after_exclusive_lock".into());
                run.checks = Checks::default();
                let abandoned_cache = directory.join(format!("native-soak-models-{}.json", run.id));
                reject_link(&abandoned_cache)?;
                match std::fs::remove_file(abandoned_cache) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("Remove abandoned synthetic cache: {error}")),
                }
            }
        }
        let store = Self {
            path,
            directory,
            stop_path,
            _lock: lock,
        };
        Ok((store, document))
    }

    pub fn save(&self, document: &mut Document) -> Check<()> {
        if let Some(run) = document.runs.last_mut() {
            run.checkpoint_unix_ms = unix_ms();
        }
        atomic_json(&self.path, document)
    }
}

fn owned_relative_path(path: &Path) -> Check<PathBuf> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let relative = if path.is_absolute() {
        path.strip_prefix(&cwd)
            .map_err(|_| "Report must stay below the working directory.")?
    } else {
        path
    };
    let mut result = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let text = name.to_str().ok_or("Use a UTF-8 report path.")?;
                if text.contains(':') || text.ends_with(['.', ' ']) {
                    return Err("Ambiguous report path component.".into());
                }
                result.push(name);
                reject_link(&result)?;
            }
            _ => {
                return Err("Report path may not escape through '..', a root, or a prefix.".into());
            }
        }
    }
    Ok(result)
}

fn reject_link(path: &Path) -> Check<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let linked = metadata.file_type().is_symlink();
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                linked || metadata.file_attributes() & 0x400 != 0
            };
            if linked {
                return Err(
                    "Owned report paths may not contain symlinks or reparse points.".into(),
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Inspect owned path: {error}")),
    }
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Check<()> {
    let pending = path.with_extension("json.pending");
    reject_link(path)?;
    reject_link(&pending)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err("Bounded report exceeded its 8 MiB safety limit.".into());
    }
    let mut file = File::create(&pending).map_err(|e| format!("Create report checkpoint: {e}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("Flush report checkpoint: {e}"))?;
    drop(file);
    std::fs::rename(pending, path).map_err(|e| format!("Atomically replace report: {e}"))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Check<T> {
    reject_link(path)?;
    let file = File::open(path).map_err(|e| format!("Read owned report: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err("Existing report exceeds the read limit.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid owned report: {e}"))
}

#[derive(Default)]
pub struct ActiveClock {
    pub active: Duration,
    pub observed: Duration,
    pub excluded: Duration,
    pub largest_gap: Duration,
    pub largest_wall_gap: Duration,
    pub wall_discontinuities: u64,
    last_wall_ms: Option<u64>,
}

impl ActiveClock {
    #[cfg(test)]
    pub fn observe(&mut self, elapsed: Duration) -> bool {
        self.observe_interval(elapsed, true)
    }

    pub fn observe_with_wall(&mut self, elapsed: Duration, wall_ms: u64) -> bool {
        let mut continuous = true;
        if let Some(previous) = self.last_wall_ms {
            let gap = Duration::from_millis(wall_ms.saturating_sub(previous));
            self.largest_wall_gap = self.largest_wall_gap.max(gap);
            if wall_ms < previous || gap > MAX_OBSERVATION_GAP {
                self.wall_discontinuities += 1;
                continuous = false;
            }
        }
        self.last_wall_ms = Some(wall_ms);
        self.observe_interval(elapsed, continuous)
    }

    fn observe_interval(&mut self, elapsed: Duration, wall_continuous: bool) -> bool {
        let interval = elapsed.saturating_sub(self.observed);
        self.observed = elapsed;
        self.largest_gap = self.largest_gap.max(interval);
        if interval > MAX_OBSERVATION_GAP || !wall_continuous {
            self.excluded += interval;
            false
        } else {
            self.active += interval;
            true
        }
    }

    pub fn update(&self, run: &mut Run) {
        run.active_elapsed_ms = millis(self.active);
        run.observation_elapsed_ms = millis(self.observed);
        run.excluded_gap_ms = millis(self.excluded);
        run.largest_observation_gap_ms = millis(self.largest_gap);
        run.largest_wall_clock_gap_ms = millis(self.largest_wall_gap);
        run.wall_clock_discontinuities = self.wall_discontinuities;
    }
}

pub fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn unix_ms() -> u64 {
    millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

pub fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
