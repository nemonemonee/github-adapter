//! Bounded, offline release evidence, not a replacement runtime or a throughput benchmark.

#[path = "soak_support/fixture.rs"]
mod fixture;
#[path = "soak_support/platform.rs"]
mod platform;
#[path = "soak_support/report.rs"]
mod report;
#[allow(dead_code)]
#[path = "../src/test_support.rs"]
mod support;

use fixture::Fixture;
use report::{ActiveClock, Config, Document, Evidence, Run, Statistics, Store};
use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

type Check<T> = std::result::Result<T, String>;
type SharedStatistics = Arc<Mutex<Statistics>>;

const WORKLOAD: &[&str] = &[
    "json",
    "sse",
    "slow_sse",
    "disconnect",
    "cancel",
    "malformed_json",
    "malformed_shape",
    "oversized_body",
    "upstream_error",
    "malformed_sse",
];
const REQUIRED_CASES: &[&str] = &[
    "admission",
    "json",
    "sse",
    "slow_sse",
    "disconnect",
    "cancel",
    "malformed_json",
    "malformed_shape",
    "oversized_body",
    "upstream_error",
    "malformed_sse",
    "health",
    "drain",
];
const CASE_TIMEOUT: Duration = Duration::from_secs(10);

struct Arguments {
    config: Config,
    report: PathBuf,
    resume: bool,
    stdin_control: bool,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Check<Self> {
        let mut config = Config::default();
        let mut path = None;
        let mut resume = false;
        let mut stdin_control = false;
        let mut seen = std::collections::HashSet::new();
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            require(seen.insert(flag.clone()), "Duplicate CLI option")?;
            match flag.as_str() {
                "--resume" => resume = true,
                "--stdin-control" => stdin_control = true,
                "--report" => {
                    path = Some(PathBuf::from(
                        arguments.next().ok_or("Missing report path")?,
                    ))
                }
                "--duration-secs" | "--concurrency" | "--sample-secs" => {
                    let value = arguments.next().ok_or("Missing numeric option")?;
                    require(
                        value.bytes().all(|b| b.is_ascii_digit()),
                        "CLI values must be unsigned decimal integers",
                    )?;
                    let value = value.parse::<u64>().map_err(|e| e.to_string())?;
                    match flag.as_str() {
                        "--duration-secs" => config.duration_secs = value,
                        "--concurrency" => {
                            config.concurrency =
                                value.try_into().map_err(|_| "Concurrency overflow")?
                        }
                        _ => config.sample_secs = value,
                    }
                }
                _ => return Err(format!("Unknown option: {flag}")),
            }
        }
        config.validate()?;
        Ok(Self {
            config,
            report: path
                .ok_or("--report is required (inside a new dedicated project directory)")?,
            resume,
            stdin_control,
        })
    }
}

#[derive(Clone, Default)]
struct Stop {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl Stop {
    fn request(&self, reason: &str) {
        let mut saved = self.reason.lock().unwrap();
        if saved.is_none() {
            *saved = Some(reason.into());
        }
        self.token.cancel();
    }

    fn reason(&self) -> String {
        self.reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "stop_requested".into())
    }
}

#[tokio::main(worker_threads = 4)]
async fn main() -> std::process::ExitCode {
    let raw = std::env::args().skip(1).collect::<Vec<_>>();
    if raw == ["--help"] {
        println!(
            "soak_fixture --report target\\owned-soak\\report.json [--duration-secs 60] \
            [--concurrency 4] [--sample-secs 5] [--resume] [--stdin-control]\n\
            Windows native resource evidence; literal loopback only. No remote/client configuration options.\n\
            Resume starts a NEW full-duration attempt; it never adds earlier active time.\n\
            Stop with Ctrl+C, owned native-soak.stop, or one stdin byte/EOF with --stdin-control."
        );
        return std::process::ExitCode::SUCCESS;
    }
    let arguments = match Arguments::parse(raw) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("Native soak arguments: {error}");
            return std::process::ExitCode::from(1);
        }
    };
    let stop = Stop::default();
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => signal_stop.request("ctrl_c"),
            Err(_) => signal_stop.request("signal_handler_failed"),
        }
    });
    if arguments.stdin_control {
        let input_stop = stop.clone();
        std::thread::spawn(move || {
            let mut byte = [0; 1];
            let reason = match std::io::stdin().read(&mut byte) {
                Ok(0) => "control_eof",
                Ok(_) => "control_input",
                Err(_) => "control_input_failed",
            };
            input_stop.request(reason);
        });
    }
    let result = execute(arguments, stop).await;
    signal.abort();
    match result {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("Native soak failed; no completion is claimed: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn execute(arguments: Arguments, stop: Stop) -> Check<u8> {
    let (store, mut document) =
        Store::open(&arguments.report, &arguments.config, arguments.resume)?;
    let statistics = Arc::new(Mutex::new(Statistics::default()));
    let identity = platform::identity();
    let mut run = Run::new(
        identity
            .clone()
            .unwrap_or_else(|error| json!({"identity_error": error})),
    );
    if let Err(error) = identity {
        statistics.lock().unwrap().error(error);
    }
    sample(&mut run, &statistics);
    document.runs.push(run);
    refresh(&store, &mut document, &statistics, None, None, false)?;
    let run_id = document.runs.last().unwrap().id.clone();
    let fixture = match Fixture::start(
        &arguments.config,
        &store.directory,
        &run_id,
        statistics.clone(),
    )
    .await
    {
        Ok(fixture) => fixture,
        Err(error) => {
            statistics.lock().unwrap().error(error);
            let run = document.runs.last_mut().unwrap();
            run.statistics = statistics.lock().unwrap().clone();
            run.finish(&arguments.config, "startup_failed");
            store.save(&mut document)?;
            return Ok(1);
        }
    };
    let failure = CancellationToken::new();
    let workers_stop = CancellationToken::new();
    let mut workers = JoinSet::new();
    if !stop.token.is_cancelled() && !measure(&statistics, "admission", fixture.admission()).await {
        failure.cancel();
    }
    let mut reason = "duration_elapsed".to_string();
    let mut clock = ActiveClock::default();
    let started = Instant::now();
    clock.observe_with_wall(Duration::ZERO, report::unix_ms());
    let mut next_sample = Duration::from_secs(arguments.config.sample_secs);
    document.runs.last_mut().unwrap().state = "running".into();
    if let Err(error) = refresh(
        &store,
        &mut document,
        &statistics,
        Some(&fixture),
        Some(&clock),
        false,
    ) {
        statistics.lock().unwrap().error(error);
        failure.cancel();
    }
    if let Err(error) = announce(
        json!({"event": "ready", "pid": std::process::id(), "run_id": run_id,
        "report": store.path, "stop_file": store.stop_path, "upstream": fixture.snapshot()}),
    ) {
        statistics.lock().unwrap().error(error);
        failure.cancel();
    }
    if statistics.lock().unwrap().unexpected_errors > 0 {
        failure.cancel();
    }
    if !failure.is_cancelled() && !stop.token.is_cancelled() {
        for index in 0..arguments.config.concurrency {
            let fixture = fixture.clone();
            let statistics = statistics.clone();
            let stop = workers_stop.clone();
            let failure = failure.clone();
            workers.spawn(async move {
                let mut sequence = 0_u64;
                while !stop.is_cancelled() && !failure.is_cancelled() {
                    let case = WORKLOAD[(sequence as usize + index) % WORKLOAD.len()];
                    let id = format!("w{index}-n{sequence}");
                    if !measure(&statistics, case, fixture.case(case, &id)).await {
                        failure.cancel();
                        break;
                    }
                    sequence += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
        }
        let fixture = fixture.clone();
        let statistics = statistics.clone();
        let stop = workers_stop.clone();
        let failure = failure.clone();
        workers.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = interval.tick() => {
                        if !measure(&statistics, "health", fixture.health()).await {
                            failure.cancel();
                            break;
                        }
                    }
                }
            }
        });
    }
    let mut heartbeat = tokio::time::interval(Duration::from_millis(200));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        heartbeat.tick().await;
        if !clock.observe_with_wall(started.elapsed(), report::unix_ms()) {
            reason = "observation_gap_exceeded_5_seconds".into();
            break;
        }
        match store.stop_path.try_exists() {
            Ok(true) => stop.request("stop_file"),
            Ok(false) => {}
            Err(error) => {
                statistics
                    .lock()
                    .unwrap()
                    .error(format!("Stop-file check: {error}"));
                failure.cancel();
            }
        }
        if failure.is_cancelled() {
            reason = "unexpected_error".into();
            break;
        }
        if stop.token.is_cancelled() {
            reason = stop.reason();
            break;
        }
        if clock.active >= Duration::from_secs(arguments.config.duration_secs) {
            break;
        }
        if clock.active >= next_sample {
            if let Err(error) = refresh(
                &store,
                &mut document,
                &statistics,
                Some(&fixture),
                Some(&clock),
                true,
            ) {
                statistics.lock().unwrap().error(error);
                failure.cancel();
            }
            if statistics.lock().unwrap().unexpected_errors > 0 {
                failure.cancel();
            }
            next_sample = clock.active + Duration::from_secs(arguments.config.sample_secs);
        }
        if let Some(result) = workers.try_join_next() {
            statistics.lock().unwrap().error(match result {
                Ok(()) => "Worker stopped before the bounded workload ended".into(),
                Err(error) => format!("Worker task failed: {error}"),
            });
            failure.cancel();
        }
    }
    workers_stop.cancel();
    document.runs.last_mut().unwrap().state = "draining".into();
    if let Err(error) = refresh(
        &store,
        &mut document,
        &statistics,
        Some(&fixture),
        Some(&clock),
        false,
    ) {
        statistics.lock().unwrap().error(error);
    }
    let mut workers_joined = true;
    if tokio::time::timeout(Duration::from_secs(12), async {
        while let Some(result) = workers.join_next().await {
            if let Err(error) = result {
                workers_joined = false;
                statistics
                    .lock()
                    .unwrap()
                    .error(format!("Worker join failed: {error}"));
            }
        }
    })
    .await
    .is_err()
    {
        workers_joined = false;
        statistics
            .lock()
            .unwrap()
            .error("Worker drain timed out; aborted owned tasks, not clean.");
        workers.abort_all();
        while workers.join_next().await.is_some() {}
    }
    statistics.lock().unwrap().start("drain");
    let drain_started = Instant::now();
    let (drain, drain_result) = fixture.stop(workers_joined).await;
    statistics
        .lock()
        .unwrap()
        .finish("drain", drain_started.elapsed(), drain_result);
    let run = document.runs.last_mut().unwrap();
    run.drain = drain;
    clock.update(run);
    sample(run, &statistics);
    run.statistics = statistics.lock().unwrap().clone();
    run.upstream = fixture.snapshot();
    run.finish(&arguments.config, &reason);
    store.save(&mut document)?;
    let run = document.runs.last().unwrap();
    announce(
        json!({"event": "final", "run_id": run.id, "state": run.state,
        "active_elapsed_ms": run.active_elapsed_ms, "reason": run.reason, "checks": run.checks,
        "unexpected_errors": run.statistics.unexpected_errors, "report": store.path}),
    )?;
    if run.state == "incomplete" {
        Ok(2)
    } else if run.checks.qualification_passed {
        Ok(0)
    } else {
        Ok(1)
    }
}

async fn measure(
    statistics: &SharedStatistics,
    name: &str,
    future: impl std::future::Future<Output = Check<Evidence>>,
) -> bool {
    statistics.lock().unwrap().start(name);
    let started = Instant::now();
    let result = tokio::time::timeout(CASE_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| {
            Err("Case exceeded its 10 s bound (not an expected cancellation).".into())
        });
    statistics
        .lock()
        .unwrap()
        .finish(name, started.elapsed(), result)
}

fn sample(run: &mut Run, statistics: &SharedStatistics) {
    match platform::sample(run.active_elapsed_ms) {
        Ok(value) => {
            if value.private_bytes > platform::PRIVATE_BYTES_LIMIT
                || value.handles > platform::HANDLES_LIMIT
            {
                statistics
                    .lock()
                    .unwrap()
                    .error("Native private-byte or handle safety ceiling exceeded.");
            }
            run.resources.push(value);
        }
        Err(error) => statistics.lock().unwrap().error(error),
    }
}

fn refresh(
    store: &Store,
    document: &mut Document,
    statistics: &SharedStatistics,
    fixture: Option<&Fixture>,
    clock: Option<&ActiveClock>,
    take_sample: bool,
) -> Check<()> {
    let run = document.runs.last_mut().unwrap();
    if let Some(clock) = clock {
        clock.update(run);
    }
    if let Some(fixture) = fixture {
        run.upstream = fixture.snapshot();
    }
    if take_sample {
        sample(run, statistics);
    }
    run.statistics = statistics.lock().unwrap().clone();
    store.save(document)
}

fn require(condition: bool, message: &str) -> Check<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn announce(value: serde_json::Value) -> Check<()> {
    let mut output = std::io::stdout().lock();
    writeln!(output, "{value}")
        .and_then(|()| output.flush())
        .map_err(|error| format!("Bounded control output failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("target")
            .join("native-soak-tests")
            .join(report::unique_id())
            .join("report.json")
    }

    #[test]
    fn cli_rejects_unbounded_or_ambiguous_inputs() {
        for arguments in [
            vec!["--report", "x", "--duration-secs", "0"],
            vec!["--report", "x", "--duration-secs", "259201"],
            vec!["--report", "x", "--concurrency", "17"],
            vec!["--report", "x", "--sample-secs", "0"],
            vec!["--report", "x", "--resume", "--resume"],
            vec!["--report", "x", "--upstream", "https://example.invalid"],
        ] {
            assert!(Arguments::parse(arguments.into_iter().map(str::to_string)).is_err());
        }
        let default = Arguments::parse(["--report", "x"].into_iter().map(str::to_string)).unwrap();
        assert_eq!(default.config.duration_secs, 60);
    }

    #[test]
    fn observation_gaps_are_excluded_and_cannot_pass_as_active_runtime() {
        let mut clock = ActiveClock::default();
        assert!(clock.observe(Duration::from_millis(200)));
        assert!(!clock.observe(Duration::from_secs(72 * 3600)));
        assert_eq!(clock.active, Duration::from_millis(200));
        let mut run = Run::new(json!({}));
        clock.update(&mut run);
        run.finish(
            &Config {
                duration_secs: 259200,
                ..Config::default()
            },
            "duration_elapsed",
        );
        assert!(!run.checks.duration_met);
        assert!(!run.checks.uninterrupted);
        assert!(!run.checks.qualified_72h);
        let mut paused_timer = ActiveClock::default();
        assert!(paused_timer.observe_with_wall(Duration::ZERO, 1000));
        assert!(!paused_timer.observe_with_wall(Duration::from_millis(100), 72 * 3600 * 1000));
        assert_eq!(paused_timer.active, Duration::ZERO);
        assert_eq!(paused_timer.wall_discontinuities, 1);
    }

    #[test]
    fn exclusive_resume_preserves_interruption_and_starts_from_zero() {
        let path = path();
        let config = Config {
            duration_secs: 259200,
            ..Config::default()
        };
        let (store, mut document) = Store::open(&path, &config, false).unwrap();
        let mut run = Run::new(json!({}));
        run.state = "running".into();
        run.active_elapsed_ms = 1234;
        run.checkpoint_unix_ms = 1;
        document.runs.push(run);
        let abandoned_cache = store
            .directory
            .join(format!("native-soak-models-{}.json", document.runs[0].id));
        std::fs::write(&abandoned_cache, b"synthetic").unwrap();
        store.save(&mut document).unwrap();
        assert!(Store::open(&path, &config, true).is_err());
        drop(store);
        assert!(Store::open(&path, &Config::default(), true).is_err());
        let (store, mut resumed) = Store::open(&path, &config, true).unwrap();
        assert!(!abandoned_cache.exists());
        assert_eq!(resumed.runs[0].state, "interrupted");
        assert_eq!(resumed.runs[0].active_elapsed_ms, 1234);
        assert!(resumed.runs[0].finished_unix_ms.is_none());
        resumed.runs.push(Run::new(json!({})));
        assert_eq!(resumed.runs[1].active_elapsed_ms, 0);
        assert!(!resumed.runs[0].checks.qualified_72h);
        store.save(&mut resumed).unwrap();
        assert!(Store::open(&path, &config, false).is_err());
        let directory = store.directory.clone();
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn completion_gates_require_one_full_clean_native_attempt() {
        let config = Config {
            duration_secs: 259200,
            ..Config::default()
        };
        let mut run = Run::new(json!({
            "process_architecture": std::env::consts::ARCH, "debug_assertions": false,
            "platform": {"os": "windows", "emulated": false, "native_architecture": std::env::consts::ARCH},
            "build": {
                "schema_version": 1, "profile": "release",
                "target": format!("{}-pc-windows-msvc", std::env::consts::ARCH),
                "source_sha256": "0".repeat(64), "source_head": "0".repeat(40),
                "rustc_verbose": "test fixture", "msvc_toolset": "test fixture",
                "windows_sdk": "test fixture", "visual_studio": "test fixture",
            },
        }));
        run.active_elapsed_ms = 259_200_000;
        for case in REQUIRED_CASES {
            run.statistics.cases.insert(
                (*case).into(),
                report::CaseCounts {
                    started: 1,
                    passed: 1,
                    ..report::CaseCounts::default()
                },
            );
        }
        run.upstream = json!({"posts": 0, "unexpected_requests": 0, "duplicate_requests": 0});
        run.resources.sample_count = 2;
        run.drain = report::Drain {
            workers_joined: true,
            active_request_cancelled: true,
            runtime_joined: true,
            upstream_joined: true,
            listeners_released: true,
            synthetic_data_removed: true,
            ..report::Drain::default()
        };
        run.finish(&config, "duration_elapsed");
        assert!(run.checks.qualified_72h);
        let good = run.clone();
        for mutation in 0..8 {
            run = good.clone();
            match mutation {
                0 => run.active_elapsed_ms = 60_000,
                1 => run.drain.workers_joined = false,
                2 => run.statistics.error("unexplained failure"),
                3 => {
                    run.statistics.cases.remove("json");
                }
                4 => run.upstream["posts"] = json!(1),
                5 => run.identity["platform"]["emulated"] = json!(true),
                6 => run.statistics.active_cases = 1,
                _ => run.wall_clock_discontinuities = 1,
            }
            run.finish(&config, "duration_elapsed");
            assert!(!run.checks.qualified_72h, "mutation {mutation}");
        }
        run = good;
        run.finish(&config, "control_input");
        assert_eq!(run.state, "incomplete");
        assert!(!run.checks.qualified_72h);
    }

    #[test]
    fn reports_bound_errors_samples_and_reject_paths_outside_the_project() {
        let mut statistics = Statistics::default();
        for _ in 0..100 {
            statistics.error("x".repeat(1024));
        }
        assert_eq!(statistics.unexpected_errors, 100);
        assert_eq!(statistics.errors.len(), 32);
        assert_eq!(statistics.errors_omitted, 68);
        assert_eq!(statistics.errors[0].len(), 512);
        let mut resources = report::Resources::default();
        for index in 0..300 {
            resources.push(platform::ResourceSample {
                active_elapsed_ms: index,
                private_bytes: index,
                working_set_bytes: index,
                handles: index,
                cpu_ms: index,
            });
        }
        assert_eq!(resources.samples.len(), 256);
        assert_eq!(resources.samples_omitted, 44);
        assert_eq!(resources.peak_private_bytes, 299);
        assert!(
            Store::open(
                &PathBuf::from("..").join("outside").join("report.json"),
                &Config::default(),
                false
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_fixture_covers_contracts_cancellation_and_drain() {
        let path = path();
        let config = Config {
            duration_secs: 1,
            concurrency: 2,
            sample_secs: 1,
        };
        let (store, _) = Store::open(&path, &config, false).unwrap();
        let statistics = Arc::new(Mutex::new(Statistics::default()));
        let fixture = Fixture::start(&config, &store.directory, &report::unique_id(), statistics)
            .await
            .unwrap();
        fixture.admission().await.unwrap();
        fixture.health().await.unwrap();
        for (index, case) in WORKLOAD.iter().enumerate() {
            fixture
                .case(case, &format!("test-{index}"))
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        }
        let (drain, result) = fixture.stop(true).await;
        result.unwrap();
        assert!(drain.runtime_joined && drain.upstream_joined && drain.listeners_released);
        assert_eq!(drain.remaining_tickets, 0);
        assert_eq!(fixture.snapshot()["unexpected_requests"], 0);
        assert_eq!(fixture.snapshot()["duplicate_requests"], 0);
        drop(fixture);
        let directory = store.directory.clone();
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
